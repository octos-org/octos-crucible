-- User-uploaded plugins (docs/plugins.md §14, docs/api.md "用户上传的插件").
CREATE TABLE user_plugins (
  id TEXT PRIMARY KEY,                 -- u-<16 hex>
  owner_id INTEGER NOT NULL,
  owner_login TEXT NOT NULL,
  upload_hash TEXT NOT NULL,
  status TEXT NOT NULL,                -- building | ready | failed
  error TEXT,
  public INTEGER NOT NULL DEFAULT 0,
  title TEXT,                          -- name from plugin.json
  plugin TEXT,                         -- pinned form (JSON): kind, version, blob, traits
  info TEXT,                           -- description, self-test result (JSON)
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX user_plugins_owner ON user_plugins (owner_id);
CREATE INDEX user_plugins_public ON user_plugins (public, status);
