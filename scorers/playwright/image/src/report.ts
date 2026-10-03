// Playwright JSON report -> result.json (crucible-core ScoreResult).
//
// Ported line for line from the prototype grader's common/resultshape.py and
// scripts/parse_report.py so the same report.json yields the same result.

import { basename } from 'node:path';

export type Status = 'passed' | 'failed' | 'system_error' | 'rejected';
/** "scored" = a report.json exists and decides passed/failed. */
export type InputStatus = 'scored' | 'failed' | 'system_error' | 'rejected';
export type Visibility = 'public' | 'hidden';

export interface TestCase {
  title: string;
  ok: boolean;
  error: string | null;
  screenshot: string | null;
}

export interface ScoreResult {
  submission_id?: string;
  task_id?: string;
  visibility?: string;
  status: Status | 'scored';
  passed: number;
  total: number;
  detail: string;
  tests?: TestCase[];
}

const ANSI = /\x1b\[[0-9;]*m/g;
const RESULTS_MOUNT = '/results/';
const ERROR_MAX_CHARS = 2000;

/** The test browser failing to start is a scorer-side fault (system_error),
 *  never "0/N tests failed" for the agent. */
export const BROWSER_LAUNCH_FAILURE =
  /browserType\.launch|No usable sandbox|Chromium sandboxing failed|Failed to launch the browser process/;

type Json = any;

/** Python's str.strip() (whitespace only). */
function pyStrip(s: string): string {
  return s.replace(/^[\s\u001c-\u001f\u0085]+|[\s\u001c-\u001f\u0085]+$/gu, '');
}

/** Screenshot path as recorded by the runner container (outputDir is
 *  /results/output) -> path relative to the results directory. */
function rel(path: string | null | undefined): string | null {
  if (!path) return null;
  if (path.startsWith(RESULTS_MOUNT)) return path.slice(RESULTS_MOUNT.length);
  return basename(path);
}

/** Flatten Playwright's suite/spec tree into per-test cases. */
export function walkReport(report: Json): TestCase[] {
  const tests: TestCase[] = [];
  const walk = (suite: Json, prefix: string): void => {
    const title = pyStrip(`${prefix} ${suite?.title ?? ''}`);
    for (const spec of suite?.specs ?? []) {
      const name = pyStrip(`${title} ${spec?.title ?? ''}`);
      const ran: Json[] = [];
      for (const t of spec?.tests ?? []) {
        for (const r of t?.results ?? []) {
          if (r && Object.keys(r).length > 0) ran.push(r);
        }
      }
      const ok =
        ran.length > 0 &&
        ran.every((r) => r?.status === 'passed' || r?.status === 'expected') &&
        Boolean(spec?.ok ?? true);
      let error: string | null = null;
      let screenshot: string | null = null;
      if (!ok) {
        for (const r of ran) {
          const err = (r?.error || {})?.message;
          if (err && error === null) {
            error = Array.from(pyStrip(String(err).replace(ANSI, '')))
              .slice(0, ERROR_MAX_CHARS)
              .join('');
          }
          for (const att of r?.attachments ?? []) {
            if (String(att?.contentType ?? '').startsWith('image/') && screenshot === null) {
              screenshot = rel(att?.path);
            }
          }
        }
      }
      tests.push({ title: name, ok, error, screenshot });
    }
    for (const child of suite?.suites ?? []) walk(child, title);
  };
  for (const suite of report?.suites ?? []) walk(suite, '');
  return tests;
}

/** `hidden` keeps only status/passed/total plus the named metadata fields:
 *  no test titles, no error text, no screenshots. */
export function applyVisibility(result: ScoreResult, visibility: string, alwaysKeep: string[]): ScoreResult {
  if (visibility !== 'hidden') return result;
  const keep = new Set(['status', 'passed', 'total', ...alwaysKeep]);
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(result)) if (keep.has(k)) out[k] = v;
  return out as unknown as ScoreResult;
}

export interface BuildArgs {
  submissionId?: string;
  taskId?: string;
  visibility: Visibility;
  status: InputStatus;
  detail: string;
  /** Parsed report.json, or an Error if it existed but could not be read. */
  report: Json | Error | null;
}

export function buildResult(a: BuildArgs): ScoreResult {
  const result: ScoreResult = {
    ...(a.submissionId !== undefined ? { submission_id: a.submissionId } : {}),
    ...(a.taskId !== undefined ? { task_id: a.taskId } : {}),
    visibility: a.visibility,
    status: a.status,
    passed: 0,
    total: 0,
    detail: a.detail,
  };
  if (a.status === 'scored' && a.report !== null) {
    let tests: TestCase[] = [];
    if (a.report instanceof Error) {
      result.status = 'system_error';
      result.detail = `report.json unreadable: ${a.report.message}`;
    } else {
      tests = walkReport(a.report);
    }
    if (tests.some((t) => !t.ok && BROWSER_LAUNCH_FAILURE.test(t.error ?? ''))) {
      result.status = 'system_error';
      result.detail = 'test browser failed to start on the grader (not caused by the submitted app)';
    } else if (tests.length > 0) {
      const passed = tests.filter((t) => t.ok).length;
      const total = tests.length;
      result.passed = passed;
      result.total = total;
      result.status = passed === total ? 'passed' : 'failed';
      result.detail = passed === total ? '' : `${total - passed}/${total} tests failed`;
      result.tests = tests;
    } else if (result.status === 'scored') {
      result.status = 'system_error';
      result.detail = result.detail || 'no tests collected';
    }
  } else if (a.status === 'scored') {
    result.status = 'system_error';
    result.detail = result.detail || 'no report.json produced';
  }
  return applyVisibility(result, a.visibility, ['submission_id', 'task_id', 'visibility', 'detail']);
}

/** JSON with Python json.dumps(indent=2) formatting (ASCII-only output). */
export function toJson(v: unknown): string {
  return JSON.stringify(v, null, 2).replace(
    /[\u0080-￿]/g,
    (c) => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'),
  );
}
