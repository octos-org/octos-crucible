#!/usr/bin/env python3
"""Bridge from crucible's scorer contract to the official ARC-Bench runner.

Every step that decides a score is a function of the official runner,
/opt/arcbench/run_submission.py, loaded from the pinned runner image the
same way the image's own /opt/arcbench/local_runner.py loads it. Nothing
here re-implements install, build, start, readiness, the Playwright config
or the result parsing; this file only splits the official sequence over
three containers so the app never sees the tests and the tests never reach
the network (see score.sh):

  build   run_web_template() up to the point where it starts the server:
          npm install + npm run build (frontend), npm install (backend).
  serve   run_web_template() again with the two install/build helpers
          turned into no-ops (their work is already on disk): starts
          `npm run start` with HOST/PORT and waits with the official
          wait_for_http_ready(url, 120).
  test    the tail of the official main(): write_playwright_config(),
          ensure_test_package(), run_playwright_tests_with_progress(),
          parse_playwright_results().

Plus two helpers that are not part of the official flow: `unpack` (zip
safety check + extraction into /workspace/template) and `result` (official
results -> crucible result.json v2).
"""

import importlib.util
import json
import os
import shutil
import stat
import sys
import time
import zipfile
from pathlib import Path

OFFICIAL = Path("/opt/arcbench/run_submission.py")
MAX_FILES = 50_000
MAX_BYTES = 2 * 1024**3


def load_official():
    spec = importlib.util.spec_from_file_location("arcbench_production_runner", OFFICIAL)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def say(msg):
    print(f"[crucible] {msg}", flush=True)


def as_non_root():
    """The official runner runs as root and npm reads /root/.npmrc (the
    registry). Where containers may not run as root (Kubernetes namespaces
    that enforce the `restricted` Pod Security Standard) build and serve
    run as an ordinary user: the same npm config (a readable copy, see the
    Dockerfile) and a writable HOME for npm's cache. Root: nothing changes."""
    if os.geteuid() == 0:
        return
    home = Path("/tmp/crucible-home")
    home.mkdir(parents=True, exist_ok=True)
    os.environ["HOME"] = str(home)
    os.environ["NPM_CONFIG_USERCONFIG"] = "/opt/crucible/npmrc"


def unpack(zip_path, dest):
    """Exit 1 = the zip is the submission's fault (bad paths, links, size)."""
    dest = Path(dest)
    try:
        zf = zipfile.ZipFile(zip_path)
        infos = zf.infolist()
    except (OSError, zipfile.BadZipFile) as e:
        say(f"unreadable zip: {type(e).__name__}")
        return 1
    if len(infos) > MAX_FILES:
        say(f"zip has {len(infos)} entries (limit {MAX_FILES})")
        return 1
    if sum(i.file_size for i in infos) > MAX_BYTES:
        say("zip expands beyond the size limit")
        return 1
    for i in infos:
        name = i.filename
        parts = name.rstrip("/").split("/")
        if name.startswith("/") or "\\" in name or any(p in ("", ".", "..") for p in parts):
            say("zip has an absolute, '..' or malformed path")
            return 1
        if stat.S_ISLNK(i.external_attr >> 16):
            say("zip contains a symbolic link")
            return 1
    dest.mkdir(parents=True, exist_ok=True)
    for i in infos:
        target = dest / i.filename
        if i.is_dir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        with zf.open(i) as src, open(target, "wb") as out:
            shutil.copyfileobj(src, out)
        # Keep only the executable bit, as git would.
        target.chmod(0o755 if (i.external_attr >> 16) & 0o111 else 0o644)
    return 0


def build():
    """Exit 0 = installed and built; 1 = the official step failed."""
    as_non_root()
    r = load_official()
    r.ARC_DIR.mkdir(parents=True, exist_ok=True)

    class Built(Exception):
        pass

    def stop_before_start(*_a, **_k):
        raise Built()

    r.start_background_process = stop_before_start
    with r.STDOUT_PATH.open("a", encoding="utf-8") as out:
        try:
            r.run_web_template(out, out)
        except Built:
            say("build ok")
            return 0
        except Exception as e:  # noqa: BLE001 - the official main() does the same
            # HEARTBEAT_SUMMARY is the official runner's last step message,
            # e.g. "Installing dependencies for frontend".
            say(f"build failed at: {r.HEARTBEAT_SUMMARY} ({type(e).__name__})")
            out.flush()
            print(r.STDOUT_PATH.read_text(encoding="utf-8", errors="replace")[-6000:], flush=True)
            return 1
    return 1


def serve():
    """Print 'app ready' and keep serving, or exit 3 if never ready."""
    as_non_root()
    r = load_official()
    r.ARC_DIR.mkdir(parents=True, exist_ok=True)
    r.install_node_dependencies = lambda *_a, **_k: None  # done by `build`
    r.run_command = lambda *_a, **_k: None  # `npm run build`, done by `build`
    with r.STDOUT_PATH.open("a", encoding="utf-8") as out:
        try:
            runtime = r.run_web_template(out, out)
        except Exception as e:  # noqa: BLE001
            say(f"app not ready: {type(e).__name__}")
            return 3
        say("app ready")
        runtime["app_process"].wait()
        say(f"app exited with {runtime['app_process'].returncode}")
        # Stay up so the test container keeps its network namespace; the
        # tests then fail against a dead app, as they would officially.
        while True:
            time.sleep(3600)


def test(pack, results):
    """Exit 0 = official results written to <results>/official.json."""
    r = load_official()
    pack = Path(pack)
    # A taskset tests block keeps its directory name (tests/...): the
    # official runner has the specs directly in /workspace/tests.
    entries = list(pack.iterdir())
    if len(entries) == 1 and entries[0].is_dir():
        pack = entries[0]
    shutil.copytree(pack, r.TESTS_DIR)
    r.ARC_DIR.mkdir(parents=True, exist_ok=True)
    with r.STDOUT_PATH.open("a", encoding="utf-8") as out:
        r.write_playwright_config(r.WEB_APP_BASE_URL)
        r.ensure_test_package(out, out)
        process = r.run_playwright_tests_with_progress(out, out)
        say(f"playwright exited {process.returncode}")
        try:
            res = r.parse_playwright_results()
        except Exception as e:  # noqa: BLE001
            say(f"no usable report: {e}")
            return 4
    keep = {"passed": res["passed"], "failed": res["failed"],
            "tests": [{"name": t["name"], "status": t["status"]} for t in res["tests"]]}
    Path(results, "official.json").write_text(json.dumps(keep, indent=1) + "\n", encoding="utf-8")
    shutil.copy(r.PLAYWRIGHT_REPORT_PATH, Path(results, "playwright-report.json"))
    return 0


def result(official, status, detail, visibility, out, task_id, submission_id):
    """crucible result.json v2 (docs/scorer-contract.md §4)."""
    res = {"schema": 2, "visibility": visibility}
    if task_id:
        res["task_id"] = task_id
    if submission_id:
        res["submission_id"] = submission_id
    if status == "scored" and Path(official).is_file():
        o = json.loads(Path(official).read_text(encoding="utf-8"))
        total = o["passed"] + o["failed"]
        res.update(status="scored", score=o["passed"], max=total, passed=o["failed"] == 0,
                   detail=f"{o['passed']}/{total} tests passed")
        if visibility == "public":
            # Test titles come from the tests: only in public results.
            res["items"] = [{"name": t["name"][:100], "passed": t["status"] in ("passed", "expected")}
                            for t in o["tests"]][:100]
    elif status == "zero":
        res.update(status="scored", score=0, passed=False, detail=detail[:300])
    else:
        res.update(status="error", error="system", detail=detail[:300])
    Path(out).write_text(json.dumps(res, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return 0


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "unpack":
        try:
            return unpack(argv[2], argv[3])
        except Exception as e:  # noqa: BLE001 - not the zip's fault: system_error
            say(f"unpack error: {type(e).__name__}: {e}")
            return 5
    if cmd == "build":
        return build()
    if cmd == "serve":
        return serve()
    if cmd == "test":
        return test(argv[2], argv[3])
    if cmd == "result":
        return result(*argv[2:9])
    print("usage: official.py unpack ZIP DIR | build | serve | test PACK RESULTS | "
          "result OFFICIAL STATUS DETAIL VISIBILITY OUT TASK_ID SUBMISSION_ID", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
