// Browser-side sealing, byte-compatible with crucible-crypto / `crucible open`:
//
//   {"crucible_envelope":1,"alg":"age-x25519","key_id":"<16 hex>"}\n<age binary>
//
// key_id = hex(SHA-256(recipient string)[..8]), as in crucible_crypto::key_id.

import { Encrypter } from "age-encryption";

export interface PublicKeyInfo {
  key_id: string;
  public_key: string;
}

const RECIPIENT_RE = /^age1[02-9ac-hj-np-z]{58}$/;

export function isRecipient(s: string): boolean {
  return RECIPIENT_RE.test(s.trim());
}

export async function keyIdOf(recipient: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(recipient.trim()),
  );
  return toHex(new Uint8Array(digest).slice(0, 8));
}

/** Header line exactly as serde_json writes `Envelope` (field order matters only for humans). */
export function headerLine(keyId: string): Uint8Array {
  const json = JSON.stringify({ crucible_envelope: 1, alg: "age-x25519", key_id: keyId });
  return new TextEncoder().encode(json + "\n");
}

/** Check that a public key's claimed key_id matches its fingerprint. */
export async function verifyKey(k: PublicKeyInfo): Promise<boolean> {
  return isRecipient(k.public_key) && (await keyIdOf(k.public_key)) === k.key_id;
}

/** Encrypt `plain` to `key` and prefix the envelope header. */
export async function seal(key: PublicKeyInfo, plain: Uint8Array | string): Promise<Uint8Array> {
  if (!(await verifyKey(key))) throw new Error("公钥与编号不匹配");
  const e = new Encrypter();
  e.addRecipient(key.public_key.trim());
  const body = await e.encrypt(plain);
  const head = headerLine(key.key_id);
  const out = new Uint8Array(head.length + body.length);
  out.set(head, 0);
  out.set(body, head.length);
  return out;
}

/** The credential envelope sent with POST /evals (base64 of a sealed JSON). */
export interface Credential {
  api_key: string;
  endpoint: string;
  download_password: string;
  eval_id: string;
}

export async function sealCredential(key: PublicKeyInfo, cred: Credential): Promise<string> {
  return toBase64(await seal(key, JSON.stringify(cred)));
}

export function toHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

export function toBase64(b: Uint8Array): string {
  let s = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < b.length; i += CHUNK) {
    s += String.fromCharCode(...b.subarray(i, i + CHUNK));
  }
  return btoa(s);
}

/** UUID v4 for eval ids. */
export function newEvalId(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = toHex(b);
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}
