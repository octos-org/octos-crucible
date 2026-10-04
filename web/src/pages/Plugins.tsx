import { useState } from "preact/hooks";
import { api, getToken } from "../api";
import { submitPlugin } from "../submit";
import type { UserPlugin } from "../types";
import { Card, ErrorBox, Loading, useAsync } from "../ui";

const STATUS: Record<string, string> = { building: "构建中", ready: "可用", failed: "未通过" };

export function Plugins() {
  const loggedIn = !!getToken();
  const me = useAsync(() => (loggedIn ? api().me() : Promise.resolve(null)), [loggedIn]);
  const admin = !!me.data?.is_admin;
  const [all, setAll] = useState(false);
  const list = useAsync(() => api().plugins(admin && all), [admin, all]);
  return (
    <div class="stack">
      <h1>插件</h1>
      <p class="muted small">
        打分器插件决定一个阶段的产出怎样得分。除了平台自带的打分器，你可以上传自己的打分器：打分逻辑写在容器镜像里，平台在隔离的容器中运行它。上传后在自己的题目包里写{" "}
        <code>"scorer": {"{"}"name": "u-…"{"}"}</code> 引用即可。
      </p>
      {loggedIn && <UploadPlugin onDone={list.reload} />}
      {admin && (
        <label class="small">
          <input type="checkbox" checked={all} onChange={(e) => setAll((e.target as HTMLInputElement).checked)} /> 显示所有用户上传的插件（管理员）
        </label>
      )}
      {list.loading && <Loading />}
      {list.error && <ErrorBox error={list.error} onRetry={list.reload} />}
      {list.data?.length === 0 && <p class="muted">暂无上传的插件。</p>}
      {list.data?.map((p) => <PluginCard p={p} admin={admin} onChange={list.reload} />)}
    </div>
  );
}

function PluginCard({ p, admin, onChange }: { p: UserPlugin; admin: boolean; onChange: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const toggle = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().setPluginPublic(p.id, !p.public);
      onChange();
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(false);
    }
  };
  const aside = (
    <span class="muted small">
      {p.kind === "scorer" ? "打分器" : (p.kind ?? "插件")}
      {p.version ? ` v${p.version}` : ""} · {p.owner_login} · {p.public ? "公开" : "私有"} · {STATUS[p.status] ?? p.status}
    </span>
  );
  const st = p.selftest;
  return (
    <Card title={`${p.title ?? "插件"}（${p.id}）`} aside={aside}>
      {p.status === "failed" && <p class="notice bad">未通过：{p.error}</p>}
      {p.status === "building" && <p class="muted small">正在检查、构建镜像并自检，通常几分钟；刷新页面查看结果。</p>}
      {p.description && <p>{p.description}</p>}
      {p.status === "ready" && (
        <p class="muted small">
          打分的产出：{p.accepts?.length ? p.accepts.join("、") : "不限"} · {p.model ? "可使用模型（经计量代理，用提交者的 key）" : "不使用模型"} ·{" "}
          {p.runs_taskset_code ? "会执行题目包文件" : "只读题目包数据"}
          {st && (
            <>
              {" "}
              · 自检：{st.status === "scored" ? `${st.score ?? "—"}${st.max != null ? ` / ${st.max}` : ""}` : "出错"}
              {st.detail ? `（${st.detail}）` : ""}
            </>
          )}
        </p>
      )}
      {admin && p.status === "ready" && (
        <p>
          <button type="button" class="link" disabled={busy} onClick={toggle}>
            {p.public ? "改为私有" : "设为公开"}
          </button>
        </p>
      )}
      {error && <ErrorBox error={error} />}
    </Card>
  );
}

function UploadPlugin({ onDone }: { onDone: () => void }) {
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
      const res = await submitPlugin(api(), new Uint8Array(await file.arrayBuffer()));
      setMsg(`已提交：${res.id}。构建和自检通过后，你的题目包即可用 "scorer": {"name": "${res.id}"} 引用（默认仅你自己的题目包可用）。`);
      setFile(null);
      onDone();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Card title="上传打分器插件">
      <form class="stack" onSubmit={submit}>
        <p class="muted small">
          zip，根目录（或唯一的顶层文件夹）放 <code>plugin.json</code>（名字、类型 <code>scorer</code>、版本、<code>runs_taskset_code</code>、<code>model</code>、
          <code>accepts</code>）和 <code>Dockerfile</code>。镜像的入口会收到 <code>--artifact /in/artifact --tests /in/tests --out /out/result.json</code>{" "}
          等参数，写出 result.json（v2）。可选 <code>selftest/artifact</code> 与 <code>selftest/tests/</code> 作为自检样例。示例见仓库{" "}
          <code>examples/plugins/keyword-scorer</code>，格式见 <code>docs/plugins.md</code> §14。文件在浏览器里加密后上传；镜像在 GitHub 托管机上构建并自检；默认私有，管理员可设为公开。
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
