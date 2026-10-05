-- Per-user quotas (docs/api.md "配额") and the reason an eval was failed by
-- the platform rather than by its workflow (the hourly sweep).
--
-- Defaults come from wrangler.toml [vars] QUOTA_*; a row here overrides
-- them for one user. A NULL limit keeps the default. `exempt`: NULL = the
-- default rule (administrators are exempt), 1 = exempt, 0 = never exempt
-- (also for an administrator).
CREATE TABLE quotas (
  github_id INTEGER PRIMARY KEY,
  uploads_per_day INTEGER,
  upload_bytes_per_day INTEGER,
  evals_running INTEGER,
  evals_per_day INTEGER,
  plugins_per_day INTEGER,
  tasksets_per_day INTEGER,
  exempt INTEGER,
  by_id INTEGER NOT NULL,
  at TEXT NOT NULL
);

-- Why the platform marked an eval failed (e.g. no status for too long).
ALTER TABLE evals ADD COLUMN error TEXT;
-- Quota counts: a user's uploads, plugins and tasksets of the last 24 h.
CREATE INDEX uploads_owner_created ON uploads (owner_id, created_at);
CREATE INDEX user_plugins_owner_created ON user_plugins (owner_id, created_at);
CREATE INDEX user_tasksets_owner_created ON user_tasksets (owner_id, created_at);
-- The sweep: unsettled registrations.
CREATE INDEX user_plugins_status ON user_plugins (status, created_at);
CREATE INDEX user_tasksets_status ON user_tasksets (status, created_at);
CREATE INDEX evals_status ON evals (status, updated_at);
