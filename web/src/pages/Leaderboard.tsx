import { useState } from "preact/hooks";
import { api } from "../api";
import { LEGACY_STAGE, fmtDuration, fmtScore, fmtTime, fmtUsd, fmtValue } from "../stats";
import type { Leaderboard, LeaderboardEntry } from "../types";
import { ErrorBox, Loading, useAsync } from "../ui";

/** Public leaderboards: one table per taskset, best eval of each (user, agent). */
export function LeaderboardPage({ taskset }: { taskset: string | null }) {
  const list = useAsync(() => api().leaderboards());
  const names = list.data?.map((b) => b.taskset) ?? [];
  const current = taskset ?? names[0] ?? null;
  if (current && !names.includes(current)) names.push(current);

  return (
    <div class="stack">
      <h1>排行榜</h1>
      <p class="muted small">
        只列出提交时选择公开分数的已完成评测；同一用户的同一个 agent 只取最好的一次。跑完全部阶段的评测进完整榜，只跑了部分阶段的按所跑阶段分开排。显示 GitHub 用户名、agent、模型和分数，产出和日志始终不公开。
      </p>
      {list.loading && !list.data && <Loading />}
      {list.error && <ErrorBox error={list.error} onRetry={list.reload} />}
      {list.data && names.length === 0 && <p class="muted">还没有公开的成绩。提交时勾选“公开分数”即可上榜。</p>}
      {names.length > 0 && (
        <label class="field">
          <span class="small muted">题目包</span>
          <select
            value={current ?? ""}
            onChange={(e) => {
              location.hash = `#/leaderboard/${(e.target as HTMLSelectElement).value}`;
            }}
          >
            {names.map((n) => (
              <option value={n}>{n}</option>
            ))}
          </select>
        </label>
      )}
      {current && <Board taskset={current} />}
    </div>
  );
}

function Board({ taskset }: { taskset: string }) {
  const b = useAsync(() => api().leaderboard(taskset), [taskset]);
  if (b.loading && !b.data) return <Loading />;
  if (b.error) return <ErrorBox error={b.error} onRetry={b.reload} />;
  const board = b.data!;
  const partial = board.partial ?? [];
  if (board.entries.length === 0 && partial.length === 0) return <p class="muted">这个题目包还没有公开的成绩。</p>;
  return (
    <>
      <p class="muted small">
        按总分{board.direction === "lower" ? "从低到高（越低越好）" : "从高到低（越高越好）"}排序；分数相同名次并列，先达到的排前。点一行查看详情。
      </p>
      <h2>完整评测</h2>
      <p class="muted small">跑完题目包全部阶段的评测。</p>
      {board.entries.length === 0 ? (
        <p class="muted">还没有跑完全部阶段的公开成绩。</p>
      ) : (
        <Table board={board} entries={board.entries} totalLabel={board.display?.name || "总分"} />
      )}
      {partial.map((g) => (
        <>
          <h2>只跑了部分阶段：{g.stages.join("、")}</h2>
          <p class="muted small">只和跑了同样阶段的评测比较，不进入上面的完整榜。</p>
          <Table board={board} entries={g.entries} totalLabel="已跑阶段合计" />
        </>
      ))}
    </>
  );
}

function Table({ board, entries, totalLabel }: { board: Leaderboard; entries: LeaderboardEntry[]; totalLabel: string }) {
  const [open, setOpen] = useState<string | null>(null);
  const stages = entries[0].stages.map((s) => s.stage);
  const cols = 8 + stages.length;
  return (
    <div class="table-wrap">
      <table class="board">
        <thead>
          <tr>
            <th>#</th>
            <th>用户</th>
            <th>agent</th>
            <th>模型</th>
            <th>{totalLabel}</th>
            {stages.map((s) => (
              <th>{s}</th>
            ))}
            <th>遍数</th>
            <th>用时</th>
            <th>花销</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((e) => {
            const on = open === e.eval_id;
            return (
              <>
                <tr
                  class={on ? "clickable open" : "clickable"}
                  tabIndex={0}
                  aria-expanded={on}
                  onClick={() => setOpen(on ? null : e.eval_id)}
                  onKeyDown={(k) => {
                    if (k.key === "Enter" || k.key === " ") {
                      k.preventDefault();
                      setOpen(on ? null : e.eval_id);
                    }
                  }}
                >
                  <td class="num">{e.rank}</td>
                  <th scope="row">{e.login}</th>
                  <td>{e.agent}</td>
                  <td>{e.model || "—"}</td>
                  <td class="num strong">{fmtScore(e.total_score, board.display)}</td>
                  {stages.map((s) => (
                    <td class="num">{stageCell(board, e, s)}</td>
                  ))}
                  <td class="num">{e.replicas}</td>
                  <td class="num">{fmtDuration(e.wall_s)}</td>
                  <td class="num">{fmtUsd(e.cost_usd)}</td>
                </tr>
                {on && (
                  <tr class="detail">
                    <td colSpan={cols}>
                      <Detail board={board} e={e} totalLabel={totalLabel} />
                    </td>
                  </tr>
                )}
              </>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function stageCell(board: Leaderboard, e: LeaderboardEntry, stage: string): string {
  const s = e.stages.find((x) => x.stage === stage);
  return fmtValue(board.stage_display ?? LEGACY_STAGE, s?.score, s?.max);
}

function Detail({ board, e, totalLabel }: { board: Leaderboard; e: LeaderboardEntry; totalLabel: string }) {
  const row = (k: string, v: string) => (
    <>
      <dt>{k}</dt>
      <dd>{v}</dd>
    </>
  );
  return (
    <dl class="kv">
      {row("名次", String(e.rank))}
      {row("用户", e.login)}
      {row("agent", `${e.agent} ${e.agent_version}`)}
      {row("模型", e.model || "—")}
      {row(totalLabel, fmtScore(e.total_score, board.display))}
      {e.stages.map((s) => row(s.stage, fmtValue(board.stage_display ?? LEGACY_STAGE, s.score, s.max)))}
      {row("遍数", String(e.replicas))}
      {row("用时（每遍平均）", fmtDuration(e.wall_s))}
      {row("等价花销（每遍平均）", fmtUsd(e.cost_usd))}
      {row("评测时间", fmtTime(e.created_at))}
      {row("评测 ID", e.eval_id)}
    </dl>
  );
}
