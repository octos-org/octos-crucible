#!/usr/bin/env bash
# astro-survey scorer: score one survey run of the astro-v4 interactive
# runner (docs/plugins.md §4.2, docs/astro-survey.md) and write result.json
# v2: score = the engine's survey score, items = its five components.
#
#   score.sh --run RUN_DIR --artifact agent.zip --tests CARD_DIR --out result.json
#            [--visibility public|hidden] [--task-id ID] [--submission-id ID]
#
# Only RUN_DIR (the runner's run.json + summary.json) is read; the agent
# and the card were already handled by the runner. The record is read in a
# container with no network and a read-only root.
#
# Exit status: 0 = result.json written, 2 = usage error, 1 = no result.json.
# Environment (optional): CRUCIBLE_SCORER_IMAGE (prebuilt image; default:
# build ./image).
set -uo pipefail

main() {

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
usage() { sed -n '2,7p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }
die_usage() { echo "score.sh: $*" >&2; exit 2; }

RUN="" OUT="" VISIBILITY="public" TASK_ID="" SUBMISSION_ID=""
while [ $# -gt 0 ]; do
  case "$1" in
    --run) RUN="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --visibility) VISIBILITY="${2:-}"; shift 2 ;;
    --task-id) TASK_ID="${2:-}"; shift 2 ;;
    --submission-id) SUBMISSION_ID="${2:-}"; shift 2 ;;
    --artifact|--tests|--options|--artifacts) shift 2 ;;
    -h|--help) usage ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
if [ -z "$RUN" ] || [ -z "$OUT" ]; then usage; fi
[ -d "$RUN" ] || die_usage "--run: no such directory: $RUN"
case "$VISIBILITY" in public|hidden) ;; *) die_usage "--visibility must be public or hidden" ;; esac
for v in "$TASK_ID" "$SUBMISSION_ID"; do
  case "$v" in *[!A-Za-z0-9._-]*) die_usage "--task-id/--submission-id: only [A-Za-z0-9._-]" ;; esac
done
command -v docker >/dev/null || die_usage "needs docker on PATH"

RUN="$(cd "$RUN" && pwd -P)"
mkdir -p "$(dirname "$OUT")" || exit 1
OUT_DIR="$(cd "$(dirname "$OUT")" && pwd -P)"
OUT="$OUT_DIR/$(basename "$OUT")"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-astro-score.XXXXXX")" || exit 1
trap 'rm -rf "$WORK"' EXIT
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; chmod a+rwx "$WORK"; else RUN_AS="$(id -u):$(id -g)"; fi

fail() {
  printf '{"schema": 2, "visibility": "%s", "status": "error", "error": "system", "detail": "%s"}\n' \
    "$VISIBILITY" "$1" >"$OUT" || exit 1
  exit 0
}
if [ -n "${CRUCIBLE_SCORER_IMAGE:-}" ]; then
  IMAGE="$CRUCIBLE_SCORER_IMAGE"
else
  IMAGE="crucible-scorer-astro-survey:local"
  docker build -q -t "$IMAGE" "$HERE/image" >/dev/null 2>&1 || fail "scorer image failed to build"
fi
docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges \
  --memory 256m --pids-limit 64 --user "$RUN_AS" \
  -v "$RUN:/record:ro" -v "$WORK:/out" --entrypoint python3 "$IMAGE" /opt/scorer/score_main.py \
  --run /record --out /out/result.json --visibility "$VISIBILITY" \
  ${TASK_ID:+--task-id "$TASK_ID"} ${SUBMISSION_ID:+--submission-id "$SUBMISSION_ID"} >&2 \
  || fail "the scorer container failed"
[ -s "$WORK/result.json" ] || fail "the scorer wrote no result"
cp "$WORK/result.json" "$OUT" || exit 1
echo "[score] $(tr -d '\n' <"$OUT" | head -c 400)" >&2
exit 0
}

main "$@"
