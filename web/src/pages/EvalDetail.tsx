import { useEffect, useState } from "preact/hooks";
import { api } from "../api";
import {
  cacheRate,
  fmtCount,
  fmtDuration,
  fmtMeanStd,
  fmtPct,
  fmtRange,
  fmtTime,
  fmtUsd,
  replicaTotals,
  scoreStatusLabel,
  stageAcross,
  stageNames,
  stageScore,
  summarize,
  totalTokens,
  usageOf,
  type ReplicaTotals,
  type Summary,
} from "../stats";
import type { EvalDetail, ReplicaEntry, StageEntry } from "../types";
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
          <Overview replicas={replicas} />
          <StageTable replicas={replicas} />
          {replicas.map((r) => (
            <ReplicaCard r={r} />
          ))}
        </>
      )}
      {!FINAL.has(d.status) && <p class="muted small">每 15 秒自动刷新。</p>}
    </div>
  );
}

function Overview({ replicas }: { replicas: ReplicaEntry[] }) {
  const totals: ReplicaTotals[] = replicas.map(replicaTotals);
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
          label="总分"
          s={score}
          fmt={pct}
          note={score && score.n < n ? `${score.n}/${n} 遍完整打分` : undefined}
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
      {note && <div class={`tile-sub ${label === "总分" ? "warn-text" : ""}`}>{note}</div>}
    </div>
  );
}

function StageTable({ replicas }: { replicas: ReplicaEntry[] }) {
  const names = stageNames(replicas);
  const multi = replicas.length > 1;
  return (
    <Card title="各阶段">
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>阶段</th>
              <th>分数{multi ? "（均值 ± 标准差）" : ""}</th>
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
              const score = summarize(ss.map(stageScore));
              const us = ss.map(usageOf);
              const costs = ss.map((s) => s.cost_usd);
              const costKnown = costs.every((c) => typeof c === "number");
              return (
                <tr>
                  <th scope="row">{name}</th>
                  <td class="num">{fmtMeanStd(score, (x) => fmtPct(x))}</td>
                  {multi && <td class="num">{fmtRange(score, (x) => fmtPct(x))}</td>}
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

function ReplicaCard({ r }: { r: ReplicaEntry }) {
  const t = replicaTotals(r);
  const stages: StageEntry[] = r.stages ?? [];
  return (
    <Card
      title={`第 ${r.replica} 遍`}
      aside={<span class="score">{t.score !== null ? fmtPct(t.score) : `${t.passed}/${t.total}`}</span>}
    >
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>阶段</th>
              <th>通过</th>
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
              return (
                <tr>
                  <th scope="row">{s.stage}</th>
                  <td class="num">{s.score && s.score.total > 0 ? `${s.score.passed}/${s.score.total}` : "—"}</td>
                  <td class={s.score?.status === "system_error" ? "warn-text" : ""}>{scoreStatusLabel(s.score?.status)}</td>
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
              <td class="num">{t.total ? `${t.passed}/${t.total}` : "—"}</td>
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
    </Card>
  );
}
