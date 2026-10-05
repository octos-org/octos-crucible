import { api, getToken } from "../api";
import type { QuotaItem } from "../types";
import { useAsync } from "../ui";

function fmt(i: QuotaItem, v: number): string {
  return i.name === "upload_bytes_per_day" ? `${Math.round(v / (1 << 20))} MB` : String(v);
}

/** Remaining quota for the given items (docs/api.md "配额"); nothing when logged out or on error. */
export function QuotaNote({ names }: { names: string[] }) {
  const loggedIn = !!getToken();
  const q = useAsync(() => (loggedIn ? api().quota() : Promise.resolve(null)), [loggedIn]);
  const d = q.data;
  if (!d) return null;
  if (d.exempt) return <p class="muted small">额度：不受限（豁免）。</p>;
  const items = d.items.filter((i) => names.includes(i.name));
  return (
    <p class="muted small">
      剩余额度：
      {items.map((i, k) => (
        <span class={i.remaining === 0 ? "warn-text" : ""}>
          {k > 0 ? " · " : ""}
          {i.description} {fmt(i, i.remaining)} / {fmt(i, i.limit)}
          {i.remaining === 0 && i.frees_at ? `（${i.frees_at} 后恢复）` : ""}
        </span>
      ))}
    </p>
  );
}
