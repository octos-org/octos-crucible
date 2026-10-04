import { describe, expect, it } from "vitest";
import {
  cacheRate,
  fmtCount,
  fmtDuration,
  fmtMeanStd,
  fmtPct,
  fmtRange,
  fmtScore,
  fmtUsd,
  replicaTotals,
  stageNames,
  stageScore,
  statusLabel,
  summarize,
} from "../src/stats";
import type { StageEntry } from "../src/types";

const st = (stage: string, passed: number, total: number, extra: Partial<StageEntry> = {}): StageEntry => ({
  stage,
  score: { status: passed === total ? "passed" : "failed", passed, total },
  wall_s: 100,
  usage: { requests: 10, prompt_tokens: 1000, cached_tokens: 250, completion_tokens: 100, reasoning_tokens: 0 },
  cost_usd: 1,
  ...extra,
});

describe("summarize", () => {
  it("mean, sample std, min, max", () => {
    const s = summarize([2, 4, 4, 4, 5, 5, 7, 9])!;
    expect(s.n).toBe(8);
    expect(s.mean).toBe(5);
    expect(s.std).toBeCloseTo(2.138, 3);
    expect([s.min, s.max]).toEqual([2, 9]);
  });

  it("ignores missing values; single value has no std", () => {
    const s = summarize([null, 0.5, undefined])!;
    expect(s).toEqual({ n: 1, mean: 0.5, std: null, min: 0.5, max: 0.5 });
    expect(summarize([null])).toBeNull();
  });
});

describe("stage and replica totals", () => {
  it("system_error is not scored", () => {
    expect(stageScore(st("a", 3, 4))).toBe(0.75);
    expect(stageScore({ ...st("a", 0, 0), score: { status: "system_error", passed: 0, total: 0 } })).toBeNull();
    expect(stageScore({ stage: "a" })).toBeNull();
  });

  it("replica score sums tests; any unscored stage makes it incomplete", () => {
    const t = replicaTotals({ replica: 1, stages: [st("a", 3, 4), st("b", 5, 6)] });
    expect(t.score).toBeCloseTo(0.8);
    expect(t.wall_s).toBe(200);
    expect(t.usage.requests).toBe(20);
    expect(t.cost_usd).toBe(2);
    const partial = replicaTotals({ replica: 2, stages: [st("a", 3, 4), { stage: "b" }] });
    expect(partial.score).toBeNull();
  });

  it("unknown price anywhere makes the total unknown", () => {
    const t = replicaTotals({ replica: 1, stages: [st("a", 1, 1), st("b", 1, 1, { cost_usd: null })] });
    expect(t.cost_usd).toBeNull();
    expect(fmtUsd(t.cost_usd)).toBe("未知价格");
  });

  it("tolerates missing fields", () => {
    const t = replicaTotals({ replica: 1 });
    expect(t).toMatchObject({ score: null, wall_s: null, cost_usd: null });
    expect(replicaTotals({ replica: 1, stages: [{ stage: "x", usage: null }] }).usage.requests).toBe(0);
  });

  it("stage names keep first-seen order", () => {
    expect(stageNames([{ replica: 1, stages: [st("s1", 1, 1)] }, { replica: 2, stages: [st("s1", 1, 1), st("s2", 1, 1)] }])).toEqual([
      "s1",
      "s2",
    ]);
  });

  it("cache rate", () => {
    expect(cacheRate({ requests: 1, prompt_tokens: 1000, cached_tokens: 250, completion_tokens: 0, reasoning_tokens: 0 })).toBe(0.25);
    expect(cacheRate({ requests: 0, prompt_tokens: 0, cached_tokens: 0, completion_tokens: 0, reasoning_tokens: 0 })).toBeNull();
  });
});

describe("formatting", () => {
  it("numbers", () => {
    expect(fmtPct(0.7754)).toBe("77.5%");
    expect(fmtPct(null)).toBe("—");
    expect(fmtScore(0.8)).toBe("80.0%");
    expect(fmtScore(80)).toBe("80.0%");
    expect(fmtCount(4_120_000)).toBe("4.12M");
    expect(fmtCount(61_200)).toBe("61.2K");
    expect(fmtCount(233)).toBe("233");
    expect(fmtCount(159_500)).toBe("160K");
    expect(fmtCount(100_000)).toBe("100K");
    expect(fmtCount(10_000_000)).toBe("10M");
    expect(fmtUsd(2.314)).toBe("$2.31");
    expect(fmtUsd(0.0012)).toBe("$0.0012");
    expect(fmtDuration(1712)).toBe("28 分 32 秒");
    expect(fmtDuration(5011)).toBe("1 小时 23 分");
    expect(fmtDuration(42)).toBe("42 秒");
    expect(fmtDuration(undefined)).toBe("—");
  });

  it("mean ± std and range", () => {
    const s = summarize([0.9, 0.8, 0.7]);
    expect(fmtMeanStd(s, (x) => fmtPct(x))).toBe("80.0% ± 10.0%");
    expect(fmtRange(s, (x) => fmtPct(x))).toBe("70.0% – 90.0%");
    expect(fmtMeanStd(summarize([0.5]), (x) => fmtPct(x))).toBe("50.0%");
    expect(fmtMeanStd(null, String)).toBe("—");
  });

  it("status labels", () => {
    expect(statusLabel("queued").text).toBe("排队中");
    expect(statusLabel("building").text).toBe("构建中");
    expect(statusLabel("running:stage-2").text).toBe("stage-2 进行中");
    expect(statusLabel("running").text).toBe("运行中");
    expect(statusLabel("scoring").text).toBe("打分中");
    expect(statusLabel("done")).toEqual({ text: "完成", tone: "ok" });
    expect(statusLabel("failed").tone).toBe("bad");
    expect(statusLabel("weird").text).toBe("weird");
  });
});
