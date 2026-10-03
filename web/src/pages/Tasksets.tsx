import { api } from "../api";
import { fmtDuration } from "../stats";
import { Card, ErrorBox, Loading, useAsync } from "../ui";

export function Tasksets() {
  const ts = useAsync(() => api().tasksets());
  return (
    <div class="stack">
      <h1>题目包</h1>
      <p class="muted small">每个题目包分若干阶段，agent 在同一工作目录里依次完成；每阶段结束后单独打分。测试材料不公开。</p>
      {ts.loading && <Loading />}
      {ts.error && <ErrorBox error={ts.error} onRetry={ts.reload} />}
      {ts.data?.length === 0 && <p class="muted">暂无题目包。</p>}
      {ts.data?.map((t) => (
        <Card
          title={t.name}
          aside={
            <span class="muted small">
              v{t.version} · 限时合计 {fmtDuration(t.stages.reduce((a, s) => a + (s.time_limit_s || 0), 0))}
            </span>
          }
        >
          <div class="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>#</th>
                  <th>阶段</th>
                  <th>限时</th>
                  <th>测试数</th>
                </tr>
              </thead>
              <tbody>
                {t.stages.map((s, i) => (
                  <tr>
                    <td class="num">{i + 1}</td>
                    <th scope="row">{s.name}</th>
                    <td class="num">{fmtDuration(s.time_limit_s)}</td>
                    <td class="num">{s.total}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Card>
      ))}
    </div>
  );
}
