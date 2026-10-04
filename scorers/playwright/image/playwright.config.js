// Runner-side Playwright config (copied from the prototype grader's
// runner/playwright.config.js; values must stay identical). The pack under
// test does not ship its own config; this file keeps baseURL, timeouts,
// concurrency and failure artifacts identical for every submission.
module.exports = {
  testDir: process.env.PACK_TEST_DIR || '/pack',
  timeout: Number(process.env.TEST_TIMEOUT_MS || 60000),
  retries: 0,
  workers: Number(process.env.RUNNER_WORKERS || 2),
  reporter: [['json', { outputFile: '/results/report.json' }], ['line']],
  outputDir: '/results/output',
  use: {
    headless: true,
    baseURL: process.env.BASE_URL,
    screenshot: 'only-on-failure',
    // score.sh sets CHROMIUM_SANDBOX=0: the tests themselves may be
    // untrusted code running in this container, so the container (non-root,
    // no capabilities, no-new-privileges, internal network, no secrets) is
    // the boundary and Chromium's own sandbox would add nothing.
    launchOptions: { chromiumSandbox: process.env.CHROMIUM_SANDBOX !== '0' },
  },
};
