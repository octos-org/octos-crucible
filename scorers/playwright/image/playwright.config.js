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
    // Chromium's own sandbox is on unless the caller sets CHROMIUM_SANDBOX=0.
    // It needs unprivileged user namespaces: on GitHub's ubuntu-latest the
    // workflow lifts AppArmor's restriction on them and score.sh runs this
    // container with Playwright's seccomp profile. Off, the container still
    // runs as a non-root user,
    // on an internal (no-internet) network, with no-new-privileges, holding
    // only this task's tests and no secrets.
    launchOptions: { chromiumSandbox: process.env.CHROMIUM_SANDBOX !== '0' },
  },
};
