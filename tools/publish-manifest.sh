#!/usr/bin/env bash
# Results sinks of a finished manifest (eval.yml and score.yml publish jobs):
#   (a) always: seal it to the platform key and store it as a blob;
#   (b) only if SCORE_PUBLIC=true: commit it in clear to the data branch as
#       evals/<eval_id>.json (without the `download` field).
#
#   tools/publish-manifest.sh <manifest.json>
#
# Env: CRUCIBLE (binary), STORE, EVAL_ID, SCORE_PUBLIC, GITHUB_TOKEN,
# GITHUB_REPOSITORY, RUNNER_TEMP, GITHUB_STEP_SUMMARY.
set -euo pipefail

manifest="$1"

pub=$(jq -r '.current as $c | .keys[] | select(.key_id == $c) | .public_key' config/keys.json)
"$CRUCIBLE" seal --recipient "$pub" --in "$manifest" --out "$RUNNER_TEMP/manifest.sealed"
sha=$("$CRUCIBLE" put --store "$STORE" "$RUNNER_TEMP/manifest.sealed")
echo "manifest archived as blob $sha"
{
  echo "## eval $EVAL_ID"
  echo
  echo "manifest blob: \`$sha\`"
  echo
  echo "total_score: $(jq -r '.total_score // "none"' "$manifest")"
} >> "$GITHUB_STEP_SUMMARY"

if [ "$SCORE_PUBLIC" != "true" ]; then
  exit 0
fi
d="$RUNNER_TEMP/data"
git init -q "$d"
cd "$d"
git remote add origin "https://github.com/$GITHUB_REPOSITORY.git"
auth="AUTHORIZATION: basic $(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 -w0)"
g() { git -c "http.https://github.com/.extraheader=$auth" "$@"; }
for attempt in 1 2 3 4 5; do
  if g ls-remote --exit-code origin refs/heads/data >/dev/null; then
    g fetch -q --depth 1 origin data
    git checkout -q -B data FETCH_HEAD
  else
    git checkout -q --orphan data
  fi
  mkdir -p evals
  jq 'del(.download)' "$manifest" > "evals/$EVAL_ID.json"
  git add "evals/$EVAL_ID.json"
  git -c user.name="crucible-bot" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
    commit -q -m "eval $EVAL_ID" || { echo "manifest unchanged"; break; }
  if g push -q origin HEAD:refs/heads/data; then
    echo "published evals/$EVAL_ID.json on data"
    break
  fi
  echo "push raced, retrying ($attempt)"
  sleep $((attempt * 3))
done
