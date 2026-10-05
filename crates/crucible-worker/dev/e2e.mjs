// End-to-end check against `wrangler dev` + dev/mock-github.mjs.
//
//   cp .dev.vars.example .dev.vars
//   node dev/mock-github.mjs &
//   npx wrangler dev --port 8787 &
//   node dev/e2e.mjs [http://localhost:8787] [http://127.0.0.1:9911]
//
// login (OAuth via the mock, and dev-login) -> upload -> submit -> Actions
// fetches and deletes the credential -> progress -> results -> query ->
// download.

import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";

const W = process.argv[2] || "http://localhost:8787";
const GH = process.argv[3] || "http://127.0.0.1:9911";
const INTERNAL = "Bearer dev-worker-token-dev-worker-token-0000";
const PAGES = "http://localhost:5173";

const step = (s) => console.log(`- ${s}`);
async function call(path, { token, ...init } = {}) {
  const headers = { ...(init.headers || {}) };
  if (token) headers.authorization = token.startsWith("Bearer ") ? token : `Bearer ${token}`;
  const res = await fetch(W + path, { redirect: "manual", ...init, headers });
  const buf = Buffer.from(await res.arrayBuffer());
  let json = null;
  try {
    json = JSON.parse(buf.toString());
  } catch {}
  return { status: res.status, headers: res.headers, buf, json };
}

// OAuth round trip through the mock.
step("OAuth login");
const login = await call("/auth/login");
assert.equal(login.status, 302);
const authorize = new URL(login.headers.get("location"));
assert.equal(authorize.origin, GH);
const state = authorize.searchParams.get("state");
const cookie = login.headers.get("set-cookie").split(";")[0];
const cb = await call(`/auth/callback?code=good&state=${encodeURIComponent(state)}`, { headers: { cookie } });
assert.equal(cb.status, 302);
const landing = cb.headers.get("location");
assert.ok(landing.startsWith(`${PAGES}/#token=`), landing);
const token = landing.split("#token=")[1];
const me = await call("/me", { token });
assert.deepEqual(me.json, { github_id: 42, login: "octocat", is_admin: false });
const forged = await call(`/auth/callback?code=good&state=${encodeURIComponent(state)}`);
assert.ok(forged.headers.get("location").endsWith("#error=oauth_state"));

step("dev-login (admin)");
const dev = await call("/auth/dev-login?github_id=1&login=admin");
assert.equal(dev.status, 200);
const admin = dev.json.token;
assert.equal((await call("/me", { token: admin })).json.is_admin, true);

step("CORS");
const pre = await call("/evals", { method: "OPTIONS", headers: { origin: PAGES, "access-control-request-method": "POST" } });
assert.equal(pre.status, 204);
assert.equal(pre.headers.get("access-control-allow-origin"), PAGES);

step("pubkey / tasksets");
const pk = (await call("/pubkey")).json;
assert.equal(pk.key_id, createHash("sha256").update(pk.public_key).digest("hex").slice(0, 16));
const ts = (await call("/tasksets")).json;
assert.deepEqual(ts, [{ name: "github-full", version: "1.0", stages: [
  { name: "stage-1", time_limit_s: 3600, total: 30 },
  { name: "stage-2", time_limit_s: 3600, total: 29 },
] }]);

// A stand-in for a browser-sealed file: real envelope header, fake body.
const sealed = (s) => Buffer.concat([
  Buffer.from(JSON.stringify({ crucible_envelope: 1, alg: "age-x25519", key_id: pk.key_id }) + "\n"),
  Buffer.from(`age-encryption.org/v1\n${s}`),
]);

step("upload");
const pkg = sealed("agent package " + randomUUID());
const up = await call("/uploads", { method: "POST", token, body: pkg, headers: { "content-type": "application/octet-stream", "x-upload-kind": "agent" } });
assert.equal(up.status, 201, up.buf.toString());
const hash = up.json.hash;
assert.equal(hash, createHash("sha256").update(pkg).digest("hex"));
const plain = await call("/uploads", { method: "POST", token, body: Buffer.from("PK\x03\x04"), headers: { "x-upload-kind": "agent" } });
assert.equal(plain.json.error.code, "not_sealed");

step("submit");
const evalId = randomUUID();
const cred = sealed(JSON.stringify({ note: "ciphertext stand-in" }));
const sub = await call("/evals", {
  method: "POST", token, headers: { "content-type": "application/json" },
  body: JSON.stringify({
    mode: "agent", eval_id: evalId, upload_hash: hash, taskset: "github-full", stages: 2,
    model: "glm-5.3", replicas: 2, budget: { max_requests: 100 },
    cred_envelope: cred.toString("base64"), score_public: false, consent: true,
  }),
});
assert.equal(sub.status, 201, sub.buf.toString());
assert.deepEqual(sub.json, { eval_id: evalId });
const ghState = await (await fetch(`${GH}/_state`)).json();
const d = ghState.dispatches.at(-1);
assert.equal(d.workflow, "eval.yml");
assert.equal(d.inputs.agent_source, `blob:${hash}`);
assert.equal(d.inputs.cred_source, "workers-kv");
assert.equal(d.inputs.owner, "42:octocat");
assert.deepEqual(JSON.parse(d.inputs.options), { stages: 2, results_url: `${W}/internal/results/${evalId}`, budget: { max_requests: 100 } });
assert.ok(!JSON.stringify(d).includes(cred.toString("base64")));

step("Actions: fetch + delete credential");
assert.equal((await call(`/internal/cred/${evalId}`)).status, 401);
const got = await call(`/internal/cred/${evalId}`, { token: INTERNAL });
assert.equal(got.status, 200);
assert.ok(got.buf.equals(cred));
assert.equal((await call(`/internal/cred/${evalId}`, { method: "DELETE", token: INTERNAL })).status, 204);
assert.equal((await call(`/internal/cred/${evalId}`, { token: INTERNAL })).status, 404);

step("progress");
let detail = (await call(`/evals/${evalId}`, { token })).json;
assert.equal(detail.status, "queued");
const runId = ghState.runs.at(-1).id;
await fetch(`${GH}/_run/${runId}`, { method: "POST", body: JSON.stringify({ status: "in_progress" }) });
detail = (await call(`/evals/${evalId}`, { token })).json;
assert.equal(detail.status, "running:stage-1");
assert.ok(detail.run_url.endsWith(`/runs/${runId}`));
const st = await call(`/internal/status/${evalId}`, { method: "POST", token: INTERNAL, body: JSON.stringify({ status: "scoring" }) });
assert.equal(st.status, 200);
assert.equal((await call(`/evals/${evalId}`, { token })).json.status, "scoring");

step("results");
const zip = createHash("sha256").update("zip").digest("hex");
const usage = { requests: 3, prompt_tokens: 10, cached_tokens: 2, completion_tokens: 5, reasoning_tokens: 0 };
const manifest = {
  schema: 1, eval_id: evalId, created_at: "2026-10-03T00:00:00Z", taskset: "github-full",
  agent: { name: "uploaded", version: "1" }, model: "glm-5.3", public: false,
  replicas: [{ replica: 1, stages: [
    { stage: "stage-1", score: { status: "failed", passed: 27, total: 30 }, usage },
    { stage: "stage-2", score: { status: "failed", passed: 20, total: 29 }, usage },
  ] }],
  download: { sha256: zip },
};
const posted = await call(`/internal/results/${evalId}`, { method: "POST", token: INTERNAL, body: JSON.stringify(manifest) });
assert.equal(posted.status, 200, posted.buf.toString());

step("query");
detail = (await call(`/evals/${evalId}`, { token })).json;
assert.equal(detail.status, "done");
assert.equal(detail.total_score, Math.round((47 / 59) * 10000) / 10000);
assert.equal(detail.manifest.replicas[0].stages[1].score.passed, 20);
const list = (await call("/evals", { token })).json;
assert.equal(list.find((e) => e.eval_id === evalId).status, "done");
assert.equal((await call(`/evals/${evalId}`, { token: (await call("/auth/dev-login?github_id=7&login=bob")).json.token })).status, 403);

step("download");
const dl = await call(`/evals/${evalId}/download`, { token });
assert.equal(dl.status, 302);
assert.ok(dl.headers.get("location").endsWith(`/releases/download/blobs-${String(parseInt(zip.slice(0, 2), 16) >> 3).padStart(2, "0")}/${zip}`));
const dlj = await call(`/evals/${evalId}/download`, { token, headers: { accept: "application/json" } });
assert.equal(dlj.json.url, dl.headers.get("location"));

step("cron (expired credentials)");
const cron = await fetch(`${W}/__scheduled?cron=${encodeURIComponent("17 * * * *")}`);
assert.equal(cron.status, 200, await cron.text());

step("ban");
assert.equal((await call("/admin/ban", { method: "POST", token: admin, body: JSON.stringify({ github_id: 42 }) })).status, 200);
assert.equal((await call("/me", { token })).json.error.code, "banned");
assert.equal((await call("/admin/unban", { method: "POST", token: admin, body: JSON.stringify({ github_id: 42 }) })).status, 200);
assert.equal((await call("/me", { token })).status, 200);

console.log("e2e OK");
