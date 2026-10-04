#!/usr/bin/env bash
# llm-judge scorer: grade a written answer (e.g. a proof in answer.md)
# against the stage's rubric with a judge model, and write result.json v2
# (score = points, max = the rubric's total, items = points per rubric item).
#
#   score.sh --artifact output.zip --tests DIR --out result.json
#            --model-network NAME --model-base-url URL --model NAME
#            [--options FILE] [--visibility public|hidden] [--task-id ID]
#
# DIR holds rubric.json (problem, reference solution, marking scheme): data
# only, embedded in the scorer's own grading prompt (image/judge.py). The
# judge model is reached only through the platform's meter on
# --model-network (the key never reaches this script or its container).
# Options (JSON): {"judges": N} independent judgements, median per item
# (default 3). Without a model: result `error` (system).
#
# Exit status: 0 = result.json written, 2 = usage error, 1 = no result.json.
# Environment (optional): CRUCIBLE_SCORER_IMAGE (prebuilt image; default:
# build ./image).
set -uo pipefail

main() {

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
usage() { sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }
die_usage() { echo "score.sh: $*" >&2; exit 2; }

ARTIFACT="" TESTS="" OUT="" VISIBILITY="public" TASK_ID="" OPTIONS="" NETWORK="" MODEL_URL="" MODEL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --tests) TESTS="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --visibility) VISIBILITY="${2:-}"; shift 2 ;;
    --task-id) TASK_ID="${2:-}"; shift 2 ;;
    --options) OPTIONS="${2:-}"; shift 2 ;;
    --model-network) NETWORK="${2:-}"; shift 2 ;;
    --model-base-url) MODEL_URL="${2:-}"; shift 2 ;;
    --model) MODEL="${2:-}"; shift 2 ;;
    --submission-id|--artifacts|--run) shift 2 ;;
    -h|--help) usage ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
if [ -z "$ARTIFACT" ] || [ -z "$TESTS" ] || [ -z "$OUT" ]; then usage; fi
[ -f "$ARTIFACT" ] || die_usage "--artifact: no such file: $ARTIFACT"
[ -d "$TESTS" ] || die_usage "--tests: no such directory: $TESTS"
[ -z "$OPTIONS" ] || [ -f "$OPTIONS" ] || die_usage "--options: no such file"
case "$VISIBILITY" in public|hidden) ;; *) die_usage "--visibility must be public or hidden" ;; esac
case "$TASK_ID" in *[!A-Za-z0-9._-]*) die_usage "--task-id: only [A-Za-z0-9._-]" ;; esac
case "$NETWORK" in *[!A-Za-z0-9._-]*) die_usage "--model-network: bad name" ;; esac
case "$MODEL_URL" in ''|http://*) ;; *) die_usage "--model-base-url must be the meter's http URL" ;; esac
case "$MODEL" in *[!A-Za-z0-9._:/-]*) die_usage "--model: bad name" ;; esac
command -v docker >/dev/null || die_usage "needs docker on PATH"

mkdir -p "$(dirname "$OUT")" || exit 1
OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
fail() {
  printf '{"schema": 2, "visibility": "%s", "status": "error", "error": "system", "detail": "%s"}\n' \
    "$VISIBILITY" "$1" >"$OUT" || exit 1
  echo "[llm-judge] $1" >&2
  exit 0
}
if [ -z "$NETWORK" ] || [ -z "$MODEL_URL" ] || [ -z "$MODEL" ]; then
  fail "no judge model: this taskset needs a model credential"
fi

abspath() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")"); }
ARTIFACT="$(abspath "$ARTIFACT")"
TESTS="$(cd "$TESTS" && pwd -P)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-judge.XXXXXX")" || exit 1
trap 'rm -rf "$WORK"' EXIT
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; chmod a+rwx "$WORK"; else RUN_AS="$(id -u):$(id -g)"; fi
opt=()
if [ -n "$OPTIONS" ]; then cp "$OPTIONS" "$WORK/options.json"; opt=(--options /out/options.json); fi

if [ -n "${CRUCIBLE_SCORER_IMAGE:-}" ]; then
  IMAGE="$CRUCIBLE_SCORER_IMAGE"
else
  IMAGE="crucible-scorer-llm-judge:local"
  docker build -q -t "$IMAGE" "$HERE/image" >/dev/null 2>&1 || fail "scorer image failed to build"
fi
docker run --rm --network "$NETWORK" --dns 127.0.0.1 --read-only --cap-drop ALL \
  --security-opt no-new-privileges --memory 512m --pids-limit 64 --user "$RUN_AS" \
  -v "$ARTIFACT:/in/output.zip:ro" -v "$TESTS:/tests:ro" -v "$WORK:/out" \
  --entrypoint python3 "$IMAGE" /opt/scorer/judge.py \
  --artifact /in/output.zip --tests /tests --out /out/result.json \
  --base-url "$MODEL_URL" --model "$MODEL" --visibility "$VISIBILITY" \
  ${TASK_ID:+--task-id "$TASK_ID"} "${opt[@]}" >&2 \
  || fail "the judge container failed"
[ -s "$WORK/result.json" ] || fail "the judge wrote no result"
cp "$WORK/result.json" "$OUT" || exit 1
echo "[llm-judge] $(tr -d '\n' <"$OUT" | head -c 400)" >&2
exit 0
}

main "$@"
