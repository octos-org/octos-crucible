# astro-v4 interactive runner image: the GOSIM Agentic Observer v4 engine, locked.
#
# engine/  = the public starter kit's runner/ (gosim-observer-examples.zip,
#            release examples-2026-10-02), byte-identical; ENGINE_MANIFEST.json
#            pins every file to survey26 commit db4bddf3. `verify_engine.py`
#            runs at build time, so a drifted copy fails the build.
# runner/  = crucible glue (bridge / agent entry / engine main).
#
# The same image plays two roles in two containers: the engine (sees the
# card) and the agent (sees only the submitted zip). Python 3.12 is what the
# platform's own runner pins; standard library only, nothing to install.
FROM python:3.12-slim@sha256:dddfd7e07f9d15aeeca61529320492139d21cac7f0070c00609243e51e4e0016

ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 LANG=C.UTF-8 LC_ALL=C.UTF-8
COPY engine/ /opt/engine/
COPY runner/ /opt/runner/
RUN cd /opt/engine && python3 verify_engine.py \
    && chmod -R a+rX /opt/engine /opt/runner
