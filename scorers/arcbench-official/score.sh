#!/usr/bin/env bash
# ARC-Bench official-format scorer: score one ARC-Bench template submission
# (zip with frontend/ + backend/ at the root, no Dockerfile) the way the
# official ARC-Bench runner does, and write result.json (result v2).
#
#   score.sh --artifact app.zip --tests DIR --out result.json
#            [--visibility public|hidden] [--artifacts DIR]
#            [--task-id ID] [--submission-id ID]
#            [--build-timeout 1200] [--ready-timeout 180] [--run-timeout 1800]
#
# Install, build, start, readiness, Playwright config and result parsing are
# the official runner's own functions (/opt/arcbench/run_submission.py in
# the pinned image), called by image/official.py. See
# docs/arcbench-official.md for the parameters and where each comes from.
#
# Containers (all from the scorer image, names unique per run):
#   build  npm install / npm run build. Its only network is an --internal
#          one whose sole exit is an HTTPS CONNECT proxy that admits the
#          npm registries in ALLOW_HOSTS. No tests mounted.
#   serve  `npm run start`, --network none (loopback only). No tests.
#   test   Playwright, joins the serve container's network namespace, so it
#          reaches the app at 127.0.0.1:3000 exactly as the official runner
#          does, and nothing else. Non-root, cap-drop ALL.
# The submission lives in a per-run docker volume (removed at the end).
#
# Exit status: 0 = result.json written (whatever the score), 2 = usage
# error, 1 = could not write result.json at all.
# Environment (optional): CRUCIBLE_SCORER_IMAGE (prebuilt image; default
# builds ./image), CRUCIBLE_SCORER_FIREWALL=1 (iptables walls around the
# build network, as in scorers/playwright).
set -uo pipefail

main() {

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# What `npm install` needs, nothing else (docs/arcbench-official.md §3):
# - npm registries: the image's /root/.npmrc registry, and the two registry
#   hosts that appear in submitted lockfiles (`replace-registry-host=npmjs`
#   rewrites only registry.npmjs.org); registry.npmmirror.com answers
#   tarball requests with a redirect to cdn.npmmirror.com;
# - native addons (sqlite3 5.x via prebuild-install, bcrypt 5.x via
#   node-pre-gyp): prebuilt binaries from GitHub releases, and node-gyp's
#   fallback (Node headers from nodejs.org) when no prebuilt matches.
ALLOW_HOSTS="repo.huaweicloud.com,registry.npmjs.org,registry.npmmirror.com,cdn.npmmirror.com"
ALLOW_HOSTS="$ALLOW_HOSTS,github.com,objects.githubusercontent.com,release-assets.githubusercontent.com,nodejs.org"

usage() { sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }
die_usage() { echo "score.sh: $*" >&2; exit 2; }

ARTIFACT="" TESTS="" OUT="" VISIBILITY="public" ARTIFACTS_OUT="" TASK_ID="" SUBMISSION_ID=""
BUILD_TIMEOUT_S=1200 READY_TIMEOUT_S=180 RUN_TIMEOUT_S=1800
while [ $# -gt 0 ]; do
  case "$1" in
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --tests) TESTS="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --visibility) VISIBILITY="${2:-}"; shift 2 ;;
    --artifacts) ARTIFACTS_OUT="${2:-}"; shift 2 ;;
    --task-id) TASK_ID="${2:-}"; shift 2 ;;
    --submission-id) SUBMISSION_ID="${2:-}"; shift 2 ;;
    --build-timeout) BUILD_TIMEOUT_S="${2:-}"; shift 2 ;;
    --ready-timeout) READY_TIMEOUT_S="${2:-}"; shift 2 ;;
    --run-timeout) RUN_TIMEOUT_S="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
if [ -z "$ARTIFACT" ] || [ -z "$TESTS" ] || [ -z "$OUT" ]; then usage; fi
[ -f "$ARTIFACT" ] || die_usage "--artifact: no such file: $ARTIFACT"
[ -d "$TESTS" ] || die_usage "--tests: no such directory: $TESTS"
case "$VISIBILITY" in public|hidden) ;; *) die_usage "--visibility must be public or hidden" ;; esac
for n in "$BUILD_TIMEOUT_S" "$READY_TIMEOUT_S" "$RUN_TIMEOUT_S"; do
  case "$n" in ''|*[!0-9]*) die_usage "timeouts must be positive integers" ;; esac
done
for v in "$TASK_ID" "$SUBMISSION_ID"; do
  case "$v" in *[!A-Za-z0-9._-]*) die_usage "--task-id/--submission-id: only [A-Za-z0-9._-]" ;; esac
done
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
[ -n "$TIMEOUT_BIN" ] || die_usage "needs \`timeout\` (GNU coreutils) on PATH"
command -v docker >/dev/null || die_usage "needs docker on PATH"

abspath() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")"); }
ARTIFACT="$(abspath "$ARTIFACT")"
TESTS="$(cd "$TESTS" && pwd -P)"
mkdir -p "$(dirname "$OUT")" || exit 1
OUT="$(abspath "$OUT")"
if [ -n "$ARTIFACTS_OUT" ]; then mkdir -p "$ARTIFACTS_OUT" || exit 1; ARTIFACTS_OUT="$(cd "$ARTIFACTS_OUT" && pwd -P)"; fi

RUN_ID="$(date +%s)$$${RANDOM}"
LABEL="crucible.scorer.run=$RUN_ID"
VOL="crucible-ws-$RUN_ID"
NET="crucible-bnet-$RUN_ID"
BRIDGE="crb$(printf '%s' "$RUN_ID" | cksum | cut -d' ' -f1)"
PROXY_CTR="crucible-proxy-$RUN_ID"
BUILD_CTR="crucible-build-$RUN_ID"
SERVE_CTR="crucible-serve-$RUN_ID"
TEST_CTR="crucible-test-$RUN_ID"
FIREWALL_UP=0

WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-score.XXXXXX")" || exit 1
WORK="$(cd "$WORK" && pwd -P)"
RESULTS="$WORK/results"
mkdir -p "$RESULTS"
# The test container runs as the caller's uid (it must read --tests, which
# the caller may have made private), root callers fall back to 1000.
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; chmod 0777 "$RESULTS"; else RUN_AS="$(id -u):$(id -g)"; fi
# Root without capabilities cannot read a private artifact: give the unpack
# container a world-readable copy.
cp "$ARTIFACT" "$WORK/app.zip" && chmod 0644 "$WORK/app.zip" || exit 1

# shellcheck disable=SC2317,SC2329 # invoked via trap
cleanup() {
  docker rm -f "$TEST_CTR" "$SERVE_CTR" "$BUILD_CTR" "$PROXY_CTR" >/dev/null 2>&1 || true
  [ "$FIREWALL_UP" = 1 ] && firewall -D >/dev/null 2>&1
  FIREWALL_UP=0
  docker network rm "$NET" >/dev/null 2>&1 || true
  docker volume rm -f "$VOL" >/dev/null 2>&1 || true
  rm -rf "$WORK" 2>/dev/null || true
}
# shellcheck disable=SC2317,SC2329 # also invoked from cleanup
firewall() { # -I | -D: the build network may reach neither the host nor anything outside it
  local rc=0
  sudo -n iptables "$1" INPUT -i "$BRIDGE" -j DROP || rc=1
  sudo -n ip6tables "$1" INPUT -i "$BRIDGE" -j DROP || rc=1
  sudo -n iptables "$1" DOCKER-USER -i "$BRIDGE" ! -o "$BRIDGE" -j DROP || rc=1
  return "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
log() { echo "[score] $*" >&2; }

STATUS="" DETAIL=""
set_outcome() { STATUS="$1"; DETAIL="$2"; }   # scored | zero | system_error

# Common hardening; the official runner runs everything as root in one
# container, here build and serve run as root (npm reads /root/.npmrc)
# without capabilities.
HARDEN=(--label "$LABEL" --platform linux/amd64 --security-opt no-new-privileges)

# 0. Scorer image.
if [ -n "${CRUCIBLE_SCORER_IMAGE:-}" ]; then
  IMAGE="$CRUCIBLE_SCORER_IMAGE"
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    n=0; until docker pull -q "$IMAGE" >/dev/null 2>&1; do n=$((n + 1)); [ "$n" -ge 3 ] && break; sleep 5; done
  fi
  docker image inspect "$IMAGE" >/dev/null 2>&1 || set_outcome system_error "scorer image unavailable"
else
  IMAGE="crucible-scorer-arcbench-official:local"
  docker build -q --platform linux/amd64 -t "$IMAGE" "$HERE/image" >"$RESULTS/scorer-build.log" 2>&1 \
    || set_outcome system_error "scorer image failed to build"
fi

# 1. Unpack into the workspace volume (/workspace/template, the official
# PROJECT_DIR). A bad zip is the submission's fault.
if [ -z "$STATUS" ]; then
  if ! docker volume create --label "$LABEL" "$VOL" >/dev/null 2>"$RESULTS/volume.log"; then
    set_outcome system_error "could not create the workspace volume"
  else
    docker run --rm "${HARDEN[@]}" --network none --cap-drop ALL \
      -v "$WORK/app.zip:/in/app.zip:ro" -v "$VOL:/workspace" --entrypoint python3 "$IMAGE" \
      /opt/crucible/official.py unpack /in/app.zip /workspace/template >"$RESULTS/unpack.log" 2>&1
    rc=$?
    if [ "$rc" -eq 1 ]; then set_outcome zero "zip failed validation (bad paths, links, or too large)"
    elif [ "$rc" -ne 0 ]; then set_outcome system_error "zip extraction exited $rc"; fi
    [ "$rc" -ne 0 ] && sed 's/^/[unpack] /' "$RESULTS/unpack.log" | tail -5 >&2
  fi
fi

# 2. Build: official npm install / npm run build; egress only to the npm
# registries through the proxy.
if [ -z "$STATUS" ]; then
  if ! docker network create --driver bridge --internal --label "$LABEL" \
      -o com.docker.network.bridge.name="$BRIDGE" "$NET" >/dev/null 2>"$RESULTS/net.log"; then
    set_outcome system_error "could not create the build network"
  elif [ "${CRUCIBLE_SCORER_FIREWALL:-0}" = 1 ] && ! { FIREWALL_UP=1; firewall -I >>"$RESULTS/net.log" 2>&1; }; then
    set_outcome system_error "could not install the build firewall"
  elif ! docker run -d --name "$PROXY_CTR" "${HARDEN[@]}" --cap-drop ALL --user 65534:65534 \
      --read-only -e "ALLOW_HOSTS=$ALLOW_HOSTS" --memory 256m --pids-limit 256 \
      --entrypoint python3 "$IMAGE" /opt/crucible/egress_proxy.py >/dev/null 2>>"$RESULTS/net.log" \
    || ! docker network connect --alias egress "$NET" "$PROXY_CTR" >>"$RESULTS/net.log" 2>&1; then
    set_outcome system_error "could not start the egress proxy"
  fi
fi
if [ -z "$STATUS" ]; then
  log "building (timeout ${BUILD_TIMEOUT_S}s)"
  P="http://egress:3128"
  rc=0
  "$TIMEOUT_BIN" --kill-after=10 "$BUILD_TIMEOUT_S" docker run --name "$BUILD_CTR" "${HARDEN[@]}" \
    --network "$NET" --cap-drop ALL --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER \
    --memory 4g --memory-swap 4g --cpus 2 --pids-limit 1024 \
    -e "HTTPS_PROXY=$P" -e "HTTP_PROXY=$P" -e "https_proxy=$P" -e "http_proxy=$P" \
    -e "npm_config_proxy=$P" -e "npm_config_https_proxy=$P" -e "NO_PROXY=localhost,127.0.0.1" \
    -v "$VOL:/workspace" --entrypoint python3 "$IMAGE" /opt/crucible/official.py build \
    >"$RESULTS/build.log" 2>&1 || rc=$?
  docker rm -f "$BUILD_CTR" >/dev/null 2>&1 || true
  docker logs "$PROXY_CTR" >"$RESULTS/egress.log" 2>&1 || true
  denied=$(grep -c '^DENY' "$RESULTS/egress.log" || true)
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    set_outcome zero "app build exceeded ${BUILD_TIMEOUT_S}s"
  elif [ "$rc" -ne 0 ]; then
    if grep -q '^UPSTREAM_FAIL' "$RESULTS/egress.log"; then
      set_outcome system_error "npm registry unreachable during the build"
    else
      step=$(sed -n 's/^\[crucible\] build failed at: \([A-Za-z ]*[A-Za-z]\).*/\1/p' "$RESULTS/build.log" | tail -1)
      set_outcome zero "app build failed${step:+ ($step)}"
      [ "${denied:-0}" != 0 ] && DETAIL="$DETAIL; egress proxy refused $denied request(s)"
    fi
  fi
  docker rm -f "$PROXY_CTR" >/dev/null 2>&1 || true
fi

# 3. Serve: official `npm run start`, no network but loopback.
if [ -z "$STATUS" ]; then
  if ! docker run -d --name "$SERVE_CTR" "${HARDEN[@]}" --network none \
      --cap-drop ALL --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER \
      --memory 2g --memory-swap 2g --cpus 1 --pids-limit 512 \
      -v "$VOL:/workspace" --entrypoint python3 "$IMAGE" /opt/crucible/official.py serve \
      >/dev/null 2>"$RESULTS/serve-start.log"; then
    set_outcome system_error "could not start the app container"
  else
    t=0
    while :; do
      if docker logs "$SERVE_CTR" 2>/dev/null | grep -q '^\[crucible\] app ready'; then break; fi
      if docker logs "$SERVE_CTR" 2>/dev/null | grep -q '^\[crucible\] app not ready' \
         || [ "$(docker inspect -f '{{.State.Running}}' "$SERVE_CTR" 2>/dev/null)" != "true" ]; then
        set_outcome zero "app did not become ready within 120s"; break
      fi
      t=$((t + 1)); [ "$t" -ge "$READY_TIMEOUT_S" ] && { set_outcome zero "app did not become ready"; break; }
      sleep 1
    done
  fi
fi

# 4. Tests: official Playwright run against 127.0.0.1:3000 in the app's
# network namespace. Tests are copied from the read-only mount into a tmpfs.
if [ -z "$STATUS" ]; then
  log "running tests (timeout ${RUN_TIMEOUT_S}s)"
  rc=0
  "$TIMEOUT_BIN" --kill-after=10 "$RUN_TIMEOUT_S" docker run --name "$TEST_CTR" "${HARDEN[@]}" \
    --network "container:$SERVE_CTR" --user "$RUN_AS" --cap-drop ALL \
    --memory 2g --memory-swap 2g --cpus 2 --pids-limit 1024 --shm-size 1g \
    --mount type=tmpfs,destination=/workspace,tmpfs-mode=1777 -e HOME=/tmp \
    -v "$TESTS:/pack:ro" -v "$RESULTS:/results" --entrypoint python3 "$IMAGE" \
    /opt/crucible/official.py test /pack /results >"$RESULTS/test.log" 2>&1 || rc=$?
  docker rm -f "$TEST_CTR" >/dev/null 2>&1 || true
  docker logs --tail 5000 "$SERVE_CTR" >"$RESULTS/app.log" 2>&1 || true
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then set_outcome zero "test run exceeded ${RUN_TIMEOUT_S}s"
  elif [ "$rc" -eq 0 ] && [ -f "$RESULTS/official.json" ]; then set_outcome scored ""
  else set_outcome system_error "test runner exited $rc without official results"; fi
fi

# 5. result.json, whatever happened above.
if docker image inspect "$IMAGE" >/dev/null 2>&1 && docker run --rm "${HARDEN[@]}" --network none \
    --cap-drop ALL --user "$RUN_AS" -v "$RESULTS:/results" --entrypoint python3 "$IMAGE" \
    /opt/crucible/official.py result /results/official.json "$STATUS" "$DETAIL" "$VISIBILITY" \
    /results/result.json "$TASK_ID" "$SUBMISSION_ID" >"$RESULTS/result.log" 2>&1 \
    && [ -f "$RESULTS/result.json" ]; then
  cp "$RESULTS/result.json" "$OUT" || exit 1
else
  printf '{"schema": 2, "status": "error", "error": "system", "visibility": "%s", "detail": "result writer failed"}\n' \
    "$VISIBILITY" >"$OUT" || exit 1
fi

if [ -n "$ARTIFACTS_OUT" ]; then
  for f in unpack.log build.log egress.log app.log test.log official.json playwright-report.json; do
    [ -f "$RESULTS/$f" ] && cp "$RESULTS/$f" "$ARTIFACTS_OUT/"
  done
fi
log "$(tr -d '\n' <"$OUT" | sed 's/  */ /g' | cut -c1-300)"
exit 0
}

main "$@"
