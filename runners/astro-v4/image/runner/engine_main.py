#!/usr/bin/env python3
"""Engine side: run the starter kit's own run_local.py on the card.

    engine_main.py --card /card --out /out

run_local.py (unmodified, see /opt/engine/ENGINE_MANIFEST.json) drives the
agent through bridge.py; its last stdout line is the JSON score summary.

Output: /out/summary.json = {"exit": <run_local exit code>, "summary":
<run_local's summary object or null>}. Scoring it is the astro-survey
scorer's job. Standard library only.
"""
import argparse
import json
import subprocess
import sys


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--card", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    proc = subprocess.run(
        [sys.executable, "/opt/engine/run_local.py", "--card", a.card,
         "--agent", f"{sys.executable} /opt/runner/bridge.py",
         "--agent-cwd", "/tmp", "--out", f"{a.out}/run", "--quiet"],
        stdout=subprocess.PIPE, text=True)
    summary = None
    # run_local prints one indented JSON object last; parse from its first "{" line.
    lines = proc.stdout.strip().splitlines()
    for i, line in enumerate(lines):
        if line.startswith("{"):
            try:
                summary = json.loads("\n".join(lines[i:]))
            except ValueError:
                summary = None
    with open(f"{a.out}/summary.json", "w", encoding="utf-8") as f:
        json.dump({"exit": proc.returncode, "summary": summary}, f, indent=2, sort_keys=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
