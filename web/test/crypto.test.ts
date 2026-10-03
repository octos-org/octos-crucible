// Interop: the browser sealing code must produce files that `crucible open`
// (Rust) and the `age` CLI (header stripped) can decrypt. Uses a throwaway
// key pair generated here; the platform private key is never involved.

import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { Decrypter, generateX25519Identity, identityToRecipient } from "age-encryption";
import { headerLine, keyIdOf, seal, sealCredential, verifyKey, type PublicKeyInfo } from "../src/crypto";
import { currentKey } from "../src/keys";

const CRUCIBLE = process.env.CRUCIBLE_BIN ?? resolve(__dirname, "../../target/debug/crucible");
const hasCrucible = existsSync(CRUCIBLE);
const hasAge = spawnSync("age", ["--version"]).status === 0;

let dir: string;
let identityFile: string;
let identity: string;
let key: PublicKeyInfo;

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), "crucible-web-test-"));
  identity = await generateX25519Identity();
  const recipient = await identityToRecipient(identity);
  key = { key_id: await keyIdOf(recipient), public_key: recipient };
  identityFile = join(dir, "test-identity.txt");
  writeFileSync(identityFile, `# throwaway test key\n${identity}\n`, { mode: 0o600 });
});

afterAll(() => rmSync(dir, { recursive: true, force: true }));

function sample(n: number): Uint8Array {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(b.subarray(i, Math.min(n, i + 65536)));
  return b;
}

function splitEnvelope(file: Uint8Array): { header: Record<string, unknown>; body: Uint8Array } {
  const nl = file.indexOf(0x0a);
  return {
    header: JSON.parse(new TextDecoder().decode(file.subarray(0, nl))),
    body: file.subarray(nl + 1),
  };
}

describe("envelope format", () => {
  it("key_id matches the Rust fingerprint of the bundled platform key", async () => {
    const k = currentKey();
    expect(await keyIdOf(k.public_key)).toBe(k.key_id);
    expect(await verifyKey(k)).toBe(true);
  });

  it("rejects a key whose id does not match", async () => {
    await expect(seal({ ...key, key_id: "0000000000000000" }, "x")).rejects.toThrow();
  });

  it("writes the header line then an age file", async () => {
    const file = await seal(key, "hello");
    const { header, body } = splitEnvelope(file);
    expect(header).toEqual({ crucible_envelope: 1, alg: "age-x25519", key_id: key.key_id });
    expect(new TextDecoder().decode(body.subarray(0, 21))).toBe("age-encryption.org/v1");
  });

  it("round-trips in JS", async () => {
    const plain = sample(200_000);
    const { body } = splitEnvelope(await seal(key, plain));
    const d = new Decrypter();
    d.addIdentity(identity);
    expect(await d.decrypt(body)).toEqual(plain);
  });
});

describe.skipIf(!hasCrucible)("interop with crucible (Rust)", () => {
  it("header line is byte-identical to `crucible seal`", () => {
    const out = execFileSync(CRUCIBLE, ["seal", "--recipient", key.public_key, "--in", "-", "--out", "-"], {
      input: "x",
    });
    const nl = out.indexOf(0x0a);
    expect(Buffer.from(headerLine(key.key_id))).toEqual(out.subarray(0, nl + 1));
  });

  it("`crucible open` decrypts a browser-sealed upload", async () => {
    const plain = sample(3 * 1024 * 1024 + 17);
    const sealedPath = join(dir, "upload.sealed");
    const outPath = join(dir, "upload.out");
    writeFileSync(sealedPath, await seal(key, plain));
    execFileSync(CRUCIBLE, ["open", "--identity", identityFile, "--in", sealedPath, "--out", outPath]);
    expect(new Uint8Array(readFileSync(outPath))).toEqual(plain);
  });

  it("`crucible open` decrypts the credential envelope", async () => {
    const cred = {
      api_key: "sk-test-not-a-real-key",
      endpoint: "https://api.example.com/v1",
      download_password: "correct horse 电池",
      eval_id: "7c1e2f5a-0b8d-4e57-9a51-3f2c1d0e9b11",
    };
    const b64 = await sealCredential(key, cred);
    const sealedPath = join(dir, "cred.sealed");
    writeFileSync(sealedPath, Buffer.from(b64, "base64"));
    const out = execFileSync(CRUCIBLE, ["open", "--identity", identityFile, "--in", sealedPath, "--out", "-"]);
    expect(JSON.parse(out.toString("utf8"))).toEqual(cred);
  });

  it("refuses with a different key", async () => {
    const other = join(dir, "other-identity.txt");
    writeFileSync(other, `${await generateX25519Identity()}\n`, { mode: 0o600 });
    const sealedPath = join(dir, "wrong.sealed");
    writeFileSync(sealedPath, await seal(key, "secret"));
    const r = spawnSync(CRUCIBLE, ["open", "--identity", other, "--in", sealedPath, "--out", "-"]);
    expect(r.status).not.toBe(0);
  });
});

describe.skipIf(!hasAge)("interop with the age CLI", () => {
  it("`age -d` decrypts after the header line is stripped", async () => {
    const plain = sample(150_000);
    const { body } = splitEnvelope(await seal(key, plain));
    const p = join(dir, "body.age");
    writeFileSync(p, body);
    const out = execFileSync("age", ["-d", "-i", identityFile, p]);
    expect(new Uint8Array(out)).toEqual(plain);
  });
});
