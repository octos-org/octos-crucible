// Fixed data for developing without a Worker. Shapes follow the API contract.

import { ApiError, type Backend } from "./api";
import type { ApiToken, EvalDetail, EvalSummary, Manifest, StageEntry, TaskSet } from "./types";
import { currentKey } from "./keys";
import { toHex } from "./crypto";

const TASKSETS: TaskSet[] = [
  {
    name: "github-full",
    version: "1",
    stages: [
      { name: "stage-1", time_limit_s: 3600, total: 30 },
      { name: "stage-2", time_limit_s: 3600, total: 42 },
      { name: "stage-3", time_limit_s: 5400, total: 55 },
    ],
  },
  {
    name: "github-lite",
    version: "2",
    stages: [{ name: "stage-1", time_limit_s: 1800, total: 30 }],
  },
];

function stage(
  name: string,
  passed: number,
  total: number,
  wall: number,
  req: number,
  prompt: number,
  cached: number,
  completion: number,
  cost: number | null,
): StageEntry {
  return {
    stage: name,
    score: { status: passed === total ? "passed" : "failed", passed, total },
    wall_s: wall,
    usage: {
      requests: req,
      prompt_tokens: prompt,
      cached_tokens: cached,
      completion_tokens: completion,
      reasoning_tokens: 0,
    },
    cost_usd: cost,
    output: { sha256: "ab".repeat(32), key_id: "1ffa702796eb5ee8" },
  };
}

const DONE: Manifest = {
  schema: 1,
  eval_id: "7c1e2f5a-0b8d-4e57-9a51-3f2c1d0e9b11",
  created_at: "2026-10-02T08:12:00Z",
  taskset: "github-full",
  agent: { name: "my-agent", version: "0.3.0", commit: "4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39" },
  model: "glm-5.3",
  public: false,
  replicas: [
    {
      replica: 1,
      stages: [
        stage("stage-1", 27, 30, 1712, 141, 4_120_000, 3_310_000, 61_200, 2.31),
        stage("stage-2", 35, 42, 2950, 233, 7_900_000, 6_420_000, 98_400, 4.12),
        stage("stage-3", 38, 55, 5011, 318, 11_200_000, 8_870_000, 141_000, 6.02),
      ],
    },
    {
      replica: 2,
      stages: [
        stage("stage-1", 30, 30, 1530, 122, 3_850_000, 3_120_000, 55_900, 2.09),
        stage("stage-2", 33, 42, 3102, 251, 8_300_000, 6_590_000, 104_000, 4.41),
        stage("stage-3", 41, 55, 4870, 296, 10_700_000, 8_640_000, 133_500, 5.71),
      ],
    },
    {
      replica: 3,
      stages: [
        stage("stage-1", 26, 30, 1801, 150, 4_400_000, 3_500_000, 64_000, 2.45),
        stage("stage-2", 36, 42, 2804, 219, 7_600_000, 6_200_000, 95_000, 3.97),
        {
          stage: "stage-3",
          score: { status: "system_error", passed: 0, total: 0 },
          wall_s: 5400,
          usage: { requests: 340, prompt_tokens: 12_000_000, cached_tokens: 9_100_000, completion_tokens: 150_000, reasoning_tokens: 0 },
          cost_usd: 6.48,
        },
      ],
    },
  ],
};

// The same agent after a change: better score, fewer tokens, a bit slower.
const TUNED: Manifest = {
  schema: 1,
  eval_id: "5b9e1c3d-7a2f-4d68-b0e4-9c8a7f6e5d21",
  created_at: "2026-10-02T20:45:00Z",
  taskset: "github-full",
  agent: { name: "my-agent", version: "0.3.1", commit: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678" },
  model: "glm-5.3",
  public: false,
  replicas: [
    {
      replica: 1,
      stages: [
        stage("stage-1", 29, 30, 1790, 118, 3_400_000, 2_980_000, 52_000, 1.82),
        stage("stage-2", 37, 42, 3010, 201, 6_900_000, 6_010_000, 90_100, 3.44),
        stage("stage-3", 44, 55, 5120, 270, 9_800_000, 8_500_000, 128_000, 4.95),
      ],
    },
    {
      replica: 2,
      stages: [
        stage("stage-1", 30, 30, 1702, 110, 3_300_000, 2_900_000, 50_400, 1.74),
        stage("stage-2", 38, 42, 3150, 214, 7_100_000, 6_200_000, 93_000, 3.58),
        stage("stage-3", 42, 55, 5003, 262, 9_500_000, 8_300_000, 125_000, 4.80),
      ],
    },
    {
      replica: 3,
      stages: [
        stage("stage-1", 28, 30, 1655, 121, 3_500_000, 3_050_000, 53_800, 1.88),
        stage("stage-2", 36, 42, 2990, 208, 7_000_000, 6_100_000, 91_500, 3.51),
        stage("stage-3", 45, 55, 5230, 281, 10_100_000, 8_800_000, 131_000, 5.10),
      ],
    },
  ],
};

// A quick dev run of the first two stages only.
const QUICK: Manifest = {
  schema: 1,
  eval_id: "c6d7e8f9-0a1b-4c2d-9e3f-4a5b6c7d8e90",
  created_at: "2026-10-02T23:10:00Z",
  taskset: "github-full",
  agent: { name: "my-agent", version: "0.3.2", commit: "9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a291807" },
  model: "glm-5.3",
  public: false,
  replicas: [
    {
      replica: 1,
      stages: [
        stage("stage-1", 30, 30, 1580, 104, 3_100_000, 2_800_000, 47_000, 1.61),
        stage("stage-2", 39, 42, 2870, 190, 6_500_000, 5_800_000, 86_000, 3.21),
      ],
    },
  ],
};

const KIMI: Manifest = {
  schema: 1,
  eval_id: "d40c9a7e-5b6f-4a21-8e3d-6a9b2c7f1e04",
  created_at: "2026-10-01T14:40:00Z",
  taskset: "github-lite",
  agent: { name: "octos", version: "1" },
  model: "kimi-k3",
  public: true,
  replicas: [
    { replica: 1, stages: [stage("stage-1", 24, 30, 1490, 98, 2_900_000, 1_700_000, 41_000, null)] },
  ],
};

const RUNNING: Partial<Manifest> = {
  eval_id: "3a8f0d61-2c4e-4b9a-a7d5-81e6f0c2b3d9",
  taskset: "github-full",
  model: "glm-5.3-flash",
  replicas: [
    { replica: 1, stages: [stage("stage-1", 22, 30, 1650, 180, 5_000_000, 3_900_000, 70_000, 0.33)] },
  ],
};

let EVALS: EvalSummary[] = [
  { eval_id: RUNNING.eval_id!, mode: "agent", taskset: "github-full", model: "glm-5.3-flash", created_at: "2026-10-03T06:30:00Z", status: "running:stage-2" },
  { eval_id: "b2e7c4d0-9f1a-4c63-8d27-5e0a1b3c4f68", mode: "app", taskset: "github-full", model: null, created_at: "2026-10-03T05:02:00Z", status: "scoring" },
  { eval_id: "e9d3b1a2-7c5f-4e08-b6a4-2d1c0f9e8a75", mode: "agent", taskset: "github-full", model: "glm-5", created_at: "2026-10-03T04:10:00Z", status: "queued" },
  { eval_id: QUICK.eval_id, mode: "agent", taskset: "github-full", model: "glm-5.3", created_at: QUICK.created_at, status: "done", total_score: 0.9583 },
  { eval_id: TUNED.eval_id, mode: "agent", taskset: "github-full", model: "glm-5.3", created_at: TUNED.created_at, status: "done", total_score: 0.8635 },
  { eval_id: DONE.eval_id, mode: "agent", taskset: "github-full", model: "glm-5.3", created_at: DONE.created_at, status: "done", total_score: 0.775 },
  { eval_id: KIMI.eval_id, mode: "agent", taskset: "github-lite", model: "kimi-k3", created_at: KIMI.created_at, status: "done", total_score: 0.8 },
  { eval_id: "0f5a6b7c-8d9e-4f10-a1b2-c3d4e5f60718", mode: "agent", taskset: "github-full", model: "glm-4.7", created_at: "2026-09-30T10:00:00Z", status: "failed" },
];

const DETAILS: Record<string, EvalDetail> = {
  [DONE.eval_id]: { eval_id: DONE.eval_id, status: "done", run_url: "https://github.com/octos-org/octos-crucible/actions", manifest: DONE },
  [TUNED.eval_id]: { eval_id: TUNED.eval_id, status: "done", manifest: TUNED },
  [QUICK.eval_id]: { eval_id: QUICK.eval_id, status: "done", manifest: QUICK },
  [KIMI.eval_id]: { eval_id: KIMI.eval_id, status: "done", manifest: KIMI },
  [RUNNING.eval_id!]: { eval_id: RUNNING.eval_id!, status: "running:stage-2", run_url: "https://github.com/octos-org/octos-crucible/actions", manifest: RUNNING },
};

let TOKENS: ApiToken[] = [];

const delay = <T>(v: T, ms = 250) => new Promise<T>((r) => setTimeout(() => r(structuredClone(v)), ms));

export const mockBackend: Backend = {
  me: () => delay({ github_id: 1, login: "demo-user", is_admin: false }),
  pubkey: () => delay(currentKey()),
  tasksets: () => delay(TASKSETS),
  async upload(_kind, sealed) {
    const d = await crypto.subtle.digest("SHA-256", sealed as BufferSource);
    return delay({ hash: toHex(new Uint8Array(d)) }, 600);
  },
  async createEval(body) {
    EVALS = [
      {
        eval_id: body.eval_id,
        mode: body.mode,
        taskset: body.taskset,
        model: body.model ?? null,
        created_at: new Date().toISOString(),
        status: "queued",
      },
      ...EVALS,
    ];
    return delay({ eval_id: body.eval_id });
  },
  evals: () => delay(EVALS),
  evalDetail(id) {
    const d = DETAILS[id];
    if (d) return delay(d);
    const s = EVALS.find((e) => e.eval_id === id);
    if (!s) return Promise.reject(new ApiError(404, "not_found", "评测不存在"));
    return delay({ eval_id: id, status: s.status });
  },
  tokens: () => delay(TOKENS),
  async createToken(name) {
    const id = toHex(crypto.getRandomValues(new Uint8Array(8)));
    const t = { id, name: name.trim() || "cli", created_at: new Date().toISOString() };
    TOKENS = [t, ...TOKENS];
    return delay({ ...t, token: `crt_${id}_${toHex(crypto.getRandomValues(new Uint8Array(32)))}` });
  },
  async deleteToken(id) {
    TOKENS = TOKENS.filter((t) => t.id !== id);
    await delay(null);
  },
  async download() {
    await delay(null);
    alert("演示模式：真实环境会下载加密 zip。");
  },
};
