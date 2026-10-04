// Mirrors the Worker API contract and crucible-core's Manifest. Every field
// that may be absent in practice is optional so the UI can degrade.

export interface Me {
  github_id: number;
  login: string;
  is_admin: boolean;
}

export interface TaskStage {
  name: string;
  time_limit_s: number;
  /** Number of tests, for test-counting scorers; null otherwise. */
  total?: number | null;
}

export interface TaskSet {
  /** Built-in: the directory name; uploaded: the id `u-<16 hex>`. */
  name: string;
  version: string;
  stages: TaskStage[];
  // Uploaded tasksets only.
  title?: string | null;
  owner_login?: string | null;
  public?: boolean | null;
  status?: "packing" | "ready" | "failed" | null;
  error?: string | null;
  /** The taskset's own `display`, when it declares one. */
  display?: Display | null;
}

export type UploadKind = Mode | "taskset";

export type Mode = "agent" | "app";

export interface Budget {
  max_requests?: number;
  max_tokens?: number;
  max_cost_usd?: number;
}

export interface CreateEval {
  mode: Mode;
  eval_id: string;
  upload_hash: string;
  taskset: string;
  stages?: number;
  model?: string;
  replicas?: number;
  budget?: Budget;
  cred_envelope?: string;
  score_public: boolean;
  consent: true;
}

export interface EvalSummary {
  eval_id: string;
  mode: Mode;
  taskset: string;
  model?: string | null;
  created_at: string;
  status: string;
  total_score?: number | null;
  /** How to show total_score (the manifest's snapshot); absent = old 0–1 ratio. */
  display?: ScoreFormat | null;
}

export interface EvalDetail {
  eval_id: string;
  status: string;
  run_url?: string | null;
  manifest?: Partial<Manifest> | null;
}

// --- crucible-core::manifest ---

/** Result v2 (docs/plugins.md §7). */
export interface ScoreItem {
  name: string;
  score?: number | null;
  max?: number | null;
  passed?: boolean | null;
}

export interface StageScoreV2 {
  status: "scored" | "error";
  error?: "system" | "rejected" | null;
  score?: number | null;
  max?: number | null;
  passed?: boolean | null;
  items?: ScoreItem[] | null;
}

/** The format before v2, still found in old manifests (read, never rewritten). */
export interface StageScoreLegacy {
  status: "passed" | "failed" | "system_error" | "rejected";
  passed: number;
  total: number;
}

export type StageScore = StageScoreV2 | StageScoreLegacy;
export type ScoreStatus = StageScore["status"];

export interface ScoreFormat {
  name?: string;
  unit?: string;
  direction?: "higher" | "lower";
  min?: number | null;
  max?: number | null;
  decimals?: number;
  format?: "number" | "percent" | "fraction";
}

export interface Display {
  stage?: ScoreFormat | null;
  total?: ScoreFormat | null;
}

export interface Aggregate {
  items?: "sum" | "mean" | "weighted";
  item_weights?: Record<string, number>;
  stages?: "ratio" | "sum" | "mean" | "weighted";
  weights?: Record<string, number>;
  normalize?: boolean;
}

/** Snapshot written by publish; absent in older manifests. */
export interface Scoring {
  aggregate: Aggregate;
  display: Display;
  plugins: { kind: string; name: string; version: string }[];
}

export interface UsageTotals {
  requests: number;
  prompt_tokens: number;
  cached_tokens: number;
  completion_tokens: number;
  reasoning_tokens: number;
}

export interface BlobRef {
  sha256: string;
  key_id: string;
}

/** Model use of one scoring slot (docs/plugins.md §10). */
export interface SlotUsage {
  usage?: Partial<UsageTotals> | null;
  cost_usd?: number | null;
}

export interface StageEntry {
  stage: string;
  score?: StageScore | null;
  wall_s?: number | null;
  usage?: Partial<UsageTotals> | null;
  cost_usd?: number | null;
  /** Model use while scoring (interactive runner, judge), apart from `usage`. */
  eval_usage?: { interactive?: SlotUsage | null; scorer?: SlotUsage | null } | null;
  output?: BlobRef | null;
  logs?: BlobRef | null;
}

export interface ReplicaEntry {
  replica: number;
  stages?: StageEntry[] | null;
}

export interface Manifest {
  /** Present when a password zip of the outputs exists (agent mode with a download password). */
  download?: { sha256: string } | null;
  schema: number;
  eval_id: string;
  created_at: string;
  taskset: string;
  agent: { name: string; version: string; package?: BlobRef | null; commit?: string | null };
  model: string;
  public: boolean;
  replicas: ReplicaEntry[];
  scoring?: Scoring | null;
  total_score?: number | null;
}

// --- leaderboard (GET /leaderboard, GET /leaderboard/:taskset) ---

export interface LeaderboardInfo {
  taskset: string;
  /** Public, done evals on it. */
  evals: number;
  latest_at: string;
}

export interface LeaderboardStage {
  stage: string;
  /** Mean over replicas; null when no replica scored the stage. */
  score: number | null;
  max: number | null;
}

export interface LeaderboardEntry {
  rank: number;
  login: string;
  agent: string;
  agent_version: string;
  model?: string | null;
  total_score: number;
  stages: LeaderboardStage[];
  replicas: number;
  wall_s?: number | null;
  cost_usd?: number | null;
  created_at: string;
  eval_id: string;
}

export interface Leaderboard {
  taskset: string;
  direction: "higher" | "lower";
  /** Total's display (newest public eval's snapshot); absent = 0–1 ratio. */
  display?: ScoreFormat | null;
  stage_display?: ScoreFormat | null;
  entries: LeaderboardEntry[];
}

/** A personal API token for the command line (GET /tokens). */
export interface ApiToken {
  id: string;
  name: string;
  created_at: string;
}

/** POST /tokens: the plaintext token is returned only here, once. */
export interface NewApiToken extends ApiToken {
  token: string;
}

export interface ApiErrorBody {
  error: { code: string; message: string };
}
