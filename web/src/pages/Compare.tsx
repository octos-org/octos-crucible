import { api } from "../api";
import { alignStages, delta, evalStats, type Better, type EvalStats } from "../compare";
import {
  fmtCount,
  fmtDuration,
  fmtMeanStd,
  fmtPct,
  fmtStageNumber,
  fmtTime,
  fmtUsd,
  fmtValue,
  type Summary,
} from "../stats";
import type { ScoreFormat } from "../types";
import type { EvalDetail, Manifest } from "../types";
import { Card, ErrorBox, Loading, shortId, useAsync } from "../ui";

interface Row {
  label: string;
  /** Mean value used for the delta, and the cell text. */
  get: (s: EvalStats) => { value: number | null; text: string };
  better?: Better;
  /** How to show the difference: percentage points, relative %, or in the score's own format. */
  diff?: "pp" | "rel" | "abs";
  /** For `abs`: the score's format. */
  fmt?: ScoreFormat;
}

const pct = (x: number) => fmtPct(x);
const ms = (s: Summary | null, fmt: (x: number) => string) => ({ value: s?.mean ?? null, text: fmtMeanStd(s, fmt) });

const better = (f: ScoreFormat): Better => (f.direction === "lower" ? "down" : "up");
/** A share shown as % differs in pp; anything else in its own unit. */
const diffOf = (f: ScoreFormat, share: boolean): "pp" | "abs" => (share || f.format === "percent" ? "pp" : "abs");

/** Rows follow the first (base) evaluation's display; the others are read by theirs. */
function rows(stages: string[], base: EvalStats): Row[] {
  const { stage, total } = base.formats;
  return [
    {
      label: total.name || "总分",
      get: (s) => ms(s.score, (x) => fmtValue(s.formats.total, x)),
      better: better(total),
      diff: diffOf(total, false),
      fmt: total,
    },
    ...stages.map(
      (name): Row => ({
        label: stage.name ? `${name} · ${stage.name}` : name,
        get: (s) => {
          const st = s.stages[name];
          const f = s.formats.stage;
          const r = ms(st?.score ?? null, (x) => fmtStageNumber(f, x));
          return { ...r, text: f.format === "fraction" && st && st.max > 0 ? `${r.text}（${st.sum}/${st.max}）` : r.text };
        },
        better: better(stage),
        diff: diffOf(stage, stage.format === "fraction"),
        fmt: stage,
      }),
    ),
    { label: "用时", get: (s) => ms(s.wall_s, fmtDuration), better: "down", diff: "rel" },
    { label: "请求数", get: (s) => ms(s.requests, fmtCount), better: "down", diff: "rel" },
    { label: "输入 token", get: (s) => ms(s.prompt_tokens, fmtCount), better: "down", diff: "rel" },
    { label: "输出 token", get: (s) => ms(s.completion_tokens, fmtCount), better: "down", diff: "rel" },
    { label: "缓存 token", get: (s) => ms(s.cached_tokens, fmtCount) },
    { label: "缓存命中率", get: (s) => ms(s.cache_rate, pct), better: "up", diff: "pp" },
    {
      label: "等价花销",
      get: (s) => (s.cost_known ? ms(s.cost_usd, fmtUsd) : { value: null, text: "未知价格" }),
      better: "down",
      diff: "rel",
    },
  ];
}

/** Model use while scoring (judge, interactive agent): its own rows, never added to the above. */
const evalRows: Row[] = [
  { label: "评测模型请求数", get: (s) => ms(s.eval_requests, fmtCount), better: "down", diff: "rel" },
  { label: "评测模型 token", get: (s) => ms(s.eval_tokens, fmtCount), better: "down", diff: "rel" },
  {
    label: "评测模型花销",
    get: (s) => (s.eval_cost_usd ? ms(s.eval_cost_usd, fmtUsd) : { value: null, text: "未知价格" }),
    better: "down",
    diff: "rel",
  },
];

function DeltaText({ base, x, row }: { base: number | null; x: number | null; row: Row }) {
  if (!row.better) return null;
  const d = delta(base, x, row.better);
  if (!d) return null;
  if (d.tone === "same") return <div class="delta same">持平</div>;
  const arrow = d.diff > 0 ? "↑" : "↓";
  const sign = d.diff > 0 ? "+" : "−";
  const text =
    row.diff === "pp"
      ? `${sign}${Math.abs(d.diff * 100).toFixed(1)} pp`
      : row.diff === "abs" && row.fmt
        ? `${sign}${fmtValue({ ...row.fmt, format: "number" }, Math.abs(d.diff))}`
        : d.rel !== null
          ? `${sign}${Math.abs(d.rel * 100).toFixed(1)}%`
          : "";
  return (
    <div class={`delta ${d.tone}`}>
      {arrow} {text}
    </div>
  );
}

export function ComparePage({ ids }: { ids: string[] }) {
  const all = useAsync(() => Promise.all(ids.map((id) => api().evalDetail(id))), [ids.join(",")]);

  const back = (
    <a href="#/evals" class="link small">
      ← 我的评测
    </a>
  );
  if (ids.length < 2)
    return (
      <div class="stack">
        {back}
        <p class="muted">请在“我的评测”中勾选 2～4 个同一题目包的评测再对比。</p>
      </div>
    );
  if (all.loading && !all.data) return <Loading />;
  if (all.error || !all.data)
    return (
      <div class="stack">
        {back}
        <ErrorBox error={all.error ?? new Error("加载失败")} onRetry={all.reload} />
      </div>
    );

  const details: EvalDetail[] = all.data;
  const mans: Partial<Manifest>[] = details.map((d) => d.manifest ?? {});
  const al = alignStages(mans);
  const stats = mans.map((m) => evalStats(m, al.common));
  const table = [...rows(al.common, stats[0]), ...(stats.some((s) => s.eval_any) ? evalRows : [])];
  const anyDropped = al.dropped.some((d) => d.length > 0);

  return (
    <div class="stack">
      {back}
      <h1>评测对比</h1>
      {al.tasksetMismatch && (
        <div class="notice warn">
          题目包不同（{[...new Set(mans.map((m) => m.taskset ?? "?"))].join("、")}），数字不可直接比较，仅按同名阶段对齐。
        </div>
      )}
      {anyDropped && (
        <div class="notice warn">
          阶段不完全相同，只比较共同阶段（{al.common.join("、") || "无"}）。未计入：
          {details
            .map((d, i) => (al.dropped[i].length ? `${shortId(d.eval_id)} 的 ${al.dropped[i].join("、")}` : ""))
            .filter(Boolean)
            .join("；")}
          。
        </div>
      )}
      <Card>
        <div class="table-wrap">
          <table class="compare">
            <thead>
              <tr>
                <th></th>
                {details.map((d, i) => {
                  const m = mans[i];
                  return (
                    <th scope="col">
                      <a href={`#/evals/${d.eval_id}`} class="link">
                        <code>{shortId(d.eval_id)}</code>
                      </a>
                      {i === 0 && <span class="base-tag">基准</span>}
                      <div class="col-meta">
                        <div>
                          {m.agent?.name ?? "—"}
                          {m.agent?.version ? ` v${m.agent.version}` : ""}
                        </div>
                        {m.agent?.commit && (
                          <div>
                            <code>{m.agent.commit.slice(0, 7)}</code>
                          </div>
                        )}
                        <div>{m.model ?? "—"}</div>
                        <div>{stats[i].replicas} 遍</div>
                        <div>{fmtTime(m.created_at)}</div>
                      </div>
                    </th>
                  );
                })}
              </tr>
            </thead>
            <tbody>
              {table.map((row) => {
                const cells = stats.map((s) => row.get(s));
                return (
                  <tr>
                    <th scope="row">{row.label}</th>
                    {cells.map((c, i) => (
                      <td class="num">
                        <div>{c.text}</div>
                        {i > 0 && <DeltaText base={cells[0].value} x={c.value} row={row} />}
                      </td>
                    ))}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
        <p class="hint">
          均值 ± 标准差，按遍计算；按用例计数的阶段括号内为各遍通过数合计；pp 为百分点。差值以第一列为基准，分数按题目包声明的方向判断好坏（绿色更好、红色更差）；缓存命中率升高为好，用时、请求、输入/输出 token、花销降低为好。
        </p>
      </Card>
    </div>
  );
}
