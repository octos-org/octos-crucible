#!/usr/bin/env python3
"""Agent side: unpack the submitted observer project and run it on the FIFOs.

    agent_entry.py /in/agent.zip

Runs in its own container (no network, no test material mounted). The zip
must carry observer.project.json at its root (observer-project-v1,
"protocol": "jsonl-v4"), as the starter kit's pack_agent.py writes it.
Only `run`, `working_directory` and `environment` are honoured; `build`
steps are not supported (the project must run as is on python:3.12-slim).

Environment for the agent = what run_local.py gives its agent (forwarded by
bridge.py via /pipes/env.json) + the model variables this container was
started with (OPENAI_BASE_URL = the platform's meter, OPENAI_API_KEY =
dummy, OPENAI_MODEL / MODEL; only when the taskset gives the agent a model)
+ the project's `environment`, with PATH, HOME and TMPDIR fixed here. stdout belongs to the protocol; the agent's
stderr is this container's stderr.

Exit codes: the agent's own, or 64 = the package is not a valid project.
Standard library only.
"""
import json
import os
import re
import stat
import subprocess
import sys
import zipfile

PIPES = "/pipes"
WORK = "/work"
PROJECT = os.path.join(WORK, "project")
MAX_FILES = 5000
MAX_BYTES = 200 << 20
SAFE_ENV_KEY = re.compile(r"^[A-Z][A-Z0-9_]{0,63}$")
PROTECTED = {"PATH", "HOME", "TMPDIR", "LD_PRELOAD", "PYTHONPATH", "PYTHONSTARTUP"}
MODEL_ENV = ("OPENAI_BASE_URL", "OPENAI_API_KEY", "OPENAI_MODEL", "MODEL")


def bad(reason: str) -> int:
    print(f"[agent_entry] invalid project: {reason}", file=sys.stderr, flush=True)
    return 64


def unpack(zip_path: str) -> str | None:
    try:
        zf = zipfile.ZipFile(zip_path)
    except (OSError, zipfile.BadZipFile):
        return "not a zip"
    infos = zf.infolist()
    if len(infos) > MAX_FILES:
        return "too many files"
    if sum(i.file_size for i in infos) > MAX_BYTES:
        return "too large"
    for i in infos:
        name = i.filename
        parts = name.split("/")
        if name.startswith("/") or "\\" in name or ".." in parts:
            return f"unsafe path {name!r}"
        if stat.S_ISLNK(i.external_attr >> 16):
            return f"symlink {name!r}"
    zf.extractall(PROJECT)
    # Restore the executable bit only (compiled agents, wrapper scripts).
    for i in infos:
        if not i.is_dir() and (i.external_attr >> 16) & 0o111:
            p = os.path.join(PROJECT, i.filename)
            os.chmod(p, os.stat(p).st_mode | 0o755)
    return None


def main() -> int:
    if len(sys.argv) != 2:
        return bad("usage: agent_entry.py ZIP")
    err = unpack(sys.argv[1])
    if err:
        return bad(err)
    try:
        with open(os.path.join(PROJECT, "observer.project.json"), encoding="utf-8") as f:
            manifest = json.load(f)
    except (OSError, ValueError):
        return bad("no readable observer.project.json at the zip root")
    if manifest.get("protocol") != "jsonl-v4":
        return bad('observer.project.json must declare "protocol": "jsonl-v4"')
    if manifest.get("build"):
        return bad("build steps are not supported by this scorer")
    run = manifest.get("run")
    if not (isinstance(run, list) and run and all(isinstance(a, str) and a for a in run)):
        return bad("run must be a non-empty list of strings")
    cwd = os.path.normpath(os.path.join(PROJECT, str(manifest.get("working_directory") or ".")))
    if not (cwd == PROJECT or cwd.startswith(PROJECT + os.sep)) or not os.path.isdir(cwd):
        return bad("working_directory must be a directory inside the project")
    extra = manifest.get("environment") or {}
    if not isinstance(extra, dict):
        return bad("environment must be an object")

    home = os.path.join(WORK, "home")
    os.makedirs(home, exist_ok=True)
    # Blocks until the engine's bridge opens the pipe; env.json is written before that.
    to_agent = os.open(os.path.join(PIPES, "to_agent"), os.O_RDONLY)
    with open(os.path.join(PIPES, "env.json"), encoding="utf-8") as f:
        env = {str(k): str(v) for k, v in json.load(f).items()}
    from_agent = os.open(os.path.join(PIPES, "from_agent"), os.O_WRONLY)
    env.update({k: os.environ[k] for k in MODEL_ENV if k in os.environ})
    env.update({"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": home, "TMPDIR": home})
    for k, v in extra.items():
        if SAFE_ENV_KEY.match(str(k)) and str(k) not in PROTECTED:
            env[str(k)] = str(v)
    try:
        proc = subprocess.Popen(run, cwd=cwd, env=env, stdin=to_agent, stdout=from_agent)
    except OSError as e:
        return bad(f"could not start {run[0]!r}: {e.strerror}")
    os.close(to_agent)
    os.close(from_agent)
    return proc.wait()


if __name__ == "__main__":
    raise SystemExit(main())
