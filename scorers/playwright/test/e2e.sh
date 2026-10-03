#!/usr/bin/env bash
# need() evals its argument later, so single quotes and "unused" vars are intended.
# shellcheck disable=SC2016,SC2034
# End-to-end check of score.sh against the fixture app and fixture tests.
# Needs docker, zip and node. Uses CRUCIBLE_SCORER_IMAGE if set.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCORE="$HERE/../score.sh"
T="$(mktemp -d "${TMPDIR:-/tmp}/scorer-e2e.XXXXXX")"
trap 'rm -rf "$T"' EXIT

(cd "$HERE/fixtures/app" && zip -qr "$T/app.zip" .)
mkdir -p "$T/noroot/sub" && cp "$HERE/fixtures/app/"* "$T/noroot/sub/"
(cd "$T/noroot" && zip -qr "$T/noroot.zip" .)
printf 'not a zip' >"$T/bad.zip"

need() { if eval "$1"; then echo "ok: $1"; else echo "FAIL: $1" >&2; exit 1; fi; }
check() { # check <result.json> <js expression over r>
  node -e '
    const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    if (!(eval(process.argv[2]))) { console.error("FAIL:", process.argv[2], JSON.stringify(r, null, 2)); process.exit(1); }
    console.log("ok:", process.argv[2]);
  ' "$1" "$2"
}

"$SCORE" --artifact "$T/app.zip" --tests "$HERE/fixtures/tests" --out "$T/public.json" \
  --artifacts "$T/art" --task-id fixture
check "$T/public.json" 'r.status === "failed" && r.passed === 2 && r.total === 3 && r.detail === "1/3 tests failed"'
check "$T/public.json" 'r.tests.filter(t => !t.ok).length === 1 && r.tests.find(t => !t.ok).title.endsWith("missing feature")'
shot="$(node -e 'const r=require(process.argv[1]); console.log(r.tests.find(t=>!t.ok).screenshot)' "$T/public.json")"
need '[ -f "$T/art/$shot" ] && [ -f "$T/art/build.log" ] && [ -f "$T/art/report.json" ]'

"$SCORE" --artifact "$T/app.zip" --tests "$HERE/fixtures/tests" --out "$T/hidden.json" --visibility hidden
check "$T/hidden.json" 'r.status === "failed" && r.passed === 2 && r.total === 3 && !("tests" in r) && r.visibility === "hidden"'

"$SCORE" --artifact "$T/bad.zip" --tests "$HERE/fixtures/tests" --out "$T/bad.json"
check "$T/bad.json" 'r.status === "failed" && r.total === 0 && r.detail.startsWith("zip failed validation")'

"$SCORE" --artifact "$T/noroot.zip" --tests "$HERE/fixtures/tests" --out "$T/noroot.json"
check "$T/noroot.json" 'r.status === "failed" && r.detail === "no Dockerfile at the root of the submitted app"'

rc=0; "$SCORE" --artifact "$T/app.zip" --tests "$T/missing" --out "$T/x.json" 2>/dev/null || rc=$?
need '[ "$rc" -eq 2 ] && [ ! -f "$T/x.json" ]'

leftover="$(docker ps -aq --filter label=crucible.scorer.run; docker network ls -q --filter name=crucible-net-)"
need '[ -z "$leftover" ]'
echo "e2e passed"
