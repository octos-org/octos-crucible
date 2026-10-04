// The submit pipeline: seal the upload, POST /uploads, seal the credential,
// POST /evals. The plaintext key and password never leave the browser.

import type { Backend } from "./api";
import { newEvalId, seal, sealCredential, verifyKey, type PublicKeyInfo } from "./crypto";
import { currentKey } from "./keys";
import type { CreateEval } from "./types";
import { MAX_UPLOAD, budgetOf, type FormInput } from "./validate";

export interface ResolvedKey {
  key: PublicKeyInfo;
  /** /pubkey named a different key than the bundled one: the page is stale. */
  rotated: boolean;
}

export async function resolveKey(backend: Backend): Promise<ResolvedKey> {
  const bundled = currentKey();
  let remote: PublicKeyInfo | null = null;
  try {
    remote = await backend.pubkey();
  } catch {
    remote = null; // Worker unreachable: the bundled key is still valid to seal with.
  }
  if (remote && remote.key_id !== bundled.key_id) {
    if (!(await verifyKey(remote))) throw new Error("服务器返回的公钥无效，请联系管理员");
    return { key: remote, rotated: true };
  }
  return { key: bundled, rotated: false };
}

/** Upload a taskset zip: sealed in the browser, then registered. */
export async function submitTaskset(
  backend: Backend,
  bytes: Uint8Array,
): Promise<{ id: string; status: string }> {
  const { key } = await resolveKey(backend);
  const sealed = await seal(key, bytes);
  if (sealed.length > MAX_UPLOAD) throw new Error("加密后文件超过 25 MB 上限");
  const { hash } = await backend.upload("taskset", sealed);
  return backend.registerTaskset(hash);
}

export type Step = "key" | "encrypt" | "upload" | "credential" | "submit";

export const STEP_TEXT: Record<Step, string> = {
  key: "获取公钥…",
  encrypt: "在浏览器中加密文件…",
  upload: "上传加密文件…",
  credential: "加密凭据…",
  submit: "提交评测…",
};

export interface SubmitResult {
  eval_id: string;
  rotated: boolean;
}

export async function submitEval(
  backend: Backend,
  f: FormInput,
  bytes: Uint8Array,
  onStep: (s: Step) => void = () => {},
): Promise<SubmitResult> {
  onStep("key");
  const { key, rotated } = await resolveKey(backend);

  onStep("encrypt");
  const sealed = await seal(key, bytes);
  if (sealed.length > MAX_UPLOAD) throw new Error("加密后文件超过 25 MB 上限");

  onStep("upload");
  const { hash } = await backend.upload(f.mode, sealed);

  const eval_id = newEvalId();
  const body: CreateEval = {
    mode: f.mode,
    eval_id,
    upload_hash: hash,
    taskset: f.taskset,
    score_public: f.scorePublic,
    consent: true,
  };
  if (f.mode === "app") body.stages = Number(f.stage);
  else {
    body.replicas = Number(f.replicas);
    const budget = budgetOf(f);
    if (budget) body.budget = budget;
  }
  if (f.mode === "agent" || f.needsModel) {
    onStep("credential");
    body.model = f.model.trim();
    body.cred_envelope = await sealCredential(key, {
      api_key: f.apiKey.trim(),
      endpoint: f.endpoint.trim(),
      download_password: f.password,
      eval_id,
    });
  }

  onStep("submit");
  const res = await backend.createEval(body);
  return { eval_id: res.eval_id || eval_id, rotated };
}
