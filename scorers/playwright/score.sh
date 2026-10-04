#!/usr/bin/env bash
# Playwright scorer: score one web-app artifact against one Playwright test
# pack and write result.json (crucible-core ScoreResult).
#
#   score.sh --artifact app.zip --tests DIR --out result.json
#            [--visibility public|hidden] [--artifacts DIR]
#            [--task-id ID] [--submission-id ID] [--app-port 3000]
#            [--build-timeout 600] [--ready-timeout 60] [--run-timeout 900]
#
# Contract: docs/scorer-contract.md. Behaviour is a port of the prototype
# grader's scripts/grade.sh (arcbench-grader): zip safety check, app built
# with --network=none, app run on an --internal network with 512 MB / 1 CPU /
# 256 pids, Playwright 1.57.0 + Chromium in a separate container that only
# sees the tests and the app's URL; retries=0, workers=2, 60 s per test.
#
# Exit status: 0 = result.json written (whatever the score), 2 = usage
# error, 1 = could not write result.json at all.
#
# Host requirements: bash (3.2+), crucible (`$CRUCIBLE`, default on PATH),
# timeout (coreutils). Containers are started with `crucible ctr`, i.e. by
# the step's execution backend (Docker by default, docs/executors.md §2.3).
# Environment (optional):
#   CRUCIBLE_SCORER_IMAGE        prebuilt scorer image (default: build ./image)
#   CRUCIBLE_PRUNE_BUILD_CACHE   1 = clear the build cache after the run
#                                (for throwaway CI machines; off by default
#                                because it wipes the whole daemon's cache)
#   CRUCIBLE_SCORER_FIREWALL     1 = (Docker backend) `ctr net create` also
#                                drops, with `sudo iptables`, everything the
#                                scoring network sends to the host (INPUT, v4
#                                and v6) or out of the network (DOCKER-USER);
#                                failing to install the rules is system_error.
#                                For CI machines (Linux, passwordless sudo).
set -uo pipefail

# Everything runs inside main() so bash parses the whole file before
# executing any of it (safe against the file being replaced mid-run).
main() {

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
  exit 2
}
die_usage() { echo "score.sh: $*" >&2; exit 2; }

ARTIFACT="" TESTS="" OUT="" VISIBILITY="public" ARTIFACTS_OUT="" TASK_ID="" SUBMISSION_ID=""
APP_PORT=3000 BUILD_TIMEOUT_S=600 READY_TIMEOUT_S=60 RUN_TIMEOUT_S=900
while [ $# -gt 0 ]; do
  case "$1" in
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --tests) TESTS="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --visibility) VISIBILITY="${2:-}"; shift 2 ;;
    --artifacts) ARTIFACTS_OUT="${2:-}"; shift 2 ;;
    --task-id) TASK_ID="${2:-}"; shift 2 ;;
    --submission-id) SUBMISSION_ID="${2:-}"; shift 2 ;;
    --app-port) APP_PORT="${2:-}"; shift 2 ;;
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
for n in "$APP_PORT" "$BUILD_TIMEOUT_S" "$READY_TIMEOUT_S" "$RUN_TIMEOUT_S"; do
  case "$n" in ''|*[!0-9]*) die_usage "port/timeouts must be positive integers" ;; esac
done
for v in "$TASK_ID" "$SUBMISSION_ID"; do
  case "$v" in *[!A-Za-z0-9._-]*) die_usage "--task-id/--submission-id: only [A-Za-z0-9._-]" ;; esac
done

TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
[ -n "$TIMEOUT_BIN" ] || die_usage "needs \`timeout\` (GNU coreutils) on PATH"
CRUCIBLE="${CRUCIBLE:-crucible}"
command -v "$CRUCIBLE" >/dev/null || die_usage "needs crucible on PATH (or \$CRUCIBLE)"
ctr() { "$CRUCIBLE" ctr "$@"; }

abspath() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")"); }
ARTIFACT="$(abspath "$ARTIFACT")"
TESTS="$(cd "$TESTS" && pwd -P)"
mkdir -p "$(dirname "$OUT")" || exit 1
OUT="$(abspath "$OUT")"
if [ -n "$ARTIFACTS_OUT" ]; then mkdir -p "$ARTIFACTS_OUT" || exit 1; ARTIFACTS_OUT="$(cd "$ARTIFACTS_OUT" && pwd -P)"; fi

# Per-run names: several scorers may share one docker daemon.
RUN_ID="$(date +%s)$$${RANDOM}"
APP_IMAGE="crucible-app-$RUN_ID"
NET="crucible-net-$RUN_ID"
APP_CTR="crucible-app-$RUN_ID"
RUNNER_CTR="crucible-runner-$RUN_ID"
LABEL="crucible.scorer.run=$RUN_ID"

# Containers write into bind mounts as the caller's uid so cleanup can
# delete everything; root callers fall back to the image's `node` user
# (Chromium refuses to sandbox as root).
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; else RUN_AS="$(id -u):$(id -g)"; fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-score.XXXXXX")" || exit 1
WORK="$(cd "$WORK" && pwd -P)"
RESULTS="$WORK/results"
APP_SRC="$WORK/app_src"
mkdir -p "$RESULTS" "$APP_SRC"
[ "$RUN_AS" = "1000:1000" ] && chmod -R a+rwX "$WORK"

# shellcheck disable=SC2317,SC2329 # invoked via trap
cleanup() {
  ctr rm -f "$RUNNER_CTR" "$APP_CTR" >/dev/null 2>&1 || true
  ctr net rm "$NET" >/dev/null 2>&1 || true
  ctr image rm "$APP_IMAGE" >/dev/null 2>&1 || true
  if [ "${CRUCIBLE_PRUNE_BUILD_CACHE:-0}" = "1" ]; then
    ctr prune-build-cache >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

log() { echo "[score] $*" >&2; }

STATUS="" DETAIL=""
set_outcome() { STATUS="$1"; DETAIL="$2"; }

# 0. Scorer image.
if [ -n "${CRUCIBLE_SCORER_IMAGE:-}" ]; then
  IMAGE="$CRUCIBLE_SCORER_IMAGE"
  if ! ctr image exists "$IMAGE" >/dev/null 2>&1; then
    n=0
    until ctr image pull -q "$IMAGE" >/dev/null 2>&1; do
      n=$((n + 1)); [ "$n" -ge 3 ] && break; sleep 5
    done
  fi
  ctr image exists "$IMAGE" >/dev/null 2>&1 || set_outcome system_error "scorer image unavailable: $IMAGE"
else
  IMAGE="crucible-scorer-playwright:local"
  ctr build -q -t "$IMAGE" "$HERE/image" >"$RESULTS/scorer-build.log" 2>&1 \
    || set_outcome system_error "scorer image failed to build"
fi

in_image() {
  # Helper commands (no network, read-only root, caller's uid).
  ctr run --rm --network none --read-only --tmpfs /tmp:rw,size=64m \
    --security-opt no-new-privileges --cap-drop ALL --user "$RUN_AS" \
    --entrypoint node --label "$LABEL" "$@"
}

# 1. Zip safety check + extraction. A malformed zip or a missing root
# Dockerfile is the agent's fault: failed, not system_error.
if [ -z "$STATUS" ]; then
  in_image -v "$ARTIFACT:/in/app.zip:ro" -v "$APP_SRC:/out" "$IMAGE" \
    /opt/scorer/src/cli.ts unpack /in/app.zip /out >"$RESULTS/unpack.log" 2>&1
  rc=$?
  if [ "$rc" -eq 1 ]; then
    set_outcome failed "zip failed validation (bad paths, too large, or too many files)"
  elif [ "$rc" -ne 0 ]; then
    set_outcome system_error "zip extraction helper exited $rc"
  elif [ ! -f "$APP_SRC/Dockerfile" ]; then
    set_outcome failed "no Dockerfile at the root of the submitted app"
  fi
fi

# 2. Build the app: no network, resource-capped, killed on timeout (the
# Docker backend uses BuildKit whenever buildx is installed: it cancels the
# daemon-side build when the client dies, the legacy builder does not).
if [ -z "$STATUS" ]; then
  log "building app (timeout ${BUILD_TIMEOUT_S}s)"
  rc=0
  "$TIMEOUT_BIN" --kill-after=10 --signal=TERM "$BUILD_TIMEOUT_S" \
    "$CRUCIBLE" ctr build --network=none \
    --memory=2g --memory-swap=2g --cpu-quota=200000 --cpu-period=100000 \
    --label "$LABEL" -t "$APP_IMAGE" "$APP_SRC" >"$RESULTS/build.log" 2>&1 || rc=$?
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    set_outcome failed "app build exceeded ${BUILD_TIMEOUT_S}s (terminated)"
  elif [ "$rc" -ne 0 ]; then
    set_outcome failed "app build failed"
  fi
fi

# 3. App container on a fresh internal (no internet) network.
if [ -z "$STATUS" ]; then
  if ! ctr net create --internal --label "$LABEL" "$NET" >/dev/null 2>"$RESULTS/net.log"; then
    set_outcome system_error "could not create the scoring network"
  elif ! ctr run -d --name "$APP_CTR" --network "$NET" --network-alias app \
      --label "$LABEL" \
      -e "PORT=$APP_PORT" \
      --memory=512m --memory-swap=512m --cpus=1.0 --pids-limit=256 \
      --tmpfs /tmp:rw,noexec,size=64m \
      --security-opt no-new-privileges \
      --cap-drop NET_RAW --cap-drop MKNOD --cap-drop SYS_CHROOT --cap-drop AUDIT_WRITE --cap-drop SETFCAP \
      --log-opt max-size=10m --log-opt max-file=1 \
      "$APP_IMAGE" >/dev/null 2>"$RESULTS/app-start.log"; then
    set_outcome failed "app container failed to start"
  fi
fi

# 4. Playwright runner: sees only the tests (read-only) and the app's URL.
# The tests may be untrusted (uploaded tasksets): no environment beyond the
# three variables below, non-root, no capabilities, no-new-privileges,
# Docker's default seccomp profile, on the internal network only (plus the
# firewall above on CI). The container is the boundary: test code runs in it
# directly, so Chromium's own sandbox (which needs user namespaces, i.e.
# capabilities and a looser seccomp profile) would add nothing and is off.
# Retried once if the harness itself died without a report (scorer fault);
# never retried once the app was ready and tests ran or timed out.
if [ -z "$STATUS" ]; then
  log "running tests (ready ${READY_TIMEOUT_S}s, run ${RUN_TIMEOUT_S}s)"
  attempts=0
  while :; do
    attempts=$((attempts + 1))
    rm -f "$RESULTS/report.json"
    ctr rm -f "$RUNNER_CTR" >/dev/null 2>&1 || true
    "$TIMEOUT_BIN" "$RUN_TIMEOUT_S" "$CRUCIBLE" ctr run --name "$RUNNER_CTR" --network "$NET" \
      --label "$LABEL" --user "$RUN_AS" \
      -e "BASE_URL=http://app:$APP_PORT" \
      -e "READY_TIMEOUT=$READY_TIMEOUT_S" \
      -e "CHROMIUM_SANDBOX=0" \
      --memory=2g --cpus=2.0 --pids-limit=1024 \
      --cap-drop ALL --security-opt no-new-privileges \
      --shm-size=1g \
      -v "$TESTS:/pack:ro" \
      -v "$RESULTS:/results" \
      "$IMAGE" >"$RESULTS/runner.log" 2>&1
    rc=$?
    # `timeout` kills the client, not the container: stop it explicitly.
    ctr rm -f "$RUNNER_CTR" >/dev/null 2>&1 || true
    if [ "$rc" -eq 124 ] || [ "$rc" -eq 3 ] || [ -f "$RESULTS/report.json" ]; then break; fi
    [ "$attempts" -ge 2 ] && break
    log "runner exited $rc with no report (attempt $attempts/2), retrying"
  done
  ctr logs --tail 5000 "$APP_CTR" >"$RESULTS/app.log" 2>&1 || true
  if [ "$rc" -eq 124 ]; then
    set_outcome failed "test run exceeded ${RUN_TIMEOUT_S}s"
  elif [ "$rc" -eq 3 ]; then
    set_outcome failed "app did not become ready within ${READY_TIMEOUT_S}s"
  elif [ ! -f "$RESULTS/report.json" ]; then
    set_outcome system_error "runner exited $rc without a report after $attempts attempt(s)"
  else
    set_outcome scored ""
  fi
fi

# 5. result.json, whatever happened above.
json_str() { printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"; }
write_fallback() {
  {
    printf '{\n'
    [ -n "$SUBMISSION_ID" ] && printf '  "submission_id": %s,\n' "$(json_str "$SUBMISSION_ID")"
    [ -n "$TASK_ID" ] && printf '  "task_id": %s,\n' "$(json_str "$TASK_ID")"
    printf '  "visibility": %s,\n  "status": "system_error",\n  "passed": 0,\n  "total": 0,\n  "detail": %s\n}' \
      "$(json_str "$VISIBILITY")" "$(json_str "$1")"
  } >"$OUT"
}

ID_ARGS=()
[ -n "$TASK_ID" ] && ID_ARGS+=(--task-id "$TASK_ID")
[ -n "$SUBMISSION_ID" ] && ID_ARGS+=(--submission-id "$SUBMISSION_ID")
if [ "$STATUS" = "system_error" ] && ! ctr image exists "$IMAGE" >/dev/null 2>&1; then
  write_fallback "$DETAIL"
elif in_image -v "$RESULTS:/results" "$IMAGE" /opt/scorer/src/cli.ts result \
    --status "$STATUS" --detail "$DETAIL" --visibility "$VISIBILITY" \
    --report /results/report.json --out /results/result.json ${ID_ARGS[@]+"${ID_ARGS[@]}"} \
    >"$RESULTS/result.log" 2>&1 && [ -f "$RESULTS/result.json" ]; then
  cp "$RESULTS/result.json" "$OUT" || exit 1
else
  write_fallback "result writer failed (scoring outcome was: ${STATUS}${DETAIL:+: $DETAIL})"
fi

if [ -n "$ARTIFACTS_OUT" ]; then
  # Logs, raw report and failure screenshots; screenshot paths in
  # result.json are relative to this directory.
  for f in build.log app.log runner.log report.json unpack.log; do
    [ -f "$RESULTS/$f" ] && cp "$RESULTS/$f" "$ARTIFACTS_OUT/"
  done
  [ -d "$RESULTS/output" ] && cp -R "$RESULTS/output" "$ARTIFACTS_OUT/"
fi

log "$(grep -E '^  "(status|passed|total)"' "$OUT" | tr -d ' ,"' | tr '\n' ' ')"
exit 0
}

main "$@"
