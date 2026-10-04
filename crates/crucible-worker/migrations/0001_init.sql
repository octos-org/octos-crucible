-- octos-crucible Worker: D1 schema (binding CRUCIBLE_DB). docs/api.md "D1 表结构".
-- Applied with `wrangler d1 migrations apply crucible --remote` (CI deploy job)
-- and `--local` for `wrangler dev`. The native tests run this file on SQLite.
--
-- Booleans are 0/1, JSON values are TEXT, times are RFC 3339 TEXT plus unix
-- seconds where a query sorts by them.

-- Who uploaded a blob (the claim). An eval may only reference its owner's
-- uploads. Claimed with INSERT ... ON CONFLICT DO NOTHING.
CREATE TABLE uploads (
  hash TEXT PRIMARY KEY,               -- SHA-256 hex of the sealed bytes
  owner_id INTEGER NOT NULL,
  kind TEXT NOT NULL,                  -- agent | app | taskset
  size INTEGER NOT NULL,
  created_at TEXT NOT NULL
);

-- One row per eval. `status` is written on submit, on /internal/status and
-- when a refresh finds the run over; never once it is done/failed.
CREATE TABLE evals (
  eval_id TEXT PRIMARY KEY,
  owner_id INTEGER NOT NULL,
  owner_login TEXT NOT NULL,
  mode TEXT NOT NULL,                  -- agent | app
  upload_hash TEXT NOT NULL,
  taskset TEXT NOT NULL,
  stages INTEGER NOT NULL,
  stage_names TEXT NOT NULL,           -- JSON array
  model TEXT,
  replicas INTEGER NOT NULL,
  budget TEXT,                         -- JSON object
  score_public INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  created_s INTEGER NOT NULL,
  status TEXT NOT NULL,
  run_id INTEGER,
  run_url TEXT,
  run_completed_s INTEGER,
  updated_at TEXT NOT NULL
);
-- GET /evals (own list) and GET /evals?all=1 (admins), newest first.
CREATE INDEX evals_owner_created ON evals (owner_id, created_s DESC);
CREATE INDEX evals_created ON evals (created_s DESC);

-- What the workflow posted to /internal/results (one row per eval). The
-- list reads total_score/display from here without parsing manifests.
CREATE TABLE results (
  eval_id TEXT PRIMARY KEY,
  manifest TEXT NOT NULL,              -- JSON
  download_sha256 TEXT,
  status TEXT NOT NULL,
  total_score REAL,
  display TEXT,                        -- JSON (manifest scoring.display.total)
  updated_at TEXT NOT NULL
);

-- User-uploaded tasksets (u-<16 hex>).
CREATE TABLE user_tasksets (
  id TEXT PRIMARY KEY,
  owner_id INTEGER NOT NULL,
  owner_login TEXT NOT NULL,
  upload_hash TEXT NOT NULL,
  status TEXT NOT NULL,                -- packing | ready | failed
  error TEXT,
  public INTEGER NOT NULL DEFAULT 0,
  title TEXT,
  taskset TEXT,                        -- packed taskset.json (JSON)
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX user_tasksets_owner ON user_tasksets (owner_id);
CREATE INDEX user_tasksets_public ON user_tasksets (public, status);

-- Personal API tokens: only the SHA-256 of the token is stored.
CREATE TABLE tokens (
  id TEXT PRIMARY KEY,                 -- 16 hex, part of the token
  owner_id INTEGER NOT NULL,
  login TEXT NOT NULL,
  name TEXT NOT NULL,
  hash TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX tokens_owner ON tokens (owner_id);

CREATE TABLE bans (
  github_id INTEGER PRIMARY KEY,
  by_id INTEGER NOT NULL,
  at TEXT NOT NULL,
  reason TEXT
);

-- Caches of GitHub lookups: the built-in taskset list (5 min) and blob
-- release ids (no expiry: tags never move).
CREATE TABLE cache (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  expires_s INTEGER                    -- unix seconds; NULL = never
);
