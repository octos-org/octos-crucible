#!/usr/bin/env bash
# The generic shell of uploaded scorer plugins (docs/plugins.md §14): runs
# the plugin's image (built by `crucible step score-tests` from its
# package and passed in CRUCIBLE_SCORER_IMAGE) on one stage output, inside
# the same walls as every scorer, and copies out its result.json.
#
#   score.sh --artifact FILE --tests DIR --out result.json
#            [--visibility public|hidden] [--run DIR] [--options FILE]
#            [--model-network NAME --model-base-url URL --model NAME]
#            [--task-id ID] [--submission-id ID] [--artifacts DIR]
#
# The container gets (its ENTRYPOINT is called with these arguments):
#   --artifact /in/artifact --tests /in/tests --out /out/result.json
#   --visibility V [--run /in/run] [--options /in/options.json]
#   [--model-base-url URL --model NAME]  (then OPENAI_BASE_URL is the meter's
#                                         address and OPENAI_API_KEY=dummy)
# Inputs are read-only, /out and /tmp are its only writable places; no
# network (or, with a model, only the meter); read-only root, no
# capabilities, no-new-privileges, memory / CPU / process limits, a non-root
# uid, CRUCIBLE_USER_SCORER_TIMEOUT_S (default 1200) of wall clock.
#
# Exit status: 0 = result.json written (an error result when the plugin
# wrote none), 2 = usage error. Containers: `crucible ctr` ($CRUCIBLE).
set -uo pipefail

main() {

usage() { sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }
die_usage() { echo "score.sh: $*" >&2; exit 2; }

ARTIFACT="" TESTS="" OUT="" VISIBILITY="public" OPTIONS="" RUN="" NETWORK="" MODEL_URL="" MODEL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --tests) TESTS="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --visibility) VISIBILITY="${2:-}"; shift 2 ;;
    --options) OPTIONS="${2:-}"; shift 2 ;;
    --run) RUN="${2:-}"; shift 2 ;;
    --model-network) NETWORK="${2:-}"; shift 2 ;;
    --model-base-url) MODEL_URL="${2:-}"; shift 2 ;;
    --model) MODEL="${2:-}"; shift 2 ;;
    --task-id|--submission-id|--artifacts) shift 2 ;;
    -h|--help) usage ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
if [ -z "$ARTIFACT" ] || [ -z "$TESTS" ] || [ -z "$OUT" ]; then usage; fi
[ -f "$ARTIFACT" ] || die_usage "--artifact: no such file: $ARTIFACT"
[ -d "$TESTS" ] || die_usage "--tests: no such directory: $TESTS"
[ -z "$OPTIONS" ] || [ -f "$OPTIONS" ] || die_usage "--options: no such file"
[ -z "$RUN" ] || [ -d "$RUN" ] || die_usage "--run: no such directory"
case "$VISIBILITY" in public|hidden) ;; *) die_usage "--visibility must be public or hidden" ;; esac
case "$NETWORK" in *[!A-Za-z0-9._-]*) die_usage "--model-network: bad name" ;; esac
case "$MODEL_URL" in ''|http://*) ;; *) die_usage "--model-base-url must be the meter's http URL" ;; esac
case "$MODEL" in *[!A-Za-z0-9._:/-]*) die_usage "--model: bad name" ;; esac
IMAGE="${CRUCIBLE_SCORER_IMAGE:-}"
[ -n "$IMAGE" ] || die_usage "CRUCIBLE_SCORER_IMAGE is not set (the scoring step builds the plugin)"
CRUCIBLE="${CRUCIBLE:-crucible}"
command -v "$CRUCIBLE" >/dev/null || die_usage "needs crucible on PATH (or \$CRUCIBLE)"
TIMEOUT_S="${CRUCIBLE_USER_SCORER_TIMEOUT_S:-1200}"
case "$TIMEOUT_S" in ''|*[!0-9]*) die_usage "CRUCIBLE_USER_SCORER_TIMEOUT_S must be a number" ;; esac
ctr() { "$CRUCIBLE" ctr "$@"; }

mkdir -p "$(dirname "$OUT")" || exit 1
OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
fail() {
  printf '{"schema": 2, "visibility": "%s", "status": "error", "error": "system", "detail": "%s"}\n' \
    "$VISIBILITY" "$1" >"$OUT" || exit 1
  echo "[user-scorer] $1" >&2
  exit 0
}

abspath() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")"); }
ARTIFACT="$(abspath "$ARTIFACT")"
TESTS="$(cd "$TESTS" && pwd -P)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/crucible-user-scorer.XXXXXX")" || exit 1
NAME="crucible-user-scorer-$(basename "$WORK" | tr 'A-Z' 'a-z' | tr -cd 'a-z0-9-')"
cleanup() { ctr rm -f "$NAME" >/dev/null 2>&1; rm -rf "$WORK"; }
trap cleanup EXIT
if [ "$(id -u)" = "0" ]; then RUN_AS="1000:1000"; chmod a+rwx "$WORK"; else RUN_AS="$(id -u):$(id -g)"; fi

mounts=(-v "$ARTIFACT:/in/artifact:ro" -v "$TESTS:/in/tests:ro" -v "$WORK:/out")
args=(--artifact /in/artifact --tests /in/tests --out /out/result.json --visibility "$VISIBILITY")
if [ -n "$OPTIONS" ]; then
  cp "$OPTIONS" "$WORK/options.json" || exit 1
  args+=(--options /out/options.json)
fi
if [ -n "$RUN" ]; then mounts+=(-v "$(cd "$RUN" && pwd -P):/in/run:ro"); args+=(--run /in/run); fi
net=(--network none)
envs=(-e HOME=/tmp)
if [ -n "$NETWORK" ] && [ -n "$MODEL_URL" ] && [ -n "$MODEL" ]; then
  net=(--network "$NETWORK" --dns 127.0.0.1)
  envs+=(-e "OPENAI_BASE_URL=$MODEL_URL" -e OPENAI_API_KEY=dummy)
  args+=(--model-base-url "$MODEL_URL" --model "$MODEL")
fi
timeout --kill-after=10 "$TIMEOUT_S" "$CRUCIBLE" ctr run --rm --name "$NAME" "${net[@]}" \
  --read-only --tmpfs /tmp:rw,size=256m --cap-drop ALL --security-opt no-new-privileges \
  --memory 2g --memory-swap 2g --cpus 2 --pids-limit 256 --user "$RUN_AS" \
  "${envs[@]}" "${mounts[@]}" "$IMAGE" "${args[@]}" >&2
rc=$?
[ "$rc" = 124 ] && fail "the plugin ran out of time (${TIMEOUT_S}s)"
[ -s "$WORK/result.json" ] || fail "the plugin wrote no result.json (exit status $rc)"
cp "$WORK/result.json" "$OUT" || exit 1
echo "[user-scorer] $(tr -d '\n' <"$OUT" | head -c 400)" >&2
exit 0
}

main "$@"
