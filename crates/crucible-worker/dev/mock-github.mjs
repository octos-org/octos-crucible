// Minimal GitHub stand-in for local end-to-end runs (`wrangler dev` with
// DEV_AUTH=1 and GITHUB_API_BASE / GITHUB_WEB_BASE pointing here).
// Implements exactly the calls the Worker makes. No dependencies.
//
//   node dev/mock-github.mjs [port]      (default 9911)
//   GET /_state                          recorded dispatches, assets, runs
//   POST /_run/<id>  {"status","conclusion"}   move a run along

import http from "node:http";

const PORT = Number(process.argv[2] || 9911);
const BASE = `http://127.0.0.1:${PORT}`;
const REPO = "/repos/octos-org/octos-crucible/";

const state = { releases: {}, assets: {}, dispatches: [], runs: [], oauthCodes: ["good"] };

const taskset = {
  schema: 1,
  name: "github-full",
  version: "1.0",
  scorer: { name: "playwright" },
  stages: [
    { id: "stage-1", time_limit_s: 3600, expected_total: 30 },
    { id: "stage-2", time_limit_s: 3600, expected_total: 29 },
  ],
};

const release = (id) => ({
  id,
  upload_url: `${BASE}/uploads/releases/${id}/assets{?name,label}`,
});
const runJson = (r) => ({
  id: r.id,
  html_url: `https://github.com/octos-org/octos-crucible/actions/runs/${r.id}`,
  status: r.status,
  conclusion: r.conclusion,
  display_title: `eval github-full ${r.model} ${r.eval_id}`,
});

function send(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(body === undefined ? "" : JSON.stringify(body));
}

const server = http.createServer(async (req, res) => {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const body = Buffer.concat(chunks);
  const url = new URL(req.url, BASE);
  const path = url.pathname;
  const auth = req.headers.authorization || "";
  if (!req.headers["user-agent"]) return send(res, 403, { message: "User-Agent required" });

  if (path === "/_state") return send(res, 200, state);
  if (req.method === "POST" && path.startsWith("/_run/")) {
    const run = state.runs.find((r) => r.id === Number(path.slice(6)));
    Object.assign(run, JSON.parse(body.toString()));
    return send(res, 200, run);
  }

  // OAuth
  if (path === "/login/oauth/access_token") {
    const form = new URLSearchParams(body.toString());
    if (form.get("client_secret") !== "dev-secret") return send(res, 401, {});
    if (!state.oauthCodes.includes(form.get("code"))) return send(res, 200, { error: "bad_verification_code" });
    return send(res, 200, { access_token: "gho_mockuser", token_type: "bearer" });
  }
  if (path === "/user") {
    if (auth !== "Bearer gho_mockuser") return send(res, 401, {});
    return send(res, 200, { id: 42, login: "octocat" });
  }

  // Everything below is repo-scoped and needs the Worker's token.
  if (auth !== "Bearer dev-github-token") return send(res, 401, { message: "Bad credentials" });

  const up = path.match(/^\/uploads\/releases\/(\d+)\/assets$/);
  if (up && req.method === "POST") {
    const list = (state.assets[up[1]] ||= []);
    const name = url.searchParams.get("name");
    if (list.some((a) => a.name === name)) return send(res, 422, { errors: [{ code: "already_exists" }] });
    list.push({ name, size: body.length });
    return send(res, 201, { name });
  }
  if (!path.startsWith(REPO)) return send(res, 404, { message: "Not Found" });
  const p = path.slice(REPO.length);

  if (req.method === "GET" && p.startsWith("releases/tags/")) {
    const id = state.releases[p.slice(14)];
    return id ? send(res, 200, release(id)) : send(res, 404, { message: "Not Found" });
  }
  if (req.method === "POST" && p === "releases") {
    const b = JSON.parse(body.toString());
    if (b.prerelease !== true) return send(res, 422, {});
    const id = 100 + Object.keys(state.releases).length;
    state.releases[b.tag_name] = id;
    return send(res, 201, release(id));
  }
  const disp = p.match(/^actions\/workflows\/([\w.-]+)\/dispatches$/);
  if (req.method === "POST" && disp) {
    const b = JSON.parse(body.toString());
    state.dispatches.push({ workflow: disp[1], ref: b.ref, inputs: b.inputs });
    state.runs.push({ id: 9000 + state.runs.length, eval_id: b.inputs.eval_id, model: b.inputs.model || "", status: "queued", conclusion: null });
    res.writeHead(204);
    return res.end();
  }
  if (req.method === "GET" && /^actions\/workflows\/[\w.-]+\/runs$/.test(p)) {
    return send(res, 200, { total_count: state.runs.length, workflow_runs: state.runs.map(runJson) });
  }
  const jobs = p.match(/^actions\/runs\/(\d+)\/jobs$/);
  if (req.method === "GET" && jobs) {
    return send(res, 200, { jobs: [{ name: "setup", status: "completed" }, { name: "generate r1", status: "in_progress" }] });
  }
  const run = p.match(/^actions\/runs\/(\d+)$/);
  if (req.method === "GET" && run) {
    const r = state.runs.find((x) => x.id === Number(run[1]));
    return r ? send(res, 200, runJson(r)) : send(res, 404, {});
  }
  if (req.method === "GET" && p === "contents/tasksets") {
    return send(res, 200, [{ type: "dir", name: "github-full" }, { type: "file", name: "README.md" }]);
  }
  if (req.method === "GET" && p === "contents/tasksets/github-full/taskset.json") {
    return send(res, 200, { sha: "0123456789abcdef", content: Buffer.from(JSON.stringify(taskset)).toString("base64") });
  }
  send(res, 404, { message: `mock: no route ${req.method} ${p}` });
});

server.listen(PORT, "127.0.0.1", () => console.log(`mock GitHub on ${BASE}`));
