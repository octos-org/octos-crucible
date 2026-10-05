import { QuotaNote } from "./QuotaNote";
import { useState } from "preact/hooks";
import { api, getToken } from "../api";
import { submitPlugin } from "../submit";
import type { PluginReview, UserPlugin } from "../types";
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

/** The review checklist (docs/plugins.md §14.6); all must be checked to make a plugin public. */
const CHECKLIST: Record<string, string> = {
  source: "读过源码：打分逻辑与说明一致，没有可疑的下载、外联或混淆代码",
  dockerfile: "读过 Dockerfile：基础镜像可信，构建步骤只装声明的依赖",
  detail_leak: "hidden 模式下 detail / items 不会泄露测试内容",
  model_use: "使用模型（若声明 model）的方式合理，不会滥用提交者的 key",
};

function PluginCard({ p, admin, onChange }: { p: UserPlugin; admin: boolean; onChange: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [reviewing, setReviewing] = useState(false);
  const makePrivate = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().setPluginPublic(p.id, false);
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
          {p.image && (
            <>
              {" "}
              · 镜像 <code title={p.image}>{p.image.slice(0, 19)}</code>
            </>
          )}
        </p>
      )}
      {p.approval && (
        <p class="muted small">
          已审核：{p.approval.by_login} · {p.approval.at}
          {p.approval.note ? ` · ${p.approval.note}` : ""}
        </p>
      )}
      {admin && p.status === "ready" && (
        <p>
          {p.public ? (
            <button type="button" class="link" disabled={busy} onClick={makePrivate}>
              改为私有
            </button>
          ) : (
            <button type="button" class="link" onClick={() => setReviewing(!reviewing)}>
              {reviewing ? "收起审核" : "审核并设为公开"}
            </button>
          )}
        </p>
      )}
      {admin && reviewing && !p.public && (
        <ReviewPanel
          id={p.id}
          onDone={() => {
            setReviewing(false);
            onChange();
          }}
        />
      )}
      {error && <ErrorBox error={error} />}
    </Card>
  );
}

function size(n: number): string {
  return n < 1024 ? `${n} B` : n < 1 << 20 ? `${(n / 1024).toFixed(1)} KB` : `${(n / (1 << 20)).toFixed(1)} MB`;
}

/** Admin: the package's file list, Dockerfile and text files, the checklist, and "make public". */
function ReviewPanel({ id, onDone }: { id: string; onDone: () => void }) {
  const r = useAsync<PluginReview>(() => api().pluginReview(id), [id]);
  const [checked, setChecked] = useState<string[]>([]);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  if (r.loading) return <Loading />;
  if (r.error) return <ErrorBox error={r.error} onRetry={r.reload} />;
  const rv = r.data?.review;
  const items = r.data?.checklist ?? Object.keys(CHECKLIST);
  const all = items.every((c) => checked.includes(c));
  const toggle = (c: string, on: boolean) => setChecked(on ? [...checked, c] : checked.filter((x) => x !== c));
  const publish = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().setPluginPublic(id, true, { checked, note });
      onDone();
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div class="stack">
      {!rv && <p class="notice">这个插件登记于审核材料上线之前，没有可读的源码清单；需要的话请上传者重新上传。</p>}
      {rv && (
        <>
          <details open>
            <summary>文件清单（{rv.files.length} 个）</summary>
            <ul class="small">
              {rv.files.map((f) => (
                <li>
                  <code>{f.path}</code> <span class="muted">{size(f.size)}</span>
                </li>
              ))}
            </ul>
          </details>
          <details open>
            <summary>Dockerfile</summary>
            <pre class="small">{rv.dockerfile}</pre>
          </details>
          {rv.texts.map((t) => (
            <details>
              <summary>
                <code>{t.path}</code>
              </summary>
              <pre class="small">{t.content}</pre>
            </details>
          ))}
          {rv.truncated && <p class="muted small">文本过长，只显示了一部分。</p>}
        </>
      )}
      <fieldset class="stack">
        <legend>审核清单（全部勾选才能设为公开）</legend>
        {items.map((c) => (
          <label class="small">
            <input type="checkbox" checked={checked.includes(c)} onChange={(e) => toggle(c, (e.target as HTMLInputElement).checked)} />{" "}
            {CHECKLIST[c] ?? c}
          </label>
        ))}
        <input type="text" placeholder="备注（可选）" value={note} maxLength={1000} onInput={(e) => setNote((e.target as HTMLInputElement).value)} />
      </fieldset>
      <div>
        <button type="button" class="btn primary" disabled={!all || busy} onClick={publish}>
          {busy ? "提交中…" : "确认审核并设为公开"}
        </button>
      </div>
      {error && <ErrorBox error={error} />}
    </div>
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
        <QuotaNote names={["plugins_per_day", "uploads_per_day"]} />
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
