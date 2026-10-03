import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildResult, toJson, walkReport } from '../image/src/report.ts';

const report = {
  suites: [
    {
      title: 'a.spec.ts',
      specs: [
        { title: 'passes', ok: true, tests: [{ results: [{ status: 'passed' }] }] },
        {
          title: 'fails',
          ok: false,
          tests: [
            {
              results: [
                {
                  status: 'failed',
                  error: { message: '\u001b[31mError:\u001b[39m expected   \n' },
                  attachments: [
                    { contentType: 'text/plain', path: '/results/output/x/trace.txt' },
                    { contentType: 'image/png', path: '/results/output/x/test-failed-1.png' },
                  ],
                },
              ],
            },
          ],
        },
        { title: 'skipped', ok: true, tests: [{ results: [{ status: 'skipped' }] }] },
        { title: 'never ran', ok: true, tests: [{ results: [] }] },
      ],
      suites: [
        {
          title: 'group',
          specs: [{ title: 'nested', ok: true, tests: [{ results: [{ status: 'expected' }] }] }],
        },
      ],
    },
  ],
};

test('walkReport flattens suites like resultshape.walk_report', () => {
  assert.deepEqual(walkReport(report), [
    { title: 'a.spec.ts passes', ok: true, error: null, screenshot: null },
    { title: 'a.spec.ts fails', ok: false, error: 'Error: expected', screenshot: 'output/x/test-failed-1.png' },
    { title: 'a.spec.ts skipped', ok: false, error: null, screenshot: null },
    { title: 'a.spec.ts never ran', ok: false, error: null, screenshot: null },
    { title: 'a.spec.ts group nested', ok: true, error: null, screenshot: null },
  ]);
});

test('error text is capped at 2000 characters', () => {
  const long = { suites: [{ title: 's', specs: [{ title: 't', ok: false, tests: [{ results: [{ status: 'failed', error: { message: 'é'.repeat(3000) } }] }] }] }] };
  assert.equal(walkReport(long)[0].error!.length, 2000);
});

test('public result carries per-test detail', () => {
  const r = buildResult({ taskId: 't1', visibility: 'public', status: 'scored', detail: '', report });
  assert.equal(r.status, 'failed');
  assert.equal(r.passed, 2);
  assert.equal(r.total, 5);
  assert.equal(r.detail, '3/5 tests failed');
  assert.equal(r.tests!.length, 5);
  assert.deepEqual(Object.keys(r), ['task_id', 'visibility', 'status', 'passed', 'total', 'detail', 'tests']);
});

test('hidden result keeps only the score and metadata', () => {
  const r = buildResult({ submissionId: 's', taskId: 't1', visibility: 'hidden', status: 'scored', detail: '', report });
  assert.deepEqual(r, {
    submission_id: 's',
    task_id: 't1',
    visibility: 'hidden',
    status: 'failed',
    passed: 2,
    total: 5,
    detail: '3/5 tests failed',
  });
});

test('all passing -> passed with empty detail', () => {
  const ok = { suites: [{ title: 's', specs: [{ title: 't', ok: true, tests: [{ results: [{ status: 'passed' }] }] }] }] };
  const r = buildResult({ visibility: 'public', status: 'scored', detail: '', report: ok });
  assert.equal(r.status, 'passed');
  assert.equal(r.detail, '');
});

test('browser launch failure is a scorer fault', () => {
  const bad = {
    suites: [{ title: 's', specs: [{ title: 't', ok: false, tests: [{ results: [{ status: 'failed', error: { message: 'browserType.launch: boom' } }] }] }] }],
  };
  const r = buildResult({ visibility: 'public', status: 'scored', detail: '', report: bad });
  assert.equal(r.status, 'system_error');
  assert.equal(r.tests, undefined);
});

test('pre-decided outcomes pass through; missing or empty reports are system errors', () => {
  const f = buildResult({ visibility: 'public', status: 'failed', detail: 'app build failed', report: null });
  assert.deepEqual([f.status, f.passed, f.total, f.detail], ['failed', 0, 0, 'app build failed']);
  assert.equal(buildResult({ visibility: 'public', status: 'scored', detail: '', report: null }).detail, 'no report.json produced');
  assert.equal(buildResult({ visibility: 'public', status: 'scored', detail: '', report: { suites: [] } }).detail, 'no tests collected');
  const u = buildResult({ visibility: 'public', status: 'scored', detail: '', report: new Error('bad json') });
  assert.deepEqual([u.status, u.detail], ['system_error', 'report.json unreadable: bad json']);
});

test('toJson matches Python json.dumps(indent=2)', () => {
  assert.equal(toJson({ a: 'é', b: [], c: null }), '{\n  "a": "\\u00e9",\n  "b": [],\n  "c": null\n}');
});
