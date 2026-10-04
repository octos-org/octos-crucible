#!/usr/bin/env python3
"""Score one astro-v4 run record: /record/run.json + /record/summary.json -> result.json v2.

    score_main.py --run /record --out /out/result.json [--visibility V]
                  [--task-id ID] [--submission-id ID]

`score` = the engine's survey score S (continuous, may be negative, no
max), `items` = its five components (names fixed by this code, so they are
kept under hidden visibility), `detail` = the engine's termination reason.
The agent failed (invalid project, never started, protocol broken before
the first request, ran past the time limit): scored 0. The runner itself
failed: `error` (system). Standard library only.
"""
import argparse
import json

COMPONENTS = ("sum_best_scores", "required_penalty", "uniformity_penalty",
              "report_settlement", "observation_request_reward")


def load(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--visibility", default="public")
    ap.add_argument("--task-id", default="")
    ap.add_argument("--submission-id", default="")
    a = ap.parse_args()

    run = load(f"{a.run}/run.json") or {}
    status = run.get("status")
    r = {"schema": 2, "visibility": a.visibility, "status": "error", "error": "system",
         "detail": "the survey engine produced no score"}
    if status == "agent_failed":
        r.pop("error")
        r.update(status="scored", score=0.0, detail=str(run.get("detail") or "agent failed")[:300])
    elif status == "completed":
        summary = (load(f"{a.run}/summary.json") or {}).get("summary")
        if isinstance(summary, dict) and isinstance(summary.get("total"), (int, float)):
            r.pop("error")
            r.update(status="scored", score=float(summary["total"]),
                     detail=str(summary.get("termination_reason") or "survey finished")[:300],
                     items=[{"name": k, "score": float(summary[k])} for k in COMPONENTS
                            if isinstance(summary.get(k), (int, float))])
        elif isinstance(summary, dict):
            r.pop("error")
            r.update(status="scored", score=0.0, detail="agent failed to start or initialize")
    if a.task_id:
        r["task_id"] = a.task_id
    if a.submission_id:
        r["submission_id"] = a.submission_id
    with open(a.out, "w", encoding="utf-8") as f:
        json.dump(r, f, indent=2)
        f.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
