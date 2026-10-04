#!/usr/bin/env python3
"""Engine side: run the starter kit's own run_local.py on the card, write result.json.

    engine_main.py --card /card --out /out [--visibility public|hidden]
                   [--task-id ID] [--submission-id ID]

run_local.py (unmodified, see /opt/engine/ENGINE_MANIFEST.json) drives the
agent through bridge.py; its last stdout line is the JSON score summary.

Output: result.json v2 (docs/scorer-contract.md §4): `score` = the engine's
own survey score S (continuous, may be negative, no max), `items` = its five
components (names fixed by this code, so they are kept under hidden
visibility). The agent never started / broke the protocol before the first
request: scored 0; the engine itself produced nothing: `error` (system).
Standard library only.
"""
import argparse
import json
import subprocess
import sys

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

    r = {"schema": 2, "visibility": a.visibility, "status": "error", "error": "system",
         "detail": "the survey engine produced no score"}
    if a.task_id:
        r["task_id"] = a.task_id
    if a.submission_id:
        r["submission_id"] = a.submission_id
    if isinstance(summary, dict) and "total" in summary:
        r.pop("error")
        r.update(status="scored", score=float(summary["total"]),
                 detail=str(summary.get("termination_reason") or "survey finished")[:300],
                 items=[{"name": k, "score": float(summary[k])} for k in COMPONENTS
                        if isinstance(summary.get(k), (int, float))])
    elif isinstance(summary, dict):
        r.pop("error")
        r.update(status="scored", score=0.0, detail="agent failed to start or initialize")
    with open(f"{a.out}/result.json", "w", encoding="utf-8") as f:
        json.dump(r, f, indent=2)
        f.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
