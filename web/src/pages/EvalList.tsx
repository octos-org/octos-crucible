import { useState } from "preact/hooks";
import { api } from "../api";
import { MAX_COMPARE } from "../compare";
import { fmtScore, fmtTime } from "../stats";
import type { EvalSummary } from "../types";
import { ErrorBox, Loading, StatusBadge, shortId, useAsync } from "../ui";
import { CliTokens } from "./Tokens";

export function EvalList() {
  const evals = useAsync(() => api().evals());
  const [picked, setPicked] = useState<string[]>([]);

  const list = evals.data ?? [];
  const byId = new Map(list.map((e) => [e.eval_id, e]));
  const pickedTaskset = picked.length ? byId.get(picked[0])?.taskset : undefined;
  const canPick = (e: EvalSummary) =>
    e.status === "done" && (pickedTaskset === undefined || e.taskset === pickedTaskset) && picked.length < MAX_COMPARE;
  const toggle = (id: string) =>
    setPicked((p) => (p.includes(id) ? p.filter((x) => x !== id) : [...p, id]));

  return (
    <div class="stack">
      <div class="row between">
        <h1>我的评测</h1>
        <a class="btn primary" href="#/submit">
          提交评测
        </a>
      </div>
      {evals.loading && !evals.data && <Loading />}
      {evals.error && <ErrorBox error={evals.error} onRetry={evals.reload} />}
      {evals.data && evals.data.length === 0 && (
        <div class="empty">
          <p>还没有评测。</p>
          <a class="btn" href="#/submit">
            提交第一个
          </a>
        </div>
      )}
      {list.length > 0 && (
        <>
          <div class="row between wrap compare-bar">
            <span class="muted small">
              {picked.length
                ? `已选 ${picked.length}/${MAX_COMPARE}（${pickedTaskset}），第一个为基准`
                : "勾选 2～4 个同一题目包的已完成评测进行对比"}
            </span>
            <span class="row">
              {picked.length > 0 && (
                <button type="button" class="link small" onClick={() => setPicked([])}>
                  清除
                </button>
              )}
              <a
                class="btn"
                href={picked.length >= 2 ? `#/compare?ids=${picked.join(",")}` : undefined}
                aria-disabled={picked.length < 2}
              >
                对比
              </a>
            </span>
          </div>
          <ul class="list">
            {[...list]
              .sort((a, b) => (b.created_at ?? "").localeCompare(a.created_at ?? ""))
              .map((e) => {
                const on = picked.includes(e.eval_id);
                return (
                  <li class="pick-row">
                    <label class="pick" title={e.status === "done" ? "选择对比" : "未完成，不能对比"}>
                      <input
                        type="checkbox"
                        checked={on}
                        disabled={!on && !canPick(e)}
                        onChange={() => toggle(e.eval_id)}
                        aria-label={`选择 ${shortId(e.eval_id)} 进行对比`}
                      />
                    </label>
                    <a href={`#/evals/${e.eval_id}`} class="item">
                      <div class="item-main">
                        <div class="item-title">
                          <span>{e.taskset}</span>
                          <span class="muted">·</span>
                          <span>{e.mode === "app" ? "产出打分" : e.model || "—"}</span>
                        </div>
                        <div class="muted small">
                          {fmtTime(e.created_at)} · <code>{shortId(e.eval_id)}</code>
                        </div>
                      </div>
                      <div class="item-side">
                        {typeof e.total_score === "number" && <span class="score">{fmtScore(e.total_score, e.display)}</span>}
                        <StatusBadge status={e.status} />
                      </div>
                    </a>
                  </li>
                );
              })}
          </ul>
        </>
      )}
      <CliTokens />
    </div>
  );
}
