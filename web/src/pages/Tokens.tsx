// Personal API tokens for `crucible submit` (docs/guide/README.md, 命令行提交).
// The plaintext token is shown once, right after it is created.

import { useState } from "preact/hooks";
import { api } from "../api";
import { fmtTime } from "../stats";
import type { NewApiToken } from "../types";
import { Card, ErrorBox, useAsync } from "../ui";

export function CliTokens() {
  const list = useAsync(() => api().tokens());
  const [name, setName] = useState("");
  const [fresh, setFresh] = useState<NewApiToken | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error | null>(null);

  async function create() {
    setBusy(true);
    setError(null);
    try {
      setFresh(await api().createToken(name));
      setName("");
      list.reload();
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(false);
    }
  }

  async function revoke(id: string) {
    if (!confirm("撤销后使用该令牌的命令行将无法再提交，确定撤销？")) return;
    setError(null);
    try {
      await api().deleteToken(id);
      if (fresh?.id === id) setFresh(null);
      list.reload();
    } catch (e) {
      setError(e as Error);
    }
  }

  return (
    <Card title="命令行令牌">
      <p class="muted small">
        用于 <code>crucible submit</code> / <code>crucible status</code>：设置环境变量 <code>CRUCIBLE_TOKEN</code>{" "}
        后即可从命令行提交评测。令牌等同于你的账号，请妥善保管；不用时撤销。
      </p>
      <div class="row">
        <input
          type="text"
          placeholder="名称（可选，如 laptop）"
          maxLength={60}
          value={name}
          onInput={(e) => setName(e.currentTarget.value)}
        />
        <button type="button" class="btn" disabled={busy} onClick={create}>
          生成命令行令牌
        </button>
      </div>
      {fresh && (
        <div class="notice warn" role="status">
          <span>
            新令牌（只显示这一次，请立即复制）：
            <br />
            <code style="word-break: break-all; user-select: all">{fresh.token}</code>
            <br />
            <code>export CRUCIBLE_TOKEN={fresh.token}</code>
          </span>
        </div>
      )}
      {error && <ErrorBox error={error} />}
      {list.error && <ErrorBox error={list.error} onRetry={list.reload} />}
      {list.data && list.data.length > 0 && (
        <ul class="list">
          {list.data.map((t) => (
            <li class="item">
              <div class="item-main">
                <div class="item-title">{t.name}</div>
                <div class="muted small">
                  {fmtTime(t.created_at)} · <code>{t.id}</code>
                </div>
              </div>
              <div class="item-side">
                <button type="button" class="btn" onClick={() => revoke(t.id)}>
                  撤销
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
