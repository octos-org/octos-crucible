// Statistics and formatting for results. Pure functions, unit tested.

import type { ReplicaEntry, StageEntry, UsageTotals } from "./types";

export interface Summary {
  n: number;
  mean: number;
  /** Sample standard deviation (n-1); null when n < 2. */
  std: number | null;
  min: number;
  max: number;
}

export function summarize(values: (number | null | undefined)[]): Summary | null {
  const xs = values.filter((v): v is number => typeof v === "number" && Number.isFinite(v));
  const n = xs.length;
  if (n === 0) return null;
  const mean = xs.reduce((a, b) => a + b, 0) / n;
  const std =
    n < 2 ? null : Math.sqrt(xs.reduce((a, b) => a + (b - mean) ** 2, 0) / (n - 1));
  return { n, mean, std, min: Math.min(...xs), max: Math.max(...xs) };
}

/** Whether a stage has a score that says something about the agent. */
export function isScored(s: StageEntry): boolean {
  const st = s.score?.status;
  return (st === "passed" || st === "failed") && (s.score?.total ?? 0) > 0;
}

/** Fraction of tests passed, or null when not scored. */
export function stageScore(s: StageEntry): number | null {
  return isScored(s) ? s.score!.passed / s.score!.total : null;
}

export function usageOf(s: StageEntry): UsageTotals {
  const u = s.usage ?? {};
  return {
    requests: u.requests ?? 0,
    prompt_tokens: u.prompt_tokens ?? 0,
    cached_tokens: u.cached_tokens ?? 0,
    completion_tokens: u.completion_tokens ?? 0,
    reasoning_tokens: u.reasoning_tokens ?? 0,
  };
}

export function totalTokens(u: UsageTotals): number {
  return u.prompt_tokens + u.completion_tokens;
}

/** Cached prompt tokens / prompt tokens; null when nothing was sent. */
export function cacheRate(u: UsageTotals): number | null {
  return u.prompt_tokens > 0 ? u.cached_tokens / u.prompt_tokens : null;
}

export function addUsage(a: UsageTotals, b: UsageTotals): UsageTotals {
  return {
    requests: a.requests + b.requests,
    prompt_tokens: a.prompt_tokens + b.prompt_tokens,
    cached_tokens: a.cached_tokens + b.cached_tokens,
    completion_tokens: a.completion_tokens + b.completion_tokens,
    reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
  };
}

export interface ReplicaTotals {
  replica: number;
  /** passed/total over all stages; null when any stage is unscored. */
  score: number | null;
  passed: number;
  total: number;
  wall_s: number | null;
  usage: UsageTotals;
  /** null when any stage's price is unknown. */
  cost_usd: number | null;
}

const ZERO: UsageTotals = {
  requests: 0,
  prompt_tokens: 0,
  cached_tokens: 0,
  completion_tokens: 0,
  reasoning_tokens: 0,
};

export function replicaTotals(r: ReplicaEntry): ReplicaTotals {
  const stages = r.stages ?? [];
  let passed = 0;
  let total = 0;
  let complete = stages.length > 0;
  let wall: number | null = 0;
  let cost: number | null = 0;
  let usage = ZERO;
  for (const s of stages) {
    if (isScored(s)) {
      passed += s.score!.passed;
      total += s.score!.total;
    } else complete = false;
    wall = wall !== null && typeof s.wall_s === "number" ? wall + s.wall_s : null;
    cost = cost !== null && typeof s.cost_usd === "number" ? cost + s.cost_usd : null;
    usage = addUsage(usage, usageOf(s));
  }
  return {
    replica: r.replica,
    score: complete && total > 0 ? passed / total : null,
    passed,
    total,
    wall_s: stages.length ? wall : null,
    usage,
    cost_usd: stages.length ? cost : null,
  };
}

/** Stage names in first-seen order across replicas. */
export function stageNames(replicas: ReplicaEntry[]): string[] {
  const seen: string[] = [];
  for (const r of replicas) for (const s of r.stages ?? []) if (!seen.includes(s.stage)) seen.push(s.stage);
  return seen;
}

export function stageAcross(replicas: ReplicaEntry[], name: string): StageEntry[] {
  return replicas.flatMap((r) => (r.stages ?? []).filter((s) => s.stage === name));
}

// --- formatting ---

export function fmtPct(x: number | null | undefined, digits = 1): string {
  return typeof x === "number" && Number.isFinite(x) ? `${(x * 100).toFixed(digits)}%` : "—";
}

/** total_score from GET /evals: a 0–1 fraction (values above 1 are taken as percent). */
export function fmtScore(x: number | null | undefined): string {
  if (typeof x !== "number" || !Number.isFinite(x)) return "—";
  return x > 1 ? `${x.toFixed(1)}%` : fmtPct(x);
}

export function fmtDuration(s: number | null | undefined): string {
  if (typeof s !== "number" || !Number.isFinite(s) || s < 0) return "—";
  const t = Math.round(s);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const sec = t % 60;
  if (h > 0) return `${h} 小时 ${m} 分`;
  if (m > 0) return `${m} 分 ${sec} 秒`;
  return `${sec} 秒`;
}

export function fmtCount(n: number | null | undefined): string {
  if (typeof n !== "number" || !Number.isFinite(n)) return "—";
  const a = Math.abs(n);
  if (a >= 1e9) return `${trim(n / 1e9)}B`;
  if (a >= 1e6) return `${trim(n / 1e6)}M`;
  if (a >= 1e4) return `${trim(n / 1e3)}K`;
  return String(Math.round(n));
}

function trim(x: number): string {
  const t = x.toFixed(x >= 100 ? 0 : x >= 10 ? 1 : 2);
  return t.includes(".") ? t.replace(/\.?0+$/, "") : t;
}

export function fmtUsd(x: number | null | undefined): string {
  if (x === null || x === undefined) return "未知价格";
  if (!Number.isFinite(x)) return "—";
  return x < 0.01 && x > 0 ? `$${x.toFixed(4)}` : `$${x.toFixed(2)}`;
}

/** "mean ± std" (std omitted for a single value). */
export function fmtMeanStd(s: Summary | null, fmt: (x: number) => string): string {
  if (!s) return "—";
  return s.std === null ? fmt(s.mean) : `${fmt(s.mean)} ± ${fmt(s.std)}`;
}

export function fmtRange(s: Summary | null, fmt: (x: number) => string): string {
  if (!s) return "—";
  return s.n < 2 ? fmt(s.min) : `${fmt(s.min)} – ${fmt(s.max)}`;
}

export function fmtTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// --- status ---

export type StatusTone = "wait" | "run" | "ok" | "bad";

/**
 * Status strings: queued | building | running[:<stage>] | scoring | done | failed.
 * Unknown values are shown verbatim.
 */
export function statusLabel(status: string | null | undefined): { text: string; tone: StatusTone } {
  const s = (status ?? "").trim();
  const [head, arg] = s.split(/:(.*)/s, 2) as [string, string | undefined];
  switch (head) {
    case "queued":
      return { text: "排队中", tone: "wait" };
    case "building":
      return { text: "构建中", tone: "run" };
    case "running":
      return { text: arg ? `${arg} 进行中` : "运行中", tone: "run" };
    case "scoring":
      return { text: "打分中", tone: "run" };
    case "done":
      return { text: "完成", tone: "ok" };
    case "failed":
      return { text: "失败", tone: "bad" };
    default:
      return { text: s || "未知", tone: "wait" };
  }
}

export function scoreStatusLabel(st: string | undefined): string {
  switch (st) {
    case "passed":
      return "全部通过";
    case "failed":
      return "部分未通过";
    case "system_error":
      return "平台错误（不计分）";
    case "rejected":
      return "被拒绝";
    default:
      return "未打分";
  }
}
