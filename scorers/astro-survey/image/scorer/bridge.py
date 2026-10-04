#!/usr/bin/env python3
"""Stand-in for the agent process inside the engine container.

run_local.py starts this as the "agent command". It relays its stdin/stdout
byte for byte to the real agent, which runs in a separate container, through
two FIFOs on a shared docker volume (/pipes). Before that it hands the
environment run_local.py gave it (PARTICIPANT_PROTOCOL, SAC_SCENARIO,
SAC_WALLCLOCK_SECONDS, ...) to the agent side as /pipes/env.json.

When the engine closes our stdin (after `finish`) we close the agent's stdin;
when the agent closes its stdout (it exited) we exit. Standard library only.
"""
import json
import os
import threading

PIPES = "/pipes"
NOT_FORWARDED = {"PATH", "HOME", "TMPDIR", "PWD", "SHLVL", "_", "HOSTNAME"}


def pump(src: int, dst: int) -> None:
    try:
        while True:
            chunk = os.read(src, 65536)
            if not chunk:
                break
            view = memoryview(chunk)
            while view:
                view = view[os.write(dst, view):]
    except OSError:
        pass
    finally:
        try:
            os.close(dst)
        except OSError:
            pass


def main() -> int:
    env = {k: v for k, v in os.environ.items() if k not in NOT_FORWARDED}
    tmp = os.path.join(PIPES, "env.json.tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(env, f)
    os.replace(tmp, os.path.join(PIPES, "env.json"))
    # Same open order as agent_entry.py (to_agent, then from_agent): no deadlock.
    to_agent = os.open(os.path.join(PIPES, "to_agent"), os.O_WRONLY)
    from_agent = os.open(os.path.join(PIPES, "from_agent"), os.O_RDONLY)
    t = threading.Thread(target=pump, args=(0, to_agent), daemon=True)
    t.start()
    pump(from_agent, 1)  # returns when the agent closes its stdout
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
