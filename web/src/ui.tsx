import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";
import { statusLabel } from "./stats";

export interface Async<T> {
  data: T | null;
  error: Error | null;
  loading: boolean;
  reload: () => void;
}

export function useAsync<T>(fn: () => Promise<T>, deps: unknown[] = []): Async<T> {
  const [state, setState] = useState<{ data: T | null; error: Error | null; loading: boolean }>({
    data: null,
    error: null,
    loading: true,
  });
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let live = true;
    setState((s) => ({ ...s, loading: true, error: null }));
    fn().then(
      (data) => live && setState({ data, error: null, loading: false }),
      (error: Error) => live && setState({ data: null, error, loading: false }),
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick]);
  return { ...state, reload: () => setTick((t) => t + 1) };
}

export function Loading() {
  return <p class="muted">加载中…</p>;
}

export function ErrorBox({ error, onRetry }: { error: Error; onRetry?: () => void }) {
  return (
    <div class="notice bad" role="alert">
      <span>{error.message || "出错了"}</span>
      {onRetry && (
        <button type="button" class="link" onClick={onRetry}>
          重试
        </button>
      )}
    </div>
  );
}

export function StatusBadge({ status }: { status: string }) {
  const { text, tone } = statusLabel(status);
  return <span class={`badge ${tone}`}>{text}</span>;
}

export function Card({ title, children, aside }: { title?: string; children: ComponentChildren; aside?: ComponentChildren }) {
  return (
    <section class="card">
      {(title || aside) && (
        <header class="card-head">
          {title && <h2>{title}</h2>}
          {aside}
        </header>
      )}
      {children}
    </section>
  );
}

export function shortId(id: string): string {
  return id.slice(0, 8);
}
