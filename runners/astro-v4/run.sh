#!/usr/bin/env bash
# astro-v4 interactive runner (docs/plugins.md §4.2): run one GOSIM Agentic
# Observer project (the agent zip) against one v4 task card (the hidden
# material) with the starter kit's own engine, and write the run record.
#
#   run.sh --agent agent.zip --material CARD_DIR --out DIR --time-limit S
#          [--agent-network NAME --model-base-url URL --model NAME]
#          [--options FILE]
#
# CARD_DIR = config/ + public/ + truth/ of one card. The agent and the
# engine run in two containers (read-only roots, cap-drop ALL) joined only
# by two FIFOs on a private volume; the agent never sees the card.
# The engine container has no network. The agent container has none either,
# unless the platform gives it a model: then it joins --agent-network (built
# and firewalled by the platform: its only reachable address is the meter)
# with OPENAI_BASE_URL = --model-base-url, OPENAI_API_KEY = dummy.
#
# Output (DIR): run.json {"status": "completed" | "agent_failed" | "error",
# "detail"}, and for completed / agent_failed runs the engine's
# summary.json (read by the astro-survey scorer). docs/astro-survey.md.
#
# Exit status: 0 = run.json written, 2 = usage error, 1 = no run.json.
# Host requirements: bash, crucible (`$CRUCIBLE`, default on PATH; containers
# are started with `crucible ctr` by the step's backend), timeout (coreutils).
# Environment (optional): CRUCIBLE_RUNNER_IMAGE (prebuilt image; default:
# build ./image).
set -uo pipefail

main() {

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
usage() { sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }
die_usage() { echo "run.sh: $*" >&2; exit 2; }

AGENT="" MATERIAL="" OUT="" TIME_LIMIT_S="" NETWORK="" MODEL_URL="" MODEL="" OPTIONS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --agent) AGENT="${2:-}"; shift 2 ;;
    --material) MATERIAL="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --time-limit) TIME_LIMIT_S="${2:-}"; shift 2 ;;
    --agent-network) NETWORK="${2:-}"; shift 2 ;;
    --model-base-url) MODEL_URL="${2:-}"; shift 2 ;;
    --model) MODEL="${2:-}"; shift 2 ;;
    --options) OPTIONS="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
if [ -z "$AGENT" ] || [ -z "$MATERIAL" ] || [ -z "$OUT" ] || [ -z "$TIME_LIMIT_S" ]; then usage; fi
[ -f "$AGENT" ] || die_usage "--agent: no such file: $AGENT"
[ -d "$MATERIAL" ] || die_usage "--material: no such directory: $MATERIAL"
case "$TIME_LIMIT_S" in ''|*[!0-9]*) die_usage "--time-limit must be a positive integer" ;; esac
if [ -n "$NETWORK$MODEL_URL$MODEL" ] && { [ -z "$NETWORK" ] || [ -z "$MODEL_URL" ] || [ -z "$MODEL" ]; }; then
  die_usage "--agent-network, --model-base-url and --model go together"
fi
case "$NETWORK" in *[!A-Za-z0-9._-]*) die_usage "--agent-network: bad name" ;; esac
case "$MODEL_URL" in ''|http://*) ;; *) die_usage "--model-base-url must be the meter's http URL" ;; esac
case "$MODEL" in *[!A-Za-z0-9._:/-]*) die_usage "--model: bad name" ;; esac
[ -z "$OPTIONS" ] || [ -f "$OPTIONS" ] || die_usage "--options: no such file"
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
[ -n "$TIMEOUT_BIN" ] || die_usage "needs \`timeout\` (GNU coreutils) on PATH"
CRUCIBLE="${CRUCIBLE:-crucible}"
command -v "$CRUCIBLE" >/dev/null || die_usage "needs crucible on PATH (or \$CRUCIBLE)"
ctr() { "$CRUCIBLE" ctr "$@"; }

abspath() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")"); }
AGENT="$(abspath "$AGENT")"
MATERIAL="$(cd "$MATERIAL" && pwd -P)"
mkdir -p "$OUT" || exit 1
OUT="$(cd "$OUT" && pwd -P)"

RUN_ID="$(date +%s)$$${RANDOM}"
PIPES="crucible-astro-pipes-$RUN_ID"
AGENT_CTR="crucible-astro-agent-$RUN_ID"
ENGINE_CTR="crucible-astro-engine-$RUN_ID"
LABEL="crucible.runner.run=$RUN_ID"
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; else RUN_AS="$(id -u):$(id -g)"; fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-astro.XXXXXX")" || exit 1
WORK="$(cd "$WORK" && pwd -P)"
mkdir -p "$WORK/out"
[ "$RUN_AS" = "1000:1000" ] && chmod -R a+rwX "$WORK"

# shellcheck disable=SC2317,SC2329 # invoked via trap
cleanup() {
  ctr rm -f "$AGENT_CTR" "$ENGINE_CTR" >/dev/null 2>&1 || true
  ctr volume rm -f "$PIPES" >/dev/null 2>&1 || true
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM
log() { echo "[astro-v4] $*" >&2; }

STATUS="" DETAIL=""
if [ -n "${CRUCIBLE_RUNNER_IMAGE:-}" ]; then
  IMAGE="$CRUCIBLE_RUNNER_IMAGE"
  ctr image exists "$IMAGE" >/dev/null 2>&1 || ctr image pull -q "$IMAGE" >/dev/null 2>&1 \
    || { STATUS=error; DETAIL="runner image unavailable: $IMAGE"; }
else
  IMAGE="crucible-runner-astro-v4:local"
  ctr build -q -t "$IMAGE" "$HERE/image" >"$WORK/build.log" 2>&1 \
    || { STATUS=error; DETAIL="runner image failed to build"; }
fi

LOCKDOWN=(--read-only --security-opt no-new-privileges --cap-drop ALL
  --memory 2g --memory-swap 2g --cpus 2 --pids-limit 256 --user "$RUN_AS" --label "$LABEL")
if [ -n "$NETWORK" ]; then
  AGENT_NET=(--network "$NETWORK" --dns 127.0.0.1
    -e "OPENAI_BASE_URL=$MODEL_URL" -e OPENAI_API_KEY=dummy -e "OPENAI_MODEL=$MODEL" -e "MODEL=$MODEL")
else
  AGENT_NET=(--network none)
fi

if [ -z "$STATUS" ]; then
  # Private volume with the two FIFOs, owned by the run uid.
  ctr volume create --label "$LABEL" "$PIPES" >/dev/null \
    && ctr run --rm --network none --label "$LABEL" -v "$PIPES:/pipes" --entrypoint sh "$IMAGE" -c \
      "mkfifo -m 600 /pipes/to_agent /pipes/from_agent && chown -R $RUN_AS /pipes && chmod 700 /pipes" \
    || { STATUS=error; DETAIL="could not set up the agent pipes"; }
fi

if [ -z "$STATUS" ]; then
  # Agent: sees only its zip and the pipes (and, with a model, the meter).
  ctr run -d --name "$AGENT_CTR" "${LOCKDOWN[@]}" "${AGENT_NET[@]}" \
    --tmpfs /work:rw,exec,size=512m,uid="${RUN_AS%%:*}",gid="${RUN_AS##*:}" \
    -v "$AGENT:/in/agent.zip:ro" -v "$PIPES:/pipes" \
    --entrypoint python3 "$IMAGE" /opt/runner/agent_entry.py /in/agent.zip >/dev/null \
    || { STATUS=error; DETAIL="could not start the agent container"; }
fi

if [ -z "$STATUS" ]; then
  # Engine: sees the card (read-only) and the pipes; the wall clock is the
  # card's own (<= 900 s), --time-limit is only a backstop.
  log "running the survey (wall clock <= 900 s)"
  "$TIMEOUT_BIN" -k 10 "$TIME_LIMIT_S" "$CRUCIBLE" ctr run --rm --name "$ENGINE_CTR" "${LOCKDOWN[@]}" --network none \
    --tmpfs /tmp:rw,size=256m -v "$MATERIAL:/card:ro" -v "$WORK/out:/out" -v "$PIPES:/pipes" \
    --entrypoint python3 "$IMAGE" /opt/runner/engine_main.py --card /card --out /out \
    >"$WORK/engine.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] || log "engine container exited $rc"
  agent_rc="$(ctr inspect -f exit-code "$AGENT_CTR" 2>/dev/null || echo "")"
  ctr rm -f "$AGENT_CTR" >/dev/null 2>&1 || true
  if [ ! -s "$WORK/out/summary.json" ]; then
    if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
      STATUS=agent_failed; DETAIL="survey run exceeded ${TIME_LIMIT_S}s"
    else
      STATUS=error; DETAIL="the survey engine wrote no result"
    fi
  elif [ "$agent_rc" = "64" ]; then
    STATUS=agent_failed; DETAIL="agent zip is not a valid observer project (see docs/astro-survey.md)"
  else
    STATUS=completed; DETAIL="survey finished"
    cp "$WORK/out/summary.json" "$OUT/summary.json" || { STATUS=error; DETAIL="could not copy the summary"; }
  fi
fi

printf '{"status": "%s", "detail": "%s"}\n' "$STATUS" "$DETAIL" >"$OUT/run.json" || exit 1
log "$(cat "$OUT/run.json")"
exit 0
}

main "$@"
