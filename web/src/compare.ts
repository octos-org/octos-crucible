// Side-by-side comparison of several evaluations of the same task set.
// Pure functions, unit tested; the page only formats what these return.

import {
  cacheRate,
  replicaTotals,
  stageAcross,
  stageNames,
  stageScore,
  summarize,
  type Summary,
} from "./stats";
import type { Manifest, ReplicaEntry } from "./types";

export const MAX_COMPARE = 4;

export interface Alignment {
  /** Task sets differ between the evaluations. */
  tasksetMismatch: boolean;
  /** Stages present in every evaluation, in the first one's order. */
  common: string[];
  /** Per evaluation, the stages left out because another evaluation lacks them. */
  dropped: string[][];
}

export function alignStages(ms: Partial<Manifest>[]): Alignment {
  const names = ms.map((m) => stageNames(m.replicas ?? []));
  const common = (names[0] ?? []).filter((n) => names.every((ns) => ns.includes(n)));
  return {
    tasksetMismatch: new Set(ms.map((m) => m.taskset)).size > 1,
    common,
    dropped: names.map((ns) => ns.filter((n) => !common.includes(n))),
  };
}

export interface StageStats {
  score: Summary | null;
  passed: number;
  total: number;
}

export interface EvalStats {
  replicas: number;
  score: Summary | null;
  stages: Record<string, StageStats>;
  wall_s: Summary | null;
  requests: Summary | null;
  prompt_tokens: Summary | null;
  completion_tokens: Summary | null;
  cached_tokens: Summary | null;
  cache_rate: Summary | null;
  /** null when any replica's price is unknown. */
  cost_usd: Summary | null;
  cost_known: boolean;
}

/** Statistics over the given stages only, per replica then mean ± std across replicas. */
export function evalStats(m: Partial<Manifest>, stages: string[]): EvalStats {
  const reps: ReplicaEntry[] = (m.replicas ?? []).map((r) => ({
    replica: r.replica,
    stages: (r.stages ?? []).filter((s) => stages.includes(s.stage)),
  }));
  const totals = reps.map(replicaTotals);
  const per: Record<string, StageStats> = {};
  for (const name of stages) {
    const ss = stageAcross(reps, name);
    let passed = 0;
    let total = 0;
    for (const s of ss) {
      if (stageScore(s) !== null) {
        passed += s.score!.passed;
        total += s.score!.total;
      }
    }
    per[name] = { score: summarize(ss.map(stageScore)), passed, total };
  }
  const cost_known = totals.length > 0 && totals.every((t) => t.cost_usd !== null);
  return {
    replicas: reps.length,
    score: summarize(totals.map((t) => t.score)),
    stages: per,
    wall_s: summarize(totals.map((t) => t.wall_s)),
    requests: summarize(totals.map((t) => t.usage.requests)),
    prompt_tokens: summarize(totals.map((t) => t.usage.prompt_tokens)),
    completion_tokens: summarize(totals.map((t) => t.usage.completion_tokens)),
    cached_tokens: summarize(totals.map((t) => t.usage.cached_tokens)),
    cache_rate: summarize(totals.map((t) => cacheRate(t.usage))),
    cost_usd: cost_known ? summarize(totals.map((t) => t.cost_usd)) : null,
    cost_known,
  };
}

export type Better = "up" | "down";
export type Tone = "good" | "bad" | "same";

export interface Delta {
  /** x − base. */
  diff: number;
  /** diff / |base|; null when base is 0. */
  rel: number | null;
  tone: Tone;
}

/** Change of `x` against `base`; null when either side is missing. */
export function delta(base: number | null | undefined, x: number | null | undefined, better: Better): Delta | null {
  if (typeof base !== "number" || typeof x !== "number" || !Number.isFinite(base) || !Number.isFinite(x)) return null;
  const diff = x - base;
  const same = Math.abs(diff) <= 1e-9 * Math.max(1, Math.abs(base));
  const tone: Tone = same ? "same" : (diff > 0) === (better === "up") ? "good" : "bad";
  return { diff: same ? 0 : diff, rel: base !== 0 ? diff / Math.abs(base) : null, tone };
}

/** `?ids=a,b` from a hash route, de-duplicated, at most MAX_COMPARE. */
export function parseIds(q: string): string[] {
  const raw = new URLSearchParams(q).get("ids") ?? "";
  const ids = raw.split(",").map((s) => s.trim()).filter((s) => /^[\w-]+$/.test(s));
  return [...new Set(ids)].slice(0, MAX_COMPARE);
}
