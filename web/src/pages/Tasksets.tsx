import { useState } from "preact/hooks";
import { api, getToken } from "../api";
import { fmtDuration } from "../stats";
import { submitTaskset } from "../submit";
import type { TaskSet } from "../types";
import { Card, ErrorBox, Loading, useAsync } from "../ui";

const STATUS: Record<string, string> = { packing: "登记中", ready: "可用", failed: "未通过" };

export function Tasksets() {
  const loggedIn = !!getToken();
  const me = useAsync(() => (loggedIn ? api().me() : Promise.resolve(null)), [loggedIn]);
  const admin = !!me.data?.is_admin;
  const [all, setAll] = useState(false);
  const ts = useAsync(() => api().tasksets(admin && all), [admin, all]);
  return (
    <div class="stack">
      <h1>题目包</h1>
      <p class="muted small">题目包定义题目和打分方式：可以是写网站、与模拟器交互等任何能自动打分的任务。每个题目包分若干阶段，agent 依次完成，每阶段结束后单独打分。测试材料不公开。</p>
      {loggedIn && <UploadTaskset onDone={ts.reload} />}
      {admin && (
        <label class="small">
          <input type="checkbox" checked={all} onChange={(e) => setAll((e.target as HTMLInputElement).checked)} /> 显示所有用户上传的题目包（管理员）
        </label>
      )}
      {ts.loading && <Loading />}
      {ts.error && <ErrorBox error={ts.error} onRetry={ts.reload} />}
      {ts.data?.length === 0 && <p class="muted">暂无题目包。</p>}
      {ts.data?.map((t) => <TasksetCard t={t} admin={admin} onChange={ts.reload} />)}
    </div>
  );
}

function TasksetCard({ t, admin, onChange }: { t: TaskSet; admin: boolean; onChange: () => void }) {
  const uploaded = !!t.status;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const toggle = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().setTasksetPublic(t.name, !t.public);
      onChange();
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(false);
    }
  };
  const aside = uploaded ? (
    <span class="muted small">
      上传 · {t.owner_login} · {t.public ? "公开" : "私有"} · {STATUS[t.status!] ?? t.status}
      {t.status === "ready" && <> · 限时合计 {fmtDuration(t.stages.reduce((a, s) => a + (s.time_limit_s || 0), 0))}</>}
    </span>
  ) : (
    <span class="muted small">
      v{t.version} · 限时合计 {fmtDuration(t.stages.reduce((a, s) => a + (s.time_limit_s || 0), 0))}
    </span>
  );
  return (
    <Card title={uploaded ? `${t.title ?? t.name}（${t.name}）` : t.name} aside={aside}>
      {t.status === "failed" && <p class="notice bad">未通过检查：{t.error}</p>}
      {t.status === "packing" && <p class="muted small">正在解密、检查并封存，通常几分钟；刷新页面查看结果。</p>}
      {t.display?.stage?.name && (
        <p class="muted small">
          计分：{t.display.stage.name}
          {t.display.stage.unit ? `（${t.display.stage.unit}）` : ""}，{t.display.stage.direction === "lower" ? "越低越好" : "越高越好"}
          {t.display.total?.name ? `；总分：${t.display.total.name}` : ""}
        </p>
      )}
      {t.stages.length > 0 && (
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
                  <td class="num">{s.total ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {admin && uploaded && t.status === "ready" && (
        <p>
          <button type="button" class="link" disabled={busy} onClick={toggle}>
            {t.public ? "改为私有" : "设为公开"}
          </button>
        </p>
      )}
      {error && <ErrorBox error={error} />}
    </Card>
  );
}

function UploadTaskset({ onDone }: { onDone: () => void }) {
  const [file, setFile] = useState<File | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const submit = async (e: Event) => {
    e.preventDefault();
    if (!file) return;
    setBusy(true);
    setError(null);
    setMsg(null);
    try {
      const res = await submitTaskset(api(), new Uint8Array(await file.arrayBuffer()));
      setMsg(`已提交登记：${res.id}。检查通过后即可在“提交”页选用（默认仅你自己可见）。`);
      setFile(null);
      onDone();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Card title="上传题目包">
      <form class="stack" onSubmit={submit}>
        <p class="muted small">
          zip，格式同仓库 <code>tasksets/hello-world/source</code>：根目录（或唯一的顶层文件夹）放 <code>source.json</code>，每个阶段一个目录，内含给 agent 的需求文件和只给打分器的
          <code>tests/</code>。总限时不超过 18000 秒。打分器可以是平台开放给用户的（如 <code>playwright</code>、<code>llm-judge</code>），也可以是你在<a href="#/plugins" class="link">“插件”页</a>上传的打分器（<code>"scorer": {"{"}"name": "u-…"{"}"}</code>）。可先在本地运行 <code>crucible taskset validate 题目包.zip</code>{" "}
          检查。文件在浏览器里加密后上传；默认私有，只有你能看到和使用，管理员可设为公开。
        </p>
        <input
          type="file"
          accept=".zip,application/zip"
          onChange={(e) => setFile((e.target as HTMLInputElement).files?.[0] ?? null)}
        />
        <div>
          <button type="submit" class="btn primary" disabled={!file || busy}>
            {busy ? "加密并上传中…" : "上传并登记"}
          </button>
        </div>
        {msg && <p class="notice">{msg}</p>}
        {error && <ErrorBox error={error} />}
      </form>
    </Card>
  );
}
