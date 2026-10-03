// Submit-form validation. Pure functions, unit tested.

import type { Budget, Mode } from "./types";

/** POST /uploads limit, applied to the sealed bytes. */
export const MAX_UPLOAD = 25 * 1024 * 1024;
/** age adds ~16 bytes per 64 KiB chunk plus a header; keep a margin for it. */
export const MAX_PLAIN = MAX_UPLOAD - 64 * 1024;
export const MAX_REPLICAS = 10;
/** The output zip sits at a public URL; this password is its only protection. */
export const MIN_PASSWORD = 12;

export interface FormInput {
  mode: Mode;
  taskset: string;
  /** app mode: 1-based stage number, as text from the select. */
  stage: string;
  file: { name: string; size: number } | null;
  /** First 4 bytes of the file, for the zip magic check. */
  fileMagic?: Uint8Array | null;
  model: string;
  endpoint: string;
  apiKey: string;
  replicas: string;
  maxRequests: string;
  maxTokens: string;
  maxCostUsd: string;
  password: string;
  password2: string;
  scorePublic: boolean;
  consent: boolean;
}

export type Errors = Partial<Record<keyof FormInput, string>>;

export function emptyForm(mode: Mode): FormInput {
  return {
    mode,
    taskset: "",
    stage: "",
    file: null,
    fileMagic: null,
    model: "",
    endpoint: "",
    apiKey: "",
    replicas: "1",
    maxRequests: "",
    maxTokens: "",
    maxCostUsd: "",
    password: "",
    password2: "",
    scorePublic: false,
    consent: false,
  };
}

const PRIVATE_HOST =
  /^(localhost|.*\.localhost|.*\.local|.*\.internal|0\.0\.0\.0|127\.\d+\.\d+\.\d+|10\.\d+\.\d+\.\d+|192\.168\.\d+\.\d+|172\.(1[6-9]|2\d|3[01])\.\d+\.\d+|169\.254\.\d+\.\d+|\[.*\])$/i;

/** Client-side check only; the meter enforces the real SSRF policy. */
export function checkEndpoint(raw: string): string | null {
  const s = raw.trim();
  if (!s) return "请填写接口地址";
  let u: URL;
  try {
    u = new URL(s);
  } catch {
    return "接口地址格式不正确";
  }
  if (u.protocol !== "https:") return "接口地址必须以 https:// 开头";
  if (u.username || u.password) return "接口地址不能包含用户名或密码";
  if (PRIVATE_HOST.test(u.hostname)) return "接口地址不能指向本机或内网";
  return null;
}

function positiveInt(s: string): number | null {
  if (!/^\d+$/.test(s.trim())) return null;
  const n = Number(s.trim());
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

function positiveNumber(s: string): number | null {
  if (!/^\d+(\.\d+)?$/.test(s.trim())) return null;
  const n = Number(s.trim());
  return Number.isFinite(n) && n > 0 ? n : null;
}

export function isZipMagic(b: Uint8Array | null | undefined): boolean {
  return !!b && b.length >= 4 && b[0] === 0x50 && b[1] === 0x4b && b[2] === 0x03 && b[3] === 0x04;
}

export function validate(f: FormInput): Errors {
  const e: Errors = {};
  if (!f.taskset) e.taskset = "请选择题目包";
  if (!f.file) e.file = f.mode === "agent" ? "请选择 agent 包（zip）" : "请选择产出文件（zip）";
  else if (!/\.zip$/i.test(f.file.name)) e.file = "只接受 .zip 文件";
  else if (f.file.size === 0) e.file = "文件是空的";
  else if (f.file.size > MAX_PLAIN) e.file = "文件超过 25 MB 上限";
  else if (f.fileMagic && !isZipMagic(f.fileMagic)) e.file = "文件不是有效的 zip";

  if (f.mode === "app") {
    if (!positiveInt(f.stage)) e.stage = "请选择阶段";
  } else {
    if (!f.model.trim()) e.model = "请填写模型名";
    else if (f.model.trim().length > 200) e.model = "模型名过长";
    const ep = checkEndpoint(f.endpoint);
    if (ep) e.endpoint = ep;
    if (!f.apiKey.trim()) e.apiKey = "请填写 API key";
    const r = positiveInt(f.replicas);
    if (!r || r > MAX_REPLICAS) e.replicas = `运行遍数为 1–${MAX_REPLICAS} 的整数`;
    if (f.maxRequests.trim() && !positiveInt(f.maxRequests)) e.maxRequests = "请填正整数，或留空";
    if (f.maxTokens.trim() && !positiveInt(f.maxTokens)) e.maxTokens = "请填正整数，或留空";
    if (f.maxCostUsd.trim() && !positiveNumber(f.maxCostUsd)) e.maxCostUsd = "请填正数，或留空";
    if ([...f.password].length < MIN_PASSWORD) e.password = `下载密码至少 ${MIN_PASSWORD} 个字符`;
    else if (f.password !== f.password2) e.password2 = "两次输入的密码不一致";
  }
  if (!f.consent) e.consent = "提交前需要同意声明";
  return e;
}

export function budgetOf(f: FormInput): Budget | undefined {
  const b: Budget = {};
  const r = positiveInt(f.maxRequests);
  const t = positiveInt(f.maxTokens);
  const c = positiveNumber(f.maxCostUsd);
  if (r) b.max_requests = r;
  if (t) b.max_tokens = t;
  if (c) b.max_cost_usd = c;
  return Object.keys(b).length ? b : undefined;
}

export const CONSENT_TEXT =
  "你上传的内容、评测产出和日志会加密后永久保存，我们会用于研究和改进平台。你的模型 key 和下载密码在评测结束后立即删除，不会保存。";
