# Octos (octos-org/octos-arc agent bundle, built from source at AGENT_REF)
# inside the ARC-Bench runner image. An ordinary agent package: nothing here
# is special-cased by the platform (docs/agent-contract.md).
FROM gyataro/arcbench-runner:local-base
ARG AGENT_REF=main

# The runner image ships without zip/unzip, which arc/pack.sh and the
# unpack step below need.
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends zip unzip \
    && rm -rf /var/lib/apt/lists/*

# Bundle: arc/pack.sh output unzipped as the runner's submission dir.
RUN set -eu; \
    git clone --quiet https://github.com/octos-org/octos-arc /tmp/octos-arc; \
    git -C /tmp/octos-arc -c advice.detachedHead=false checkout --quiet "$AGENT_REF"; \
    sh /tmp/octos-arc/arc/pack.sh >&2; \
    mkdir -p /workspace/submission /workspace/tests; \
    (cd /workspace/submission && unzip -q /tmp/octos-arc/octos-arc-bundle.zip); \
    test -f /workspace/submission/main.py && test -f /workspace/submission/requirements.txt; \
    printf '{"commit": "%s", "repo": "octos-org/octos-arc"}\n' "$(git -C /tmp/octos-arc rev-parse HEAD)" > /agent-build.json; \
    rm -rf /tmp/octos-arc

# Pre-fetch the pinned, hash-verified octos kernel so the run needs no
# GitHub access: at run time the container reaches only the meter and the
# package registries.
RUN set -eu; \
    pip install --quiet --target /opt/agent-deps -r /workspace/submission/requirements.txt; \
    cd /workspace/submission && PYTHONPATH=/opt/agent-deps:/workspace/submission \
      python3 -c "from pathlib import Path; import main; print(main._download_octos(Path('/opt/octos-cache')))"; \
    rm -f /opt/octos-cache/octos-bundle.tar.gz; chmod -R a+rX /opt/octos-cache /opt/agent-deps

# run_submission.py has fixed paths under /workspace: template/ is the
# project + output dir, so it points at the platform's work dir /work. The
# container runs as a non-root uid, so /workspace itself must be writable.
COPY runner-spec.json /workspace/runner-spec.json
COPY entry.sh /opt/agent/entry.sh
RUN ln -s /work /workspace/template && chmod 1777 /workspace && chmod 0755 /opt/agent/entry.sh
ENV OCTOS_CACHE_DIR=/opt/octos-cache \
    ARC_DEBUG=1 \
    PIP_TARGET=/tmp/arcbench-agent-deps \
    PYTHONPATH=/tmp/arcbench-agent-deps \
    npm_config_cache=/tmp/arcbench-npm-cache
ENTRYPOINT ["/opt/agent/entry.sh"]
