#!/usr/bin/env bash
# Container entry, run once per stage. Octos does not know about stages: on
# stage 2+ /work already holds the previous stage's frontend/ and backend/,
# and octos seeds its pipeline from them (verify_node.py --seed), so it
# extends the existing app. /req holds this stage's requirements only.
set -euo pipefail

mkdir -p /work/.arc "$HOME"
# Fresh copy of this stage's requirements (drop files of the previous stage).
rm -rf /work/requirements
mkdir -p /work/requirements
cp -r /req/. /work/requirements/
rm -f /work/.arc/run-summary.json
git config --global --add safe.directory /work 2>/dev/null || true
echo "Actions ARC-Bench workspace assembled successfully." > /workspace/execution.debug.log

# Fit octos' own time budget into the platform deadline: its wait loop ends
# at budget + final reserve, then collect + boot gate take up to ~16 min.
# Octos raises the budget to 200 s per requirement node if that is larger;
# the platform stops the container at DEADLINE_S regardless, and octos
# delivers accepted progress to /work every minute.
# Short stages (e.g. the 600 s demo-todo smoke task) scale both down so the
# run still ends near the deadline; ARC-sized stages keep 300 + 1200 s.
deadline=${DEADLINE_S:-3600}
reserve=$(( deadline / 6 )); [ "$reserve" -gt 300 ] && reserve=300
tail_s=$(( deadline / 3 )); [ "$tail_s" -gt 1200 ] && tail_s=1200
budget=$(( deadline - reserve - tail_s ))
floor=$(( deadline / 2 )); [ "$floor" -gt 600 ] && floor=600
[ "$budget" -lt "$floor" ] && budget=$floor
export OCTOS_TIME_BUDGET="${OCTOS_TIME_BUDGET:-$budget}"
export OCTOS_ARC_FINAL_RESERVE="${OCTOS_ARC_FINAL_RESERVE:-$reserve}"

rc=0
python3 /opt/arcbench/run_submission.py || rc=$?
echo "run_submission exit code: $rc"
tail -c 2000 /work/.arc/run-summary.json 2>/dev/null || true
exit "$rc"
