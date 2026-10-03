# Playwright scorer image: pinned Playwright + Chromium, test-pack agnostic.
# Also carries the in-image helpers (zip check/extract, report -> result.json)
# so the host only needs bash + docker.
#
# Same browser environment as the prototype grader's runner/Dockerfile (same
# base, Playwright 1.57.0, `install --with-deps chromium`); the install is
# split in two layers only to halve the peak disk needed while building.
FROM node:24-bookworm-slim@sha256:0e0ff40c39bc087845bfb27465a0df4ea419520094bc35842ff83dd8cbe6f9b6

ARG PLAYWRIGHT_VERSION=1.57.0
# Fixed, world-readable browser path: installed as root here, launched later
# as an unprivileged user.
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright
RUN npm install -g @playwright/test@${PLAYWRIGHT_VERSION} \
    && npx playwright install-deps chromium \
    && npm cache clean --force
RUN npx playwright install chromium \
    && mkdir -p /ms-playwright && chmod -R a+rX /ms-playwright

# The test pack is bind-mounted read-only with no node_modules of its own, so
# `import { test } from '@playwright/test'` resolves via the global install.
ENV NODE_PATH=/usr/local/lib/node_modules
# Works for any --user uid (score.sh runs as the caller's uid so it can clean
# up everything the container wrote).
ENV HOME=/tmp
ENV NODE_OPTIONS=--disable-warning=ExperimentalWarning

COPY playwright.config.js wait-ready.mjs run.sh /opt/scorer/
COPY src/ /opt/scorer/src/
RUN chmod -R a+rX /opt/scorer && chmod a+x /opt/scorer/run.sh

# Unprivileged by default; Chromium's own sandbox stays on (needs
# unprivileged user namespaces + seccomp_profile.json, see score.sh).
USER node

ENTRYPOINT ["/opt/scorer/run.sh"]
