// Worker API client. All calls carry `Authorization: Bearer <session token>`.
// A mock backend with fixed data is used when VITE_MOCK=1 (or the
// `crucible.mock` sessionStorage flag is set), for development.

import type {
  ApiToken,
  CreateEval,
  EvalDetail,
  EvalSummary,
  Me,
  Mode,
  NewApiToken,
  TaskSet,
} from "./types";
import type { PublicKeyInfo } from "./crypto";
import { mockBackend } from "./mock";

const TOKEN_KEY = "crucible.token";
const MOCK_KEY = "crucible.mock";

export const API_BASE: string = (import.meta.env.VITE_API_BASE ?? "").replace(/\/+$/, "");

export function isMock(): boolean {
  if (import.meta.env.VITE_MOCK === "1") return true;
  try {
    return sessionStorage.getItem(MOCK_KEY) === "1";
  } catch {
    return false;
  }
}

export function setMock(on: boolean): void {
  try {
    if (on) sessionStorage.setItem(MOCK_KEY, "1");
    else sessionStorage.removeItem(MOCK_KEY);
  } catch {
    /* storage blocked */
  }
}

export function getToken(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function setToken(t: string | null): void {
  try {
    if (t) sessionStorage.setItem(TOKEN_KEY, t);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    /* storage blocked */
  }
}

/** Pick up `#token=...` left by /auth/callback; returns true if one was stored. */
export function consumeTokenFromHash(): boolean {
  const m = location.hash.match(/^#token=([^&]+)/);
  if (!m) return false;
  setToken(decodeURIComponent(m[1]));
  history.replaceState(null, "", location.pathname + location.search + "#/evals");
  return true;
}

export function loginUrl(): string {
  return `${API_BASE}/auth/login`;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

export interface Backend {
  me(): Promise<Me>;
  pubkey(): Promise<PublicKeyInfo>;
  tasksets(): Promise<TaskSet[]>;
  upload(kind: Mode, sealed: Uint8Array): Promise<{ hash: string }>;
  createEval(body: CreateEval): Promise<{ eval_id: string }>;
  evals(): Promise<EvalSummary[]>;
  evalDetail(id: string): Promise<EvalDetail>;
  /** Start the download of the password zip. */
  download(id: string): Promise<void>;
  /** Personal API tokens for `crucible submit` (session only). */
  tokens(): Promise<ApiToken[]>;
  createToken(name: string): Promise<NewApiToken>;
  deleteToken(id: string): Promise<void>;
}

async function request(path: string, init: RequestInit = {}): Promise<Response> {
  if (!API_BASE) throw new ApiError(0, "no_backend", "未配置后端地址（VITE_API_BASE）");
  const headers = new Headers(init.headers);
  const t = getToken();
  if (t) headers.set("Authorization", `Bearer ${t}`);
  let res: Response;
  try {
    res = await fetch(API_BASE + path, { ...init, headers });
  } catch {
    throw new ApiError(0, "network", "网络错误，无法连接服务器");
  }
  if (!res.ok) {
    let code = `http_${res.status}`;
    let message = `请求失败（HTTP ${res.status}）`;
    try {
      const body = await res.json();
      if (body?.error) {
        code = String(body.error.code ?? code);
        message = String(body.error.message ?? message);
      }
    } catch {
      /* non-JSON error body */
    }
    if (res.status === 401) setToken(null);
    throw new ApiError(res.status, code, message);
  }
  return res;
}

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  return (await request(path, init)).json() as Promise<T>;
}

const enc = encodeURIComponent;

export const httpBackend: Backend = {
  me: () => json("/me"),
  pubkey: () => json("/pubkey"),
  tasksets: () => json("/tasksets"),
  upload: (kind, sealed) =>
    json("/uploads", {
      method: "POST",
      headers: { "Content-Type": "application/octet-stream", "X-Upload-Kind": kind },
      body: sealed as BodyInit,
    }),
  createEval: (body) =>
    json("/evals", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  evals: () => json("/evals"),
  evalDetail: (id) => json(`/evals/${enc(id)}`),
  tokens: () => json("/tokens"),
  createToken: (name) =>
    json("/tokens", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(name.trim() ? { name: name.trim() } : {}),
    }),
  async deleteToken(id) {
    await request(`/tokens/${enc(id)}`, { method: "DELETE" });
  },
  async download(id) {
    // The endpoint needs the Bearer header, so a plain link cannot be used.
    // If the Worker answers JSON {url} we navigate there; otherwise fetch
    // follows the 302 and we save the bytes it lands on.
    const res = await request(`/evals/${enc(id)}/download`, {
      headers: { Accept: "application/json" },
    });
    const type = res.headers.get("Content-Type") ?? "";
    if (type.includes("application/json")) {
      const body = await res.json();
      if (typeof body?.url === "string") {
        location.assign(body.url);
        return;
      }
    }
    saveBlob(await res.blob(), `crucible-${id}.zip`);
  },
};

export function saveBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

export function api(): Backend {
  return isMock() ? mockBackend : httpBackend;
}
