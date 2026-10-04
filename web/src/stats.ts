// Statistics and formatting for results. Pure functions, unit tested.

import type { Aggregate, Display, Manifest, ReplicaEntry, ScoreFormat, StageEntry, StageScore, UsageTotals } from "./types";

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

// --- scores (result v2; docs/plugins.md §7) ---

/** A stage score in the v2 shape, whatever format it was stored in. */
export interface Score {
  status: "scored" | "error";
  error: "system" | "rejected" | null;
  score: number | null;
  max: number | null;
  passed: boolean | null;
}

/**
 * Read a stored stage score. Old scores (`passed|failed|system_error|rejected`
 * with passed/total) are converted by the fixed table of docs/plugins.md §7.1,
 * the same as crucible-core: passed/failed → scored, score = passed, max =
 * total; system_error / rejected → error.
 */
export function readScore(sc: StageScore | null | undefined): Score | null {
  if (!sc || typeof sc !== "object") return null;
  const num = (x: unknown) => (typeof x === "number" && Number.isFinite(x) ? x : null);
  switch (sc.status) {
    case "passed":
    case "failed":
      return { status: "scored", error: null, score: num(sc.passed) ?? 0, max: num(sc.total) ?? 0, passed: sc.status === "passed" };
    case "system_error":
      return { status: "error", error: "system", score: null, max: null, passed: null };
    case "rejected":
      return { status: "error", error: "rejected", score: null, max: null, passed: null };
    case "scored":
      return { status: "scored", error: null, score: num(sc.score) ?? 0, max: num(sc.max), passed: typeof sc.passed === "boolean" ? sc.passed : null };
    case "error":
      return { status: "error", error: sc.error ?? "system", score: null, max: null, passed: null };
    default:
      return null;
  }
}

/** Display of manifests without a `scoring` snapshot: test counts, ratio total as %. */
export const LEGACY_STAGE: ScoreFormat = { format: "fraction", decimals: 0, direction: "higher" };
export const LEGACY_TOTAL: ScoreFormat = { format: "percent", decimals: 1, direction: "higher", min: 0, max: 1 };

export interface Formats {
  stage: ScoreFormat;
  total: ScoreFormat;
  aggregate: Aggregate;
}

/** The manifest's own snapshot, or the old defaults. */
export function formatsOf(m: Partial<Manifest> | null | undefined): Formats {
  const sc = m?.scoring;
  const d: Display = sc?.display ?? {};
  return {
    stage: d.stage ?? LEGACY_STAGE,
    total: d.total ?? LEGACY_TOTAL,
    aggregate: sc?.aggregate ?? { stages: "ratio" },
  };
}

/** Same rules as crucible-core `ScoreFormat::fmt`. */
export function fmtValue(f: ScoreFormat, v: number | null | undefined, max?: number | null): string {
  if (typeof v !== "number" || !Number.isFinite(v)) return "—";
  const d = Math.min(6, Math.max(0, f.decimals ?? 2));
  if (f.format === "percent") return `${(v * 100).toFixed(d)}%`;
  const s = f.format === "fraction" && typeof max === "number" ? `${v.toFixed(d)}/${max.toFixed(d)}` : v.toFixed(d);
  return f.unit ? `${s} ${f.unit}` : s;
}

/** Whether a stage has a score that says something about the agent. */
export function isScored(s: StageEntry): boolean {
  return readScore(s.score)?.status === "scored";
}

/**
 * The number a stage contributes to statistics: for test counts (fraction
 * display) the share passed, score / max; otherwise the score itself.
 * Null when not scored.
 */
export function stageScore(s: StageEntry, f: ScoreFormat = LEGACY_STAGE): number | null {
  const r = readScore(s.score);
  if (!r || r.status !== "scored") return null;
  if (f.format === "fraction") return r.max && r.max > 0 ? r.score! / r.max : null;
  return r.score;
}

/** How such a stage number is shown: share as %, else by the format. */
export function fmtStageNumber(f: ScoreFormat, x: number): string {
  return f.format === "fraction" ? fmtPct(x) : fmtValue(f, x);
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
  /** By the aggregate (crucible-core `total_score`); null when any stage is unscored. */
  score: number | null;
  /** Σscore and Σmax of the scored stages. */
  sum: number;
  max: number;
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

/** Total of one replica's scored stages by the aggregate (docs/plugins.md §8). */
export function combineStages(vals: { stage: string; score: number; max: number | null }[], a: Aggregate): number | null {
  const mode = a.stages ?? "ratio";
  if (mode === "ratio") {
    const max = vals.reduce((x, v) => x + (v.max ?? 0), 0);
    return max > 0 ? vals.reduce((x, v) => x + v.score, 0) / max : null;
  }
  const xs: { v: number; w: number }[] = [];
  for (const v of vals) {
    let x = v.score;
    if (a.normalize && mode !== "sum") {
      if (!v.max || v.max <= 0) continue;
      x = v.score / v.max;
    }
    xs.push({ v: x, w: a.weights?.[v.stage] ?? 0 });
  }
  if (!xs.length) return null;
  if (mode === "mean") return xs.reduce((s, x) => s + x.v, 0) / xs.length;
  if (mode === "weighted") return xs.reduce((s, x) => s + x.v * x.w, 0);
  return xs.reduce((s, x) => s + x.v, 0);
}

export function replicaTotals(r: ReplicaEntry, agg: Aggregate = { stages: "ratio" }): ReplicaTotals {
  const stages = r.stages ?? [];
  let sum = 0;
  let max = 0;
  let complete = stages.length > 0;
  let wall: number | null = 0;
  let cost: number | null = 0;
  let usage = ZERO;
  const vals: { stage: string; score: number; max: number | null }[] = [];
  for (const s of stages) {
    const sc = readScore(s.score);
    if (sc && sc.status === "scored") {
      sum += sc.score ?? 0;
      max += sc.max ?? 0;
      vals.push({ stage: s.stage, score: sc.score ?? 0, max: sc.max });
    } else complete = false;
    wall = wall !== null && typeof s.wall_s === "number" ? wall + s.wall_s : null;
    cost = cost !== null && typeof s.cost_usd === "number" ? cost + s.cost_usd : null;
    usage = addUsage(usage, usageOf(s));
  }
  return {
    replica: r.replica,
    score: complete ? combineStages(vals, agg) : null,
    sum,
    max,
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

/**
 * total_score from GET /evals, by its display (the manifest's snapshot).
 * Without one it is an old 0–1 ratio (values above 1 are taken as percent).
 */
export function fmtScore(x: number | null | undefined, display?: ScoreFormat | null): string {
  if (typeof x !== "number" || !Number.isFinite(x)) return "—";
  if (display) return fmtValue(display, x);
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

/** Outcome of a stage. A scored stage below the maximum is not a failure. */
export function scoreStatusLabel(sc: StageScore | null | undefined): string {
  const r = readScore(sc);
  if (!r) return "未打分";
  if (r.status === "error") return r.error === "rejected" ? "被拒绝" : "平台错误（不计分）";
  if (r.passed === true) return "通过";
  return "已计分";
}
