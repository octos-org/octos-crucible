"""Keyword scorer: one point per required keyword found in the answer.

Called by the platform as
  score.py --artifact /in/artifact --tests /in/tests --out /out/result.json
           --visibility hidden|public [--options F] [--run D] [--model-base-url U --model M]
/in/tests/keywords.json: {"file": "answer.md", "keywords": ["...", ...]}.
The artifact is the stage output: a zip (packager `files`) holding that
file, or the text itself. Writes result.json v2 (docs/scorer-contract.md).
"""

import argparse
import json
import zipfile

p = argparse.ArgumentParser()
p.add_argument("--artifact", required=True)
p.add_argument("--tests", required=True)
p.add_argument("--out", required=True)
p.add_argument("--visibility", default="public")
args, _ = p.parse_known_args()


def write(result):
    result.update({"schema": 2, "visibility": args.visibility})
    with open(args.out, "w") as f:
        json.dump(result, f)


try:
    with open(f"{args.tests}/keywords.json") as f:
        spec = json.load(f)
    keywords = [str(k) for k in spec["keywords"]]
    name = spec.get("file", "answer.md")
except Exception:
    # Nothing to grade against (e.g. the platform's self-test without one).
    write({"status": "error", "error": "system", "detail": "no keywords.json"})
    raise SystemExit(0)

text = ""
if zipfile.is_zipfile(args.artifact):
    with zipfile.ZipFile(args.artifact) as z:
        if name in z.namelist():
            text = z.read(name)[:1_000_000].decode("utf-8", "replace")
else:
    with open(args.artifact, "rb") as f:
        text = f.read(1_000_000).decode("utf-8", "replace")

low = text.lower()
found = [k.lower() in low for k in keywords]
items = [
    # Hidden results: names fixed by this scorer, never the keywords.
    {"name": (k if args.visibility == "public" else f"keyword {i + 1}"), "passed": ok}
    for i, (k, ok) in enumerate(zip(keywords, found))
]
write({
    "status": "scored",
    "score": float(sum(found)),
    "max": float(len(keywords)),
    "passed": all(found),
    "detail": f"{sum(found)}/{len(keywords)} keywords" if text else "no answer",
    "items": items,
})
