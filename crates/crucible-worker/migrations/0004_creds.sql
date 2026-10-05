-- Sealed model credentials (formerly Workers KV `cred/<eval_id>`, 24 h TTL).
-- A row past `expires_s` counts as absent; the Worker's hourly cron
-- (wrangler.toml [triggers]) deletes such rows. docs/api.md "D1 表结构".
CREATE TABLE creds (
  eval_id TEXT PRIMARY KEY,
  envelope TEXT NOT NULL,              -- the sealed envelope (JSON text), as submitted
  expires_s INTEGER NOT NULL           -- unix seconds
);
CREATE INDEX creds_expires ON creds (expires_s);
