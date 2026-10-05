import { useEffect, useState } from "preact/hooks";
import { api } from "../api";
import {
  cacheRate,
  evalSlots,
  fmtCount,
  fmtDuration,
  fmtMeanStd,
  fmtPct,
  fmtRange,
  fmtTime,
  fmtStageNumber,
  fmtUsd,
  fmtValue,
  formatsOf,
  readScore,
  replicaTotals,
  scoreStatusLabel,
  stageAcross,
  stageNames,
  stageScore,
  summarize,
  totalTokens,
  usageOf,
  type Formats,
  type ReplicaTotals,
  type Summary,
} from "../stats";
import type { EvalDetail, ReplicaEntry, StageEntry, StageScoreV2 } from "../types";
import { Card, ErrorBox, Loading, StatusBadge, useAsync } from "../ui";

const FINAL = new Set(["done", "failed"]);
const POLL_MS = 15_000;

export function EvalDetailPage({ id }: { id: string }) {
  const detail = useAsync(() => api().evalDetail(id), [id]);
  const [dlError, setDlError] = useState<Error | null>(null);
  const [dlBusy, setDlBusy] = useState(false);

  // Poll while the evaluation is still moving.
  const status = detail.data?.status;
  useEffect(() => {
    if (!status || FINAL.has(status)) return;
    const t = setInterval(detail.reload, POLL_MS);
    return () => clearInterval(t);
  }, [status]);

  if (detail.loading && !detail.data) return <Loading />;
  if (detail.error && !detail.data) return <ErrorBox error={detail.error} onRetry={detail.reload} />;
  const d = detail.data as EvalDetail;
  const m = d.manifest ?? {};
  const replicas: ReplicaEntry[] = Array.isArray(m.replicas) ? m.replicas : [];
  const f = formatsOf(m);

  const download = async () => {
    setDlError(null);
    setDlBusy(true);
    try {
      await api().download(id);
    } catch (e) {
      setDlError(e as Error);
    } finally {
      setDlBusy(false);
    }
  };

  return (
    <div class="stack">
      <a href="#/evals" class="link small">
        ← 我的评测
      </a>
      <div class="row between wrap">
        <h1 class="title-id">
          {m.taskset ?? "评测"} {m.model ? <span class="muted">· {m.model}</span> : null}
        </h1>
        <StatusBadge status={d.status} />
      </div>
      <dl class="meta">
        <div class="span2">
          <dt>编号</dt>
          <dd>
            <code>{d.eval_id}</code>
          </dd>
        </div>
        {m.agent?.name && (
          <div>
            <dt>agent</dt>
            <dd>
              {m.agent.name} {m.agent.version ? `v${m.agent.version}` : ""}
            </dd>
          </div>
        )}
        {m.created_at && (
          <div>
            <dt>提交时间</dt>
            <dd>{fmtTime(m.created_at)}</dd>
          </div>
        )}
        {typeof m.public === "boolean" && (
          <div>
            <dt>分数</dt>
            <dd>{m.public ? "公开" : "不公开"}</dd>
          </div>
        )}
        {d.run_url && (
          <div>
            <dt>运行记录</dt>
            <dd>
              <a href={d.run_url} target="_blank" rel="noopener noreferrer" class="link">
                GitHub Actions ↗
              </a>
            </dd>
          </div>
        )}
      </dl>

      {d.status === "done" && m.download && (
        <Card title="下载产出">
          <p class="muted small">
            产出和日志打包为 AES-256 加密的 zip，用提交时设置的下载密码解压。请使用 7-Zip、Keka 或 The
            Unarchiver，系统自带的解压工具打不开。
          </p>
          <button class="btn" onClick={download} disabled={dlBusy}>
            {dlBusy ? "准备下载…" : "下载产出"}
          </button>
          {dlError && <ErrorBox error={dlError} />}
        </Card>
      )}

      {replicas.length === 0 ? (
        <p class="muted">{FINAL.has(d.status) ? "没有结果数据。" : "还没有结果，完成第一个阶段后会显示在这里。"}</p>
      ) : (
        <>
          <Overview replicas={replicas} f={f} complete={d.complete !== false} />
          <StageTable replicas={replicas} f={f} />
          {replicas.map((r) => (
            <ReplicaCard r={r} f={f} />
          ))}
        </>
      )}
      {!FINAL.has(d.status) && <p class="muted small">每 15 秒自动刷新。</p>}
    </div>
  );
}

function Overview({ replicas, f, complete }: { replicas: ReplicaEntry[]; f: Formats; complete: boolean }) {
  const totals: ReplicaTotals[] = replicas.map((r) => replicaTotals(r, f.aggregate));
  const score = summarize(totals.map((t) => t.score));
  const wall = summarize(totals.map((t) => t.wall_s));
  const tokens = summarize(totals.map((t) => totalTokens(t.usage)));
  const reqs = summarize(totals.map((t) => t.usage.requests));
  const cache = summarize(totals.map((t) => cacheRate(t.usage)));
  const costKnown = totals.every((t) => t.cost_usd !== null);
  const cost = costKnown ? summarize(totals.map((t) => t.cost_usd)) : null;
  const n = replicas.length;

  return (
    <Card title={n > 1 ? `总览（${n} 遍）` : "总览"}>
      <div class="tiles">
        <Tile
          label={complete ? f.total.name || "总分" : "已跑阶段合计"}
          s={score}
          fmt={(x) => fmtValue(f.total, x)}
          note={
            !complete
              ? "只跑了部分阶段，不计入排行榜"
              : score && score.n < n
                ? `${score.n}/${n} 遍完整打分`
                : undefined
          }
        />
        <Tile label="用时" s={wall} fmt={fmtDuration} />
        <Tile label="token" s={tokens} fmt={fmtCount} note={reqs ? `请求 ${fmtMeanStd(reqs, fmtCount)}` : undefined} />
        <Tile label="缓存命中率" s={cache} fmt={pct} />
        {costKnown ? <Tile label="等价花销" s={cost} fmt={fmtUsd} /> : <Tile label="等价花销" text="未知价格" />}
      </div>
      {n > 1 && <p class="hint">均值 ± 标准差（样本），按遍计算。</p>}
    </Card>
  );
}

const pct = (x: number) => fmtPct(x);

/** Mean as the headline; ± std and min–max underneath when there are several replicas. */
function Tile({
  label,
  s,
  fmt,
  text,
  note,
}: {
  label: string;
  s?: Summary | null;
  fmt?: (x: number) => string;
  text?: string;
  note?: string;
}) {
  const multi = s && s.n > 1 && fmt;
  return (
    <div class="tile">
      <div class="tile-label">{label}</div>
      <div class="tile-value">{text ?? (s && fmt ? fmt(s.mean) : "—")}</div>
      {multi && s.std !== null && <div class="tile-sub">± {fmt(s.std)}</div>}
      {multi && <div class="tile-sub">{fmtRange(s, fmt)}</div>}
      {note && <div class="tile-sub warn-text">{note}</div>}
    </div>
  );
}

function StageTable({ replicas, f }: { replicas: ReplicaEntry[]; f: Formats }) {
  const fmt = (x: number) => fmtStageNumber(f.stage, x);
  const names = stageNames(replicas);
  const multi = replicas.length > 1;
  return (
    <Card title="各阶段">
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>阶段</th>
              <th>
                {f.stage.name || "分数"}
                {multi ? "（均值 ± 标准差）" : ""}
              </th>
              {multi && <th>最低–最高</th>}
              <th>用时</th>
              <th>请求</th>
              <th>token</th>
              <th>缓存命中</th>
              <th>等价花销</th>
            </tr>
          </thead>
          <tbody>
            {names.map((name) => {
              const ss = stageAcross(replicas, name);
              const score = summarize(ss.map((s) => stageScore(s, f.stage)));
              const us = ss.map(usageOf);
              const costs = ss.map((s) => s.cost_usd);
              const costKnown = costs.every((c) => typeof c === "number");
              return (
                <tr>
                  <th scope="row">{name}</th>
                  <td class="num">{fmtMeanStd(score, fmt)}</td>
                  {multi && <td class="num">{fmtRange(score, fmt)}</td>}
                  <td class="num">{fmtMeanStd(summarize(ss.map((s) => s.wall_s)), fmtDuration)}</td>
                  <td class="num">{fmtMeanStd(summarize(us.map((u) => u.requests)), fmtCount)}</td>
                  <td class="num">{fmtMeanStd(summarize(us.map(totalTokens)), fmtCount)}</td>
                  <td class="num">{fmtMeanStd(summarize(us.map(cacheRate)), (x) => fmtPct(x))}</td>
                  <td class="num">{costKnown ? fmtMeanStd(summarize(costs), fmtUsd) : "未知价格"}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

function ReplicaCard({ r, f }: { r: ReplicaEntry; f: Formats }) {
  const t = replicaTotals(r, f.aggregate);
  const stages: StageEntry[] = r.stages ?? [];
  const counted = f.stage.format === "fraction";
  const withItems = stages.filter((s) => (s.score as StageScoreV2 | null | undefined)?.items?.length);
  return (
    <Card
      title={`第 ${r.replica} 遍`}
      aside={<span class="score">{t.score !== null ? fmtValue(f.total, t.score) : counted ? `${t.sum}/${t.max}` : "—"}</span>}
    >
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>阶段</th>
              <th>{f.stage.name || (counted ? "通过" : "分数")}</th>
              <th>结果</th>
              <th>用时</th>
              <th>请求</th>
              <th>输入 token</th>
              <th>输出 token</th>
              <th>缓存命中</th>
              <th>等价花销</th>
            </tr>
          </thead>
          <tbody>
            {stages.map((s) => {
              const u = usageOf(s);
              const sc = readScore(s.score);
              const shown = sc?.status === "scored" && !(counted && !sc.max) ? fmtValue(f.stage, sc.score, sc.max) : "—";
              return (
                <tr>
                  <th scope="row">{s.stage}</th>
                  <td class="num">{shown}</td>
                  <td class={sc?.status === "error" ? "warn-text" : ""}>{scoreStatusLabel(s.score)}</td>
                  <td class="num">{fmtDuration(s.wall_s)}</td>
                  <td class="num">{fmtCount(u.requests)}</td>
                  <td class="num">{fmtCount(u.prompt_tokens)}</td>
                  <td class="num">{fmtCount(u.completion_tokens)}</td>
                  <td class="num">{fmtPct(cacheRate(u))}</td>
                  <td class="num">{fmtUsd(s.cost_usd)}</td>
                </tr>
              );
            })}
          </tbody>
          <tfoot>
            <tr>
              <th scope="row">合计</th>
              <td class="num">{t.score !== null ? fmtValue(f.total, t.score) : counted && t.max ? `${t.sum}/${t.max}` : "—"}</td>
              <td></td>
              <td class="num">{fmtDuration(t.wall_s)}</td>
              <td class="num">{fmtCount(t.usage.requests)}</td>
              <td class="num">{fmtCount(t.usage.prompt_tokens)}</td>
              <td class="num">{fmtCount(t.usage.completion_tokens)}</td>
              <td class="num">{fmtPct(cacheRate(t.usage))}</td>
              <td class="num">{fmtUsd(t.cost_usd)}</td>
            </tr>
          </tfoot>
        </table>
      </div>
      {t.eval.any && (
        <div class="table-wrap">
          <p class="muted small">
            评测阶段的模型用量（交互运行中的 agent、模型评判），用的是提交者的 key，单独计算，不含在上表里。
          </p>
          <table>
            <thead>
              <tr>
                <th>阶段</th>
                <th>用途</th>
                <th>请求</th>
                <th>输入 token</th>
                <th>输出 token</th>
                <th>等价花销</th>
              </tr>
            </thead>
            <tbody>
              {stages.flatMap((s) =>
                evalSlots(s).map(([label, u, c]) => (
                  <tr>
                    <th scope="row">{s.stage}</th>
                    <td>{label}</td>
                    <td class="num">{fmtCount(u.requests)}</td>
                    <td class="num">{fmtCount(u.prompt_tokens)}</td>
                    <td class="num">{fmtCount(u.completion_tokens)}</td>
                    <td class="num">{fmtUsd(c)}</td>
                  </tr>
                )),
              )}
            </tbody>
          </table>
        </div>
      )}
      {withItems.map((s) => (
        <div class="table-wrap">
          <p class="muted small">{s.stage} 分项</p>
          <table>
            <tbody>
              {(s.score as StageScoreV2).items!.map((it) => (
                <tr>
                  <th scope="row">{it.name}</th>
                  <td class="num">
                    {typeof it.score === "number"
                      ? fmtValue({ ...f.stage, format: "number" }, it.score)
                      : it.passed === true
                        ? "通过"
                        : it.passed === false
                          ? "未通过"
                          : "—"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ))}
    </Card>
  );
}
