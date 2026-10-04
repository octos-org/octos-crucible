#!/usr/bin/env python3
"""Minimal proof-writing agent (docs/agent-contract.md).

Reads the stage's problem from $REQ_DIR (problem.md, or every file there),
asks the model ($OPENAI_BASE_URL, $MODEL; the platform's meter) for a
complete proof, and writes it to answer.md in the work dir. One request,
plus up to two retries on network errors. Standard library only.
"""
import json
import os
import pathlib
import sys
import time
import urllib.request

SYSTEM = ("You are an expert competition mathematician. Write a complete, rigorous, "
          "well-organized proof in Markdown. Justify every step; state the final answer clearly.")


def problem_text(req: pathlib.Path) -> str:
    p = req / "problem.md"
    if p.is_file():
        return p.read_text(encoding="utf-8")
    return "\n\n".join(f.read_text(encoding="utf-8", errors="replace")
                       for f in sorted(req.rglob("*")) if f.is_file())


def ask(base: str, model: str, problem: str) -> str:
    body = json.dumps({"model": model, "temperature": 0.2, "messages": [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": problem + "\n\nReply with the proof only."}]}).encode()
    req = urllib.request.Request(base.rstrip("/") + "/chat/completions", data=body, headers={
        "Content-Type": "application/json",
        "Authorization": "Bearer " + os.environ.get("OPENAI_API_KEY", "dummy")})
    with urllib.request.urlopen(req, timeout=int(os.environ.get("DEADLINE_S", "600"))) as r:
        return json.load(r)["choices"][0]["message"].get("content") or ""


def main() -> int:
    req = pathlib.Path(os.environ.get("REQ_DIR", "/req"))
    work = pathlib.Path(os.environ.get("WORK_DIR", "/work"))
    base = os.environ.get("OPENAI_BASE_URL", "")
    model = os.environ.get("MODEL", "")
    problem = problem_text(req)
    for attempt in range(3):
        try:
            proof = ask(base, model, problem)
            break
        except Exception as e:  # noqa: BLE001 - report and retry
            print(f"model request failed ({type(e).__name__}), attempt {attempt + 1}", file=sys.stderr)
            time.sleep(5)
    else:
        return 1
    (work / "answer.md").write_text(proof.strip() + "\n", encoding="utf-8")
    print(f"wrote answer.md ({len(proof)} characters)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
