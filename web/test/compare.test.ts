import { describe, expect, it } from "vitest";
import { alignStages, delta, evalStats, parseIds } from "../src/compare";
import type { Manifest, StageEntry } from "../src/types";

const st = (stage: string, passed: number, total: number, wall = 100, cost: number | null = 1): StageEntry => ({
  stage,
  score: { status: passed === total ? "passed" : "failed", passed, total },
  wall_s: wall,
  usage: { requests: 10, prompt_tokens: 1000, cached_tokens: 500, completion_tokens: 100, reasoning_tokens: 0 },
  cost_usd: cost,
});

const man = (taskset: string, ...reps: StageEntry[][]): Partial<Manifest> => ({
  taskset,
  replicas: reps.map((stages, i) => ({ replica: i + 1, stages })),
});

describe("delta", () => {
  it("higher is better for scores", () => {
    expect(delta(0.5, 0.6, "up")).toMatchObject({ tone: "good" });
    expect(delta(0.5, 0.6, "up")!.diff).toBeCloseTo(0.1);
    expect(delta(0.6, 0.5, "up")!.tone).toBe("bad");
  });

  it("lower is better for time, tokens, cost", () => {
    expect(delta(200, 150, "down")).toEqual({ diff: -50, rel: -0.25, tone: "good" });
    expect(delta(200, 250, "down")).toEqual({ diff: 50, rel: 0.25, tone: "bad" });
  });

  it("equal values, zero base, missing sides", () => {
    expect(delta(1, 1, "up")).toEqual({ diff: 0, rel: 0, tone: "same" });
    expect(delta(0.1 + 0.2, 0.3, "down")!.tone).toBe("same");
    expect(delta(0, 5, "down")).toEqual({ diff: 5, rel: null, tone: "bad" });
    expect(delta(null, 5, "up")).toBeNull();
    expect(delta(5, undefined, "up")).toBeNull();
  });
});

describe("alignStages", () => {
  it("keeps stages common to all, in the first one's order", () => {
    const a = man("t", [st("s1", 1, 1), st("s2", 1, 1), st("s3", 1, 1)]);
    const b = man("t", [st("s2", 1, 1), st("s1", 1, 1)]);
    expect(alignStages([a, b])).toEqual({ tasksetMismatch: false, common: ["s1", "s2"], dropped: [["s3"], []] });
  });

  it("flags different task sets", () => {
    const al = alignStages([man("a", [st("s1", 1, 1)]), man("b", [st("s1", 1, 1)])]);
    expect(al.tasksetMismatch).toBe(true);
    expect(al.common).toEqual(["s1"]);
  });

  it("tolerates missing replicas", () => {
    expect(alignStages([{}, man("t", [st("s1", 1, 1)])])).toMatchObject({ common: [], dropped: [[], ["s1"]] });
  });
});

describe("evalStats", () => {
  it("counts only the given stages", () => {
    const m = man("t", [st("s1", 3, 4, 100), st("s2", 0, 4, 900)], [st("s1", 4, 4, 300), st("s2", 0, 4, 900)]);
    const s = evalStats(m, ["s1"]);
    expect(s.replicas).toBe(2);
    expect(s.score!.mean).toBeCloseTo(0.875);
    expect(s.stages.s1).toMatchObject({ sum: 7, max: 8 });
    expect(s.stages.s1.score!.mean).toBeCloseTo(0.875);
    expect(s.wall_s!.mean).toBe(200);
    expect(s.requests!.mean).toBe(10);
    expect(s.cache_rate!.mean).toBe(0.5);
    expect(s.cost_usd!.mean).toBe(1);
  });

  it("unknown price makes cost unknown", () => {
    const s = evalStats(man("t", [st("s1", 1, 1, 100, null)]), ["s1"]);
    expect(s.cost_known).toBe(false);
    expect(s.cost_usd).toBeNull();
  });
});

describe("parseIds", () => {
  it("dedups, filters, caps at 4", () => {
    expect(parseIds("ids=a,b,a,,c%20,d,e")).toEqual(["a", "b", "c", "d"]);
    expect(parseIds("ids=a,<x>")).toEqual(["a"]);
    expect(parseIds("")).toEqual([]);
  });
});

describe("result v2 with a display snapshot", () => {
  const v2 = (stage: string, score: number): StageEntry => ({ stage, score: { status: "scored", score }, wall_s: 1, cost_usd: 0 });
  const astro = (...scores: number[]): Partial<Manifest> => ({
    taskset: "astro-practice",
    scoring: {
      aggregate: { stages: "sum" },
      display: { stage: { name: "观测得分", decimals: 2 }, total: { name: "四卡总分", decimals: 2 } },
      plugins: [],
    },
    replicas: [{ replica: 1, stages: scores.map((x, i) => v2(`l${i + 1}`, x)) }],
  });

  it("sums raw scores and keeps them unscaled", () => {
    const s = evalStats(astro(4458.556, -10), ["l1", "l2"]);
    expect(s.score!.mean).toBeCloseTo(4448.556);
    expect(s.stages.l1.score!.mean).toBe(4458.556);
    expect(s.formats.total.name).toBe("四卡总分");
  });

  it("direction decides good and bad", () => {
    expect(delta(4000, 4458.56, "up")!.tone).toBe("good");
    expect(delta(4000, 4458.56, "down")!.tone).toBe("bad");
  });
});

