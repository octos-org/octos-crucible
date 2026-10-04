#!/usr/bin/env python3
"""Engine side: run the starter kit's own run_local.py on the card, write result.json.

    engine_main.py --card /card --out /out [--visibility public|hidden]
                   [--task-id ID] [--submission-id ID]

run_local.py (unmodified, see /opt/engine/ENGINE_MANIFEST.json) drives the
agent through bridge.py; its last stdout line is the JSON score summary.

Score mapping (docs/astro-survey.md): ScoreResult only has integer
passed/total, so the continuous survey score S becomes
    passed = clamp(round(S * 1000), 0, SCALE_TOTAL), total = SCALE_TOTAL
(milli-points out of 10,000 points). The exact S is kept in `detail`.

status: `failed` whenever a score was produced but is below the cap (that is
"scored, agent's result", not "the agent broke"), `passed` only at the cap;
no score because the agent never started / broke the protocol before the
first request: `failed` 0/0; the engine itself produced nothing:
`system_error`. Standard library only.
"""
import argparse
import json
import subprocess
import sys

SCALE = 1000
SCALE_TOTAL = 10_000_000
COMPONENTS = ("sum_best_scores", "required_penalty", "uniformity_penalty",
              "report_settlement", "observation_request_reward")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--card", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--visibility", default="public")
    ap.add_argument("--task-id", default="")
    ap.add_argument("--submission-id", default="")
    a = ap.parse_args()

    proc = subprocess.run(
        [sys.executable, "/opt/engine/run_local.py", "--card", a.card,
         "--agent", f"{sys.executable} /opt/scorer/bridge.py",
         "--agent-cwd", "/tmp", "--out", f"{a.out}/run", "--quiet"],
        stdout=subprocess.PIPE, text=True)
    summary = None
    text = proc.stdout.strip()
    # run_local prints one indented JSON object last; parse from its first "{" line.
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if line.startswith("{"):
            try:
                summary = json.loads("\n".join(lines[i:]))
            except ValueError:
                summary = None
    with open(f"{a.out}/summary.json", "w", encoding="utf-8") as f:
        json.dump({"exit": proc.returncode, "summary": summary}, f, indent=2, sort_keys=True)

    r = {"visibility": a.visibility, "status": "system_error", "passed": 0, "total": 0,
         "detail": "the survey engine produced no score"}
    if a.task_id:
        r["task_id"] = a.task_id
    if a.submission_id:
        r["submission_id"] = a.submission_id
    tests = []
    if isinstance(summary, dict) and "total" in summary:
        score = float(summary["total"])
        passed = max(0, min(SCALE_TOTAL, round(score * SCALE)))
        r.update(status="passed" if passed == SCALE_TOTAL else "failed",
                 passed=passed, total=SCALE_TOTAL,
                 detail=f"survey score {summary['total']} ({summary.get('termination_reason')})")
        tests = [{"title": f"{k} = {summary.get(k)}", "ok": True, "error": None, "screenshot": None}
                 for k in COMPONENTS]
    elif isinstance(summary, dict):
        r.update(status="failed", detail="agent failed to start or initialize")
    if a.visibility == "public":
        r["tests"] = tests
    with open(f"{a.out}/result.json", "w", encoding="utf-8") as f:
        json.dump(r, f, indent=2)
        f.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
