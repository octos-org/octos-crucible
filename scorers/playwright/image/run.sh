#!/usr/bin/env bash
# Runner entrypoint: readiness probe, then the Playwright test pack.
# Inputs: env BASE_URL, READY_TIMEOUT; mounts /pack (tests, read-only) and
# /results (writable: report.json, screenshots).
# Exit codes: 3 = app never became ready; otherwise Playwright's own.
set -uo pipefail

echo "[runner] probing $BASE_URL (timeout ${READY_TIMEOUT:-60}s)"
node /opt/scorer/wait-ready.mjs || exit 3

echo "[runner] app ready, running pack at ${PACK_TEST_DIR:-/pack}"
# The globally installed @playwright/test binary (same as `npx playwright`,
# without npx needing a writable npm cache under an arbitrary --user uid).
exec playwright test --config=/opt/scorer/playwright.config.js
