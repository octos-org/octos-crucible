#!/usr/bin/env bash
# Astro-survey scorer: run one GOSIM Agentic Observer project (the artifact
# zip) against one v4 task card (the tests dir) with the starter kit's own
# engine, and write result.json v2 (docs/scorer-contract.md §4): score = the
# engine's survey score, items = its five components.
#
#   score.sh --artifact agent.zip --tests CARD_DIR --out result.json
#            [--visibility public|hidden] [--artifacts DIR]
#            [--task-id ID] [--submission-id ID] [--run-timeout 1500]
#
# CARD_DIR = config/ + public/ + truth/ of one card. The agent and the
# engine run in two containers (no network, read-only roots) joined only by
# two FIFOs on a private docker volume; the agent never sees the card.
# Score mapping and limits: docs/astro-survey.md.
#
# Exit status: 0 = result.json written, 2 = usage error, 1 = no result.json.
# Host requirements: bash, docker, timeout (coreutils).
# Environment (optional): CRUCIBLE_SCORER_IMAGE (prebuilt image; default:
# build ./image).
set -uo pipefail

main() {

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
usage() { sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }
die_usage() { echo "score.sh: $*" >&2; exit 2; }

ARTIFACT="" TESTS="" OUT="" VISIBILITY="public" ARTIFACTS_OUT="" TASK_ID="" SUBMISSION_ID=""
RUN_TIMEOUT_S=1500
while [ $# -gt 0 ]; do
  case "$1" in
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --tests) TESTS="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --visibility) VISIBILITY="${2:-}"; shift 2 ;;
    --artifacts) ARTIFACTS_OUT="${2:-}"; shift 2 ;;
    --task-id) TASK_ID="${2:-}"; shift 2 ;;
    --submission-id) SUBMISSION_ID="${2:-}"; shift 2 ;;
    --run-timeout) RUN_TIMEOUT_S="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
if [ -z "$ARTIFACT" ] || [ -z "$TESTS" ] || [ -z "$OUT" ]; then usage; fi
[ -f "$ARTIFACT" ] || die_usage "--artifact: no such file: $ARTIFACT"
[ -d "$TESTS" ] || die_usage "--tests: no such directory: $TESTS"
case "$VISIBILITY" in public|hidden) ;; *) die_usage "--visibility must be public or hidden" ;; esac
case "$RUN_TIMEOUT_S" in ''|*[!0-9]*) die_usage "--run-timeout must be a positive integer" ;; esac
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
PIPES="crucible-astro-pipes-$RUN_ID"
AGENT_CTR="crucible-astro-agent-$RUN_ID"
ENGINE_CTR="crucible-astro-engine-$RUN_ID"
LABEL="crucible.scorer.run=$RUN_ID"
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; else RUN_AS="$(id -u):$(id -g)"; fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-score.XXXXXX")" || exit 1
WORK="$(cd "$WORK" && pwd -P)"
mkdir -p "$WORK/out"
[ "$RUN_AS" = "1000:1000" ] && chmod -R a+rwX "$WORK"

# shellcheck disable=SC2317,SC2329 # invoked via trap
cleanup() {
  docker rm -f "$AGENT_CTR" "$ENGINE_CTR" >/dev/null 2>&1 || true
  docker volume rm -f "$PIPES" >/dev/null 2>&1 || true
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM
log() { echo "[score] $*" >&2; }

STATUS="" DETAIL=""
if [ -n "${CRUCIBLE_SCORER_IMAGE:-}" ]; then
  IMAGE="$CRUCIBLE_SCORER_IMAGE"
  docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE" >/dev/null 2>&1 \
    || { STATUS=error; DETAIL="scorer image unavailable: $IMAGE"; }
else
  IMAGE="crucible-scorer-astro-survey:local"
  docker build -q -t "$IMAGE" "$HERE/image" >"$WORK/scorer-build.log" 2>&1 \
    || { STATUS=error; DETAIL="scorer image failed to build"; }
fi

LOCKDOWN=(--network none --read-only --security-opt no-new-privileges --cap-drop ALL
  --memory 2g --memory-swap 2g --cpus 2 --pids-limit 256 --user "$RUN_AS" --label "$LABEL")

if [ -z "$STATUS" ]; then
  # Private volume with the two FIFOs, owned by the run uid.
  docker volume create --label "$LABEL" "$PIPES" >/dev/null \
    && docker run --rm --network none --label "$LABEL" -v "$PIPES:/pipes" --entrypoint sh "$IMAGE" -c \
      "mkfifo -m 600 /pipes/to_agent /pipes/from_agent && chown -R $RUN_AS /pipes && chmod 700 /pipes" \
    || { STATUS=error; DETAIL="could not set up the agent pipes"; }
fi

if [ -z "$STATUS" ]; then
  # Agent: sees only its zip and the pipes.
  docker run -d --name "$AGENT_CTR" "${LOCKDOWN[@]}" \
    --tmpfs /work:rw,exec,size=512m,uid="${RUN_AS%%:*}",gid="${RUN_AS##*:}" \
    -v "$ARTIFACT:/in/agent.zip:ro" -v "$PIPES:/pipes" \
    --entrypoint python3 "$IMAGE" /opt/scorer/agent_entry.py /in/agent.zip >/dev/null \
    || { STATUS=error; DETAIL="could not start the agent container"; }
fi

if [ -z "$STATUS" ]; then
  # Engine: sees the card (read-only) and the pipes; the wall clock is the
  # card's own (<= 900 s), RUN_TIMEOUT_S is only a backstop.
  log "running the survey (wall clock <= 900 s)"
  "$TIMEOUT_BIN" -k 10 "$RUN_TIMEOUT_S" docker run --rm --name "$ENGINE_CTR" "${LOCKDOWN[@]}" \
    --tmpfs /tmp:rw,size=256m -v "$TESTS:/card:ro" -v "$WORK/out:/out" -v "$PIPES:/pipes" \
    --entrypoint python3 "$IMAGE" /opt/scorer/engine_main.py --card /card --out /out \
    --visibility "$VISIBILITY" ${TASK_ID:+--task-id "$TASK_ID"} ${SUBMISSION_ID:+--submission-id "$SUBMISSION_ID"} \
    >"$WORK/engine.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] || log "engine container exited $rc"
  agent_rc="$(docker inspect -f '{{.State.ExitCode}}' "$AGENT_CTR" 2>/dev/null || echo "")"
  docker logs "$AGENT_CTR" >"$WORK/agent.stderr.log" 2>&1 || true
  docker rm -f "$AGENT_CTR" >/dev/null 2>&1 || true
  if [ ! -s "$WORK/out/result.json" ]; then
    if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
      STATUS=scored; DETAIL="survey run exceeded ${RUN_TIMEOUT_S}s"
    else
      STATUS=error; DETAIL="the survey engine wrote no result"
    fi
  elif [ "$agent_rc" = "64" ]; then
    STATUS=scored; DETAIL="agent zip is not a valid observer project (see docs/astro-survey.md)"
  fi
fi

if [ -n "$ARTIFACTS_OUT" ]; then
  cp "$WORK"/*.log "$ARTIFACTS_OUT"/ 2>/dev/null || true
  if [ "$VISIBILITY" = "public" ] && [ -d "$WORK/out/run" ]; then
    cp -R "$WORK/out/run" "$ARTIFACTS_OUT/run" 2>/dev/null || true
    rm -rf "$ARTIFACTS_OUT/run/scratch" 2>/dev/null || true
  fi
fi

if [ -n "$STATUS" ]; then
  {
    printf '{\n  "schema": 2,\n  "visibility": "%s",\n  "status": "%s",\n  "detail": "%s"' "$VISIBILITY" "$STATUS" "$DETAIL"
    if [ "$STATUS" = error ]; then printf ',\n  "error": "system"'; else printf ',\n  "score": 0'; fi
    [ -n "$TASK_ID" ] && printf ',\n  "task_id": "%s"' "$TASK_ID"
    [ -n "$SUBMISSION_ID" ] && printf ',\n  "submission_id": "%s"' "$SUBMISSION_ID"
    printf '\n}\n'
  } >"$OUT" || exit 1
else
  cp "$WORK/out/result.json" "$OUT" || exit 1
fi
log "$(tr -d '\n' <"$OUT" | head -c 400)"
exit 0
}

main "$@"
