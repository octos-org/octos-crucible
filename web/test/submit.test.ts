// The submit pipeline against a fake backend: what reaches the Worker must be
// sealed, and the plaintext key/password must never appear in a request.

import { describe, expect, it } from "vitest";
import { Decrypter, generateX25519Identity, identityToRecipient } from "age-encryption";
import type { Backend } from "../src/api";
import { keyIdOf } from "../src/crypto";
import { currentKey } from "../src/keys";
import { resolveKey, submitEval } from "../src/submit";
import type { CreateEval } from "../src/types";
import { emptyForm, type FormInput } from "../src/validate";

function fakeBackend(pub: { key_id: string; public_key: string }) {
  const calls: { uploads: Uint8Array[]; evals: CreateEval[] } = { uploads: [], evals: [] };
  const b: Backend = {
    me: async () => ({ github_id: 1, login: "t", is_admin: false }),
    pubkey: async () => pub,
    tasksets: async () => [],
    upload: async (_k, sealed) => {
      calls.uploads.push(sealed);
      return { hash: "h".repeat(64) };
    },
    createEval: async (body) => {
      calls.evals.push(body);
      return { eval_id: body.eval_id };
    },
    evals: async () => [],
    evalDetail: async () => ({ eval_id: "x", status: "queued" }),
    download: async () => {},
  };
  return { b, calls };
}

const form: FormInput = {
  ...emptyForm("agent"),
  taskset: "github-full",
  file: { name: "a.zip", size: 3 },
  model: "glm-5.3",
  endpoint: "https://api.example.com/v1",
  apiKey: "sk-PLAINTEXT-SECRET",
  replicas: "2",
  maxTokens: "1000000",
  password: "PLAINTEXT-PASSWORD",
  password2: "PLAINTEXT-PASSWORD",
  scorePublic: true,
  consent: true,
};

function decode(b64: string): Uint8Array {
  return new Uint8Array(Buffer.from(b64, "base64"));
}

describe("submitEval", () => {
  it("seals upload and credential to the /pubkey key when it differs (rotation)", async () => {
    const id = await generateX25519Identity();
    const rec = await identityToRecipient(id);
    const pub = { key_id: await keyIdOf(rec), public_key: rec };
    const { b, calls } = fakeBackend(pub);

    const res = await submitEval(b, form, new Uint8Array([1, 2, 3]));
    expect(res.rotated).toBe(true);

    const body = calls.evals[0];
    expect(body).toMatchObject({
      mode: "agent",
      taskset: "github-full",
      model: "glm-5.3",
      replicas: 2,
      budget: { max_tokens: 1000000 },
      score_public: true,
      consent: true,
    });
    expect(body.eval_id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    const raw = JSON.stringify(body) + new TextDecoder().decode(calls.uploads[0]);
    expect(raw).not.toContain("PLAINTEXT");

    const d = new Decrypter();
    d.addIdentity(id);
    const strip = (f: Uint8Array) => f.subarray(f.indexOf(0x0a) + 1);
    expect(await d.decrypt(strip(calls.uploads[0]))).toEqual(new Uint8Array([1, 2, 3]));
    const cred = JSON.parse(await d.decrypt(strip(decode(body.cred_envelope!)), "text"));
    expect(cred).toEqual({
      api_key: "sk-PLAINTEXT-SECRET",
      endpoint: "https://api.example.com/v1",
      download_password: "PLAINTEXT-PASSWORD",
      eval_id: body.eval_id,
    });
  });

  it("app mode sends the stage number and no credential", async () => {
    const { b, calls } = fakeBackend(currentKey());
    const app: FormInput = { ...emptyForm("app"), taskset: "github-full", stage: "2", consent: true };
    const res = await submitEval(b, app, new Uint8Array([9]));
    expect(res.rotated).toBe(false);
    expect(calls.evals[0]).toMatchObject({ mode: "app", stages: 2, score_public: false });
    expect(calls.evals[0].cred_envelope).toBeUndefined();
    expect(calls.evals[0].model).toBeUndefined();
  });

  it("falls back to the bundled key when /pubkey is unreachable, rejects a bad one", async () => {
    const { b } = fakeBackend(currentKey());
    b.pubkey = async () => {
      throw new Error("offline");
    };
    expect((await resolveKey(b)).key).toEqual(currentKey());
    b.pubkey = async () => ({ key_id: "deadbeefdeadbeef", public_key: currentKey().public_key });
    await expect(resolveKey(b)).rejects.toThrow();
  });
});
