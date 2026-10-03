# OpenAI Codex CLI (github.com/openai/codex), built from source at AGENT_REF
# with patches/ applied. An ordinary agent package: nothing here is
# special-cased by the platform (docs/agent-contract.md).
#
# Built from source rather than from the official release binary because the
# z.ai role-compat patch is needed (GLM chat completions reject the
# "developer" role). Pinned to rust-v0.93.0: the meter only serves
# /chat/completions, and later Codex releases drop wire_api = "chat".
#
# AGENT_REF is a tag name or a commit / tag-object SHA (the eval workflow
# resolves upstream.json with `git ls-remote` and passes the SHA).
FROM rust:1-bookworm AS build
ARG AGENT_REF=rust-v0.93.0
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
COPY patches /patches
RUN set -eu; \
    git init --quiet /src; \
    git -C /src fetch --quiet --depth 1 https://github.com/openai/codex "$AGENT_REF"; \
    git -C /src -c advice.detachedHead=false checkout --quiet "FETCH_HEAD^{commit}"; \
    for p in /patches/*.patch; do git -C /src apply "$p"; done; \
    printf '{"commit": "%s", "repo": "openai/codex", "patches": "%s"}\n' \
      "$(git -C /src rev-parse HEAD)" "$(cat /patches/*.patch | sha256sum | cut -c1-16)" > /agent-build.json
# codex-rs/rust-toolchain.toml pins the toolchain; rustup installs it here.
RUN cd /src/codex-rs && cargo build --release --locked -p codex-cli --bin codex \
    && install -m 0755 target/release/codex /usr/local/bin/codex \
    && rm -rf target

# Node for the web apps codex writes; git and ripgrep are tools codex uses.
FROM node:24-bookworm-slim
RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends ca-certificates git ripgrep \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/codex /usr/local/bin/codex
COPY --from=build /agent-build.json /agent-build.json
COPY prompt.txt entry.sh /opt/agent/
RUN chmod 0755 /opt/agent/entry.sh && chmod 0644 /opt/agent/prompt.txt
ENTRYPOINT ["/opt/agent/entry.sh"]
