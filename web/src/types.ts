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
  total: number;
}

export interface TaskSet {
  name: string;
  version: string;
  stages: TaskStage[];
}

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
}

export interface EvalDetail {
  eval_id: string;
  status: string;
  run_url?: string | null;
  manifest?: Partial<Manifest> | null;
}

// --- crucible-core::manifest ---

export type ScoreStatus = "passed" | "failed" | "system_error" | "rejected";

export interface StageScore {
  status: ScoreStatus;
  passed: number;
  total: number;
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

export interface StageEntry {
  stage: string;
  score?: StageScore | null;
  wall_s?: number | null;
  usage?: Partial<UsageTotals> | null;
  cost_usd?: number | null;
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
