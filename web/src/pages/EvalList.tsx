import { api } from "../api";
import { fmtScore, fmtTime } from "../stats";
import { ErrorBox, Loading, StatusBadge, shortId, useAsync } from "../ui";

export function EvalList() {
  const evals = useAsync(() => api().evals());

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
      {evals.data && evals.data.length > 0 && (
        <ul class="list">
          {[...evals.data]
            .sort((a, b) => (b.created_at ?? "").localeCompare(a.created_at ?? ""))
            .map((e) => (
              <li>
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
                    {typeof e.total_score === "number" && <span class="score">{fmtScore(e.total_score)}</span>}
                    <StatusBadge status={e.status} />
                  </div>
                </a>
              </li>
            ))}
        </ul>
      )}
    </div>
  );
}
