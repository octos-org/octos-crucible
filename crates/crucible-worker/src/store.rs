//! Typed access to the D1 tables (`migrations/0001_init.sql`).
//!
//! Idempotence and concurrency come from the SQL itself: claims are
//! `INSERT ... ON CONFLICT DO NOTHING` followed by a read, status updates
//! carry `WHERE status NOT IN ('done','failed')`, and one-shot transitions
//! (taskset packing) carry the state they leave in their `WHERE`. Errors are
//! the backend's strings; the handlers turn them into API errors.

use serde_json::Value;

use crate::http::{Backend, Row, SqlArg, Stmt};
use crate::leaderboard::{BoardInfo, Candidate};
use crate::model::{
    Budget, EvalRecord, EvalSummary, Mode, StoredResults, UploadKind, UploadRecord,
    UserPluginRecord, UserTaskset, score_of, shown_status,
};
use crate::quota::{DAY_S, Override, Usage, Used};
use crate::tokens::{TokenRecord, TokenSummary};
use crate::util::{parse_rfc3339, rfc3339};
use crucible_core::taskset::ScoreFormat;

type R<T> = Result<T, String>;

impl From<&str> for SqlArg {
    fn from(v: &str) -> Self {
        SqlArg::Text(v.to_owned())
    }
}
impl From<&String> for SqlArg {
    fn from(v: &String) -> Self {
        SqlArg::Text(v.clone())
    }
}
impl From<String> for SqlArg {
    fn from(v: String) -> Self {
        SqlArg::Text(v)
    }
}
impl From<u64> for SqlArg {
    fn from(v: u64) -> Self {
        SqlArg::Int(v as i64)
    }
}
impl From<u32> for SqlArg {
    fn from(v: u32) -> Self {
        SqlArg::Int(v.into())
    }
}
impl From<bool> for SqlArg {
    fn from(v: bool) -> Self {
        SqlArg::Int(v.into())
    }
}
impl From<f64> for SqlArg {
    fn from(v: f64) -> Self {
        SqlArg::Real(v)
    }
}
impl<T: Into<SqlArg>> From<Option<T>> for SqlArg {
    fn from(v: Option<T>) -> Self {
        v.map_or(SqlArg::Null, Into::into)
    }
}

macro_rules! args {
    ($($v:expr),* $(,)?) => { vec![$(SqlArg::from($v)),*] };
}

fn json_text<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("json")
}

// ---- reading rows ------------------------------------------------------

fn text(r: &Row, c: &str) -> Option<String> {
    r.get(c)?.as_str().map(str::to_owned)
}

fn int(r: &Row, c: &str) -> Option<i64> {
    let v = r.get(c)?;
    // D1 hands numbers to Rust as JS numbers (f64 when not a safe integer).
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

fn uint(r: &Row, c: &str) -> Option<u64> {
    int(r, c).and_then(|v| u64::try_from(v).ok())
}

fn real(r: &Row, c: &str) -> Option<f64> {
    r.get(c)?.as_f64()
}

fn json_col<T: for<'de> serde::Deserialize<'de>>(r: &Row, c: &str) -> Option<T> {
    serde_json::from_str(&text(r, c)?).ok()
}

fn need<T>(v: Option<T>, table: &str, c: &str) -> R<T> {
    v.ok_or_else(|| format!("corrupt row in {table}: {c}"))
}

fn upload_of(r: &Row) -> R<UploadRecord> {
    Ok(UploadRecord {
        owner_id: need(uint(r, "owner_id"), "uploads", "owner_id")?,
        kind: need(
            text(r, "kind").as_deref().and_then(UploadKind::parse),
            "uploads",
            "kind",
        )?,
        size: uint(r, "size").unwrap_or(0),
        created_at: text(r, "created_at").unwrap_or_default(),
    })
}

fn eval_of(r: &Row) -> R<EvalRecord> {
    let t = "evals";
    Ok(EvalRecord {
        eval_id: need(text(r, "eval_id"), t, "eval_id")?,
        owner_id: need(uint(r, "owner_id"), t, "owner_id")?,
        owner_login: need(text(r, "owner_login"), t, "owner_login")?,
        mode: need(text(r, "mode").as_deref().and_then(Mode::parse), t, "mode")?,
        upload_hash: need(text(r, "upload_hash"), t, "upload_hash")?,
        taskset: need(text(r, "taskset"), t, "taskset")?,
        stages: need(uint(r, "stages"), t, "stages")? as u32,
        stage_names: json_col(r, "stage_names").unwrap_or_default(),
        model: text(r, "model"),
        replicas: need(uint(r, "replicas"), t, "replicas")? as u32,
        budget: json_col::<Budget>(r, "budget"),
        score_public: int(r, "score_public") == Some(1),
        created_at: need(text(r, "created_at"), t, "created_at")?,
        created_s: uint(r, "created_s").unwrap_or(0),
        status: need(text(r, "status"), t, "status")?,
        run_id: uint(r, "run_id"),
        run_url: text(r, "run_url"),
        run_completed_s: uint(r, "run_completed_s"),
        manifest: None,
        download_sha256: None,
        updated_at: need(text(r, "updated_at"), t, "updated_at")?,
        error: text(r, "error"),
    })
}

fn results_of(r: &Row) -> R<StoredResults> {
    Ok(StoredResults {
        manifest: need(json_col::<Value>(r, "manifest"), "results", "manifest")?,
        download_sha256: text(r, "download_sha256"),
        status: need(text(r, "status"), "results", "status")?,
        updated_at: text(r, "updated_at").unwrap_or_default(),
    })
}

fn taskset_of(r: &Row) -> R<UserTaskset> {
    let t = "user_tasksets";
    Ok(UserTaskset {
        id: need(text(r, "id"), t, "id")?,
        owner_id: need(uint(r, "owner_id"), t, "owner_id")?,
        owner_login: need(text(r, "owner_login"), t, "owner_login")?,
        upload_hash: need(text(r, "upload_hash"), t, "upload_hash")?,
        status: need(text(r, "status"), t, "status")?,
        error: text(r, "error"),
        public: int(r, "public") == Some(1),
        title: text(r, "title"),
        taskset: json_col(r, "taskset"),
        created_at: text(r, "created_at").unwrap_or_default(),
        updated_at: text(r, "updated_at").unwrap_or_default(),
    })
}

fn plugin_of(r: &Row) -> R<UserPluginRecord> {
    let t = "user_plugins";
    Ok(UserPluginRecord {
        id: need(text(r, "id"), t, "id")?,
        owner_id: need(uint(r, "owner_id"), t, "owner_id")?,
        owner_login: need(text(r, "owner_login"), t, "owner_login")?,
        upload_hash: need(text(r, "upload_hash"), t, "upload_hash")?,
        status: need(text(r, "status"), t, "status")?,
        error: text(r, "error"),
        public: int(r, "public") == Some(1),
        title: text(r, "title"),
        plugin: json_col(r, "plugin"),
        info: json_col(r, "info"),
        review: json_col(r, "review"),
        approval: json_col(r, "approval"),
        created_at: text(r, "created_at").unwrap_or_default(),
        updated_at: text(r, "updated_at").unwrap_or_default(),
    })
}

fn token_of(r: &Row) -> R<TokenRecord> {
    Ok(TokenRecord {
        owner_id: need(uint(r, "owner_id"), "tokens", "owner_id")?,
        login: need(text(r, "login"), "tokens", "login")?,
        name: text(r, "name").unwrap_or_default(),
        hash: need(text(r, "hash"), "tokens", "hash")?,
        created_at: text(r, "created_at").unwrap_or_default(),
    })
}

/// Leaderboard rows: public evals (the partial index), done and scored, of
/// owners not banned, on built-in or public user tasksets.
const PUBLIC_DONE: &str = "FROM evals e JOIN results r ON r.eval_id = e.eval_id \
     LEFT JOIN bans b ON b.github_id = e.owner_id \
     LEFT JOIN user_tasksets t ON t.id = e.taskset \
     WHERE e.score_public = 1 AND r.status = 'done' AND r.total_score IS NOT NULL \
     AND b.github_id IS NULL AND (t.id IS NULL OR (t.public = 1 AND t.status = 'ready'))";

// ---- statements ------------------------------------------------------------

const INSERT_UPLOAD: &str = "INSERT INTO uploads (hash, owner_id, kind, size, created_at) \
     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (hash) DO NOTHING";

pub fn insert_upload(hash: &str, u: &UploadRecord) -> Stmt {
    Stmt {
        sql: INSERT_UPLOAD,
        args: args![hash, u.owner_id, u.kind.as_str(), u.size, &u.created_at],
    }
}

const INSERT_EVAL: &str = "INSERT INTO evals (eval_id, owner_id, owner_login, mode, upload_hash, \
     taskset, stages, stage_names, model, replicas, budget, score_public, created_at, created_s, \
     status, run_id, run_url, run_completed_s, updated_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19) \
     ON CONFLICT (eval_id) DO NOTHING";

pub fn insert_eval(e: &EvalRecord) -> Stmt {
    Stmt {
        sql: INSERT_EVAL,
        args: args![
            &e.eval_id,
            e.owner_id,
            &e.owner_login,
            e.mode.as_str(),
            &e.upload_hash,
            &e.taskset,
            e.stages,
            json_text(&e.stage_names),
            e.model.as_ref(),
            e.replicas,
            e.budget.as_ref().map(json_text),
            e.score_public,
            &e.created_at,
            e.created_s,
            &e.status,
            e.run_id,
            e.run_url.as_ref(),
            e.run_completed_s,
            &e.updated_at,
        ],
    }
}

const INSERT_RESULTS: &str = "INSERT INTO results (eval_id, manifest, download_sha256, status, \
     total_score, display, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
     ON CONFLICT (eval_id) DO NOTHING";

/// A terminal result is only ever replaced by another terminal one (a
/// rerun's final results), never by a late partial delivery.
const UPSERT_RESULTS: &str = "INSERT INTO results (eval_id, manifest, download_sha256, status, \
     total_score, display, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
     ON CONFLICT (eval_id) DO UPDATE SET manifest = excluded.manifest, \
     download_sha256 = excluded.download_sha256, status = excluded.status, \
     total_score = excluded.total_score, display = excluded.display, \
     updated_at = excluded.updated_at \
     WHERE results.status NOT IN ('done', 'failed') OR excluded.status IN ('done', 'failed')";

fn results_stmt(sql: &'static str, eval_id: &str, r: &StoredResults) -> Stmt {
    let (score, display) = score_of(&r.manifest);
    Stmt {
        sql,
        args: args![
            eval_id,
            json_text(&r.manifest),
            r.download_sha256.as_ref(),
            &r.status,
            score,
            display.as_ref().map(json_text),
            &r.updated_at,
        ],
    }
}

pub fn insert_results(eval_id: &str, r: &StoredResults) -> Stmt {
    results_stmt(INSERT_RESULTS, eval_id, r)
}

const INSERT_TASKSET: &str = "INSERT INTO user_tasksets (id, owner_id, owner_login, upload_hash, \
     status, error, public, title, taskset, created_at, updated_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) ON CONFLICT (id) DO NOTHING";

pub fn insert_taskset(u: &UserTaskset) -> Stmt {
    Stmt {
        sql: INSERT_TASKSET,
        args: args![
            &u.id,
            u.owner_id,
            &u.owner_login,
            &u.upload_hash,
            &u.status,
            u.error.as_ref(),
            u.public,
            u.title.as_ref(),
            u.taskset.as_ref().map(json_text),
            &u.created_at,
            &u.updated_at,
        ],
    }
}

const INSERT_TOKEN: &str = "INSERT INTO tokens (id, owner_id, login, name, hash, created_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (id) DO NOTHING";

pub fn insert_token(id: &str, t: &TokenRecord) -> Stmt {
    Stmt {
        sql: INSERT_TOKEN,
        args: args![id, t.owner_id, &t.login, &t.name, &t.hash, &t.created_at],
    }
}

const INSERT_BAN: &str = "INSERT INTO bans (github_id, by_id, at, reason) VALUES (?1, ?2, ?3, ?4) \
     ON CONFLICT (github_id) DO NOTHING";

pub fn insert_ban(github_id: u64, by: u64, at: &str, reason: Option<&str>) -> Stmt {
    Stmt {
        sql: INSERT_BAN,
        args: args![github_id, by, at, reason],
    }
}

/// Outcome of claiming an upload hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// This request created the claim.
    New,
    /// The caller claimed these bytes before (a retry).
    Own,
    /// Someone else's.
    Other,
}

pub struct Db<'a, B: Backend>(pub &'a B);

impl<B: Backend> Db<'_, B> {
    async fn rows(&self, sql: &str, args: Vec<SqlArg>) -> R<Vec<Row>> {
        self.0.db_query(sql, &args).await
    }

    async fn first(&self, sql: &str, args: Vec<SqlArg>) -> R<Option<Row>> {
        Ok(self.rows(sql, args).await?.into_iter().next())
    }

    async fn exec(&self, sql: &str, args: Vec<SqlArg>) -> R<u64> {
        self.0.db_exec(sql, &args).await
    }

    async fn run(&self, s: Stmt) -> R<u64> {
        self.0.db_exec(s.sql, &s.args).await
    }

    // ---- cache ----------------------------------------------------------

    pub async fn cache_get(&self, key: &str, now: u64) -> R<Option<String>> {
        let row = self
            .first(
                "SELECT value FROM cache WHERE key = ?1 AND (expires_s IS NULL OR expires_s > ?2)",
                args![key, now],
            )
            .await?;
        Ok(row.and_then(|r| text(&r, "value")))
    }

    pub async fn cache_put(&self, key: &str, value: &str, expires_s: Option<u64>) -> R<()> {
        self.exec(
            "INSERT INTO cache (key, value, expires_s) VALUES (?1, ?2, ?3) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, expires_s = excluded.expires_s",
            args![key, value, expires_s],
        )
        .await
        .map(drop)
    }

    // ---- sealed credentials ---------------------------------------------

    /// The sealed credential of an eval; an expired row counts as absent
    /// (the hourly cron deletes it later).
    pub async fn cred(&self, eval_id: &str, now: u64) -> R<Option<String>> {
        let row = self
            .first(
                "SELECT envelope FROM creds WHERE eval_id = ?1 AND expires_s > ?2",
                args![eval_id, now],
            )
            .await?;
        Ok(row.and_then(|r| text(&r, "envelope")))
    }

    pub async fn put_cred(&self, eval_id: &str, envelope: &str, expires_s: u64) -> R<()> {
        self.exec(
            "INSERT INTO creds (eval_id, envelope, expires_s) VALUES (?1, ?2, ?3) \
             ON CONFLICT (eval_id) DO UPDATE SET envelope = excluded.envelope, \
             expires_s = excluded.expires_s",
            args![eval_id, envelope, expires_s],
        )
        .await
        .map(drop)
    }

    /// Deleting a missing row writes nothing, so callers need not check.
    pub async fn delete_cred(&self, eval_id: &str) -> R<()> {
        self.exec("DELETE FROM creds WHERE eval_id = ?1", args![eval_id])
            .await
            .map(drop)
    }

    /// Cron: drops expired credentials and cache entries; returns the
    /// number of rows deleted.
    pub async fn purge_expired(&self, now: u64) -> R<u64> {
        let creds = self
            .exec("DELETE FROM creds WHERE expires_s <= ?1", args![now])
            .await?;
        let cache = self
            .exec(
                "DELETE FROM cache WHERE expires_s IS NOT NULL AND expires_s <= ?1",
                args![now],
            )
            .await?;
        Ok(creds + cache)
    }

    // ---- quotas ---------------------------------------------------------

    pub async fn quota_override(&self, github_id: u64) -> R<Option<Override>> {
        let Some(r) = self
            .first(
                "SELECT * FROM quotas WHERE github_id = ?1",
                args![github_id],
            )
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Override {
            uploads_per_day: uint(&r, "uploads_per_day"),
            upload_bytes_per_day: uint(&r, "upload_bytes_per_day"),
            evals_running: uint(&r, "evals_running"),
            evals_per_day: uint(&r, "evals_per_day"),
            plugins_per_day: uint(&r, "plugins_per_day"),
            tasksets_per_day: uint(&r, "tasksets_per_day"),
            exempt: int(&r, "exempt").map(|v| v == 1),
        }))
    }

    /// Replaces a user's override row; an empty override deletes it.
    pub async fn set_quota_override(
        &self,
        github_id: u64,
        o: &Override,
        by: u64,
        at: &str,
    ) -> R<()> {
        if o.is_empty() {
            return self
                .exec("DELETE FROM quotas WHERE github_id = ?1", args![github_id])
                .await
                .map(drop);
        }
        self.exec(
            "INSERT INTO quotas (github_id, uploads_per_day, upload_bytes_per_day, evals_running, \
             evals_per_day, plugins_per_day, tasksets_per_day, exempt, by_id, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT (github_id) DO UPDATE SET uploads_per_day = excluded.uploads_per_day, \
             upload_bytes_per_day = excluded.upload_bytes_per_day, \
             evals_running = excluded.evals_running, evals_per_day = excluded.evals_per_day, \
             plugins_per_day = excluded.plugins_per_day, \
             tasksets_per_day = excluded.tasksets_per_day, exempt = excluded.exempt, \
             by_id = excluded.by_id, at = excluded.at",
            args![
                github_id,
                o.uploads_per_day,
                o.upload_bytes_per_day,
                o.evals_running,
                o.evals_per_day,
                o.plugins_per_day,
                o.tasksets_per_day,
                o.exempt,
                by,
                at,
            ],
        )
        .await
        .map(drop)
    }

    /// What a user has used: the last 24 hours (from `now`), and the evals
    /// not settled. One query.
    pub async fn usage(&self, github_id: u64, now: u64) -> R<Usage> {
        let since_s = now.saturating_sub(DAY_S);
        let since = rfc3339(since_s);
        let r = self
            .first(
                "SELECT \
                 (SELECT COUNT(*) FROM uploads WHERE owner_id = ?1 AND created_at > ?2) AS up_n, \
                 (SELECT COALESCE(SUM(size), 0) FROM uploads WHERE owner_id = ?1 AND created_at > ?2) AS up_b, \
                 (SELECT MIN(created_at) FROM uploads WHERE owner_id = ?1 AND created_at > ?2) AS up_t, \
                 (SELECT COUNT(*) FROM evals e LEFT JOIN results r ON r.eval_id = e.eval_id \
                   WHERE e.owner_id = ?1 AND e.status NOT IN ('done', 'failed') \
                   AND (r.status IS NULL OR r.status NOT IN ('done', 'failed'))) AS ev_run, \
                 (SELECT COUNT(*) FROM evals WHERE owner_id = ?1 AND created_s > ?3) AS ev_n, \
                 (SELECT MIN(created_s) FROM evals WHERE owner_id = ?1 AND created_s > ?3) AS ev_t, \
                 (SELECT COUNT(*) FROM user_plugins WHERE owner_id = ?1 AND created_at > ?2) AS pl_n, \
                 (SELECT MIN(created_at) FROM user_plugins WHERE owner_id = ?1 AND created_at > ?2) AS pl_t, \
                 (SELECT COUNT(*) FROM user_tasksets WHERE owner_id = ?1 AND created_at > ?2) AS ts_n, \
                 (SELECT MIN(created_at) FROM user_tasksets WHERE owner_id = ?1 AND created_at > ?2) AS ts_t",
                args![github_id, since, since_s],
            )
            .await?
            .unwrap_or_default();
        let frees = |t: Option<u64>| t.map(|t| t + DAY_S);
        let at = |c: &str| frees(text(&r, c).as_deref().and_then(parse_rfc3339));
        let n = |c: &str| uint(&r, c).unwrap_or(0);
        Ok(Usage([
            Used {
                used: n("up_n"),
                frees_at: at("up_t"),
            },
            Used {
                used: n("up_b"),
                frees_at: at("up_t"),
            },
            Used {
                used: n("ev_run"),
                frees_at: None,
            },
            Used {
                used: n("ev_n"),
                frees_at: frees(uint(&r, "ev_t")),
            },
            Used {
                used: n("pl_n"),
                frees_at: at("pl_t"),
            },
            Used {
                used: n("ts_n"),
                frees_at: at("ts_t"),
            },
        ]))
    }

    // ---- the sweep (hourly cron) ----------------------------------------

    /// Fails taskset and plugin registrations still unsettled `older` than
    /// their creation; returns how many.
    pub async fn fail_stuck_registrations(&self, now: u64, older: u64) -> R<u64> {
        let before = rfc3339(now.saturating_sub(older));
        let at = rfc3339(now);
        let hours = older / 3600;
        let ts = self
            .exec(
                "UPDATE user_tasksets SET status = 'failed', error = ?3, updated_at = ?2 \
                 WHERE status = 'packing' AND created_at < ?1",
                args![
                    &before,
                    &at,
                    format!("packing timed out: no result from the packing workflow within {hours} h; please upload again / 打包超时（{hours} 小时内没有收到结果），请重新上传")
                ],
            )
            .await?;
        let pl = self
            .exec(
                "UPDATE user_plugins SET status = 'failed', error = ?3, updated_at = ?2 \
                 WHERE status = 'building' AND created_at < ?1",
                args![
                    &before,
                    &at,
                    format!("building timed out: no result from the build workflow within {hours} h; please upload again / 构建超时（{hours} 小时内没有收到结果），请重新上传")
                ],
            )
            .await?;
        Ok(ts + pl)
    }

    /// Evals not settled (neither the record nor posted results) whose last
    /// update is before `before`, oldest first.
    pub async fn unsettled_evals(&self, before: &str, limit: usize) -> R<Vec<EvalRecord>> {
        let rows = self
            .rows(
                "SELECT e.* FROM evals e LEFT JOIN results r ON r.eval_id = e.eval_id \
                 WHERE e.status NOT IN ('done', 'failed') \
                 AND (r.status IS NULL OR r.status NOT IN ('done', 'failed')) \
                 AND e.updated_at < ?1 AND (r.updated_at IS NULL OR r.updated_at < ?1) \
                 ORDER BY e.updated_at LIMIT ?2",
                args![before, limit as u64],
            )
            .await?;
        rows.iter().map(eval_of).collect()
    }

    /// Fails an eval that has not settled, with the reason; false if it had.
    pub async fn fail_eval(&self, id: &str, error: &str, at: &str) -> R<bool> {
        Ok(self
            .exec(
                "UPDATE evals SET status = 'failed', error = ?2, updated_at = ?3 \
                 WHERE eval_id = ?1 AND status NOT IN ('done', 'failed')",
                args![id, error, at],
            )
            .await?
            == 1)
    }

    // ---- bans -----------------------------------------------------------

    pub async fn is_banned(&self, github_id: u64) -> R<bool> {
        Ok(self
            .first(
                "SELECT 1 AS x FROM bans WHERE github_id = ?1",
                args![github_id],
            )
            .await?
            .is_some())
    }

    pub async fn ban(&self, github_id: u64, by: u64, at: &str, reason: Option<&str>) -> R<()> {
        self.exec(
            "INSERT INTO bans (github_id, by_id, at, reason) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (github_id) DO UPDATE SET by_id = excluded.by_id, at = excluded.at, \
             reason = excluded.reason",
            args![github_id, by, at, reason],
        )
        .await
        .map(drop)
    }

    pub async fn unban(&self, github_id: u64) -> R<()> {
        self.exec("DELETE FROM bans WHERE github_id = ?1", args![github_id])
            .await
            .map(drop)
    }

    // ---- tokens ---------------------------------------------------------

    pub async fn token(&self, id: &str) -> R<Option<TokenRecord>> {
        self.first("SELECT * FROM tokens WHERE id = ?1", args![id])
            .await?
            .map(|r| token_of(&r))
            .transpose()
    }

    pub async fn token_count(&self, owner_id: u64) -> R<u64> {
        let row = self
            .first(
                "SELECT COUNT(*) AS n FROM tokens WHERE owner_id = ?1",
                args![owner_id],
            )
            .await?;
        Ok(row.and_then(|r| uint(&r, "n")).unwrap_or(0))
    }

    /// False if the id is taken.
    pub async fn add_token(&self, id: &str, t: &TokenRecord) -> R<bool> {
        Ok(self.run(insert_token(id, t)).await? == 1)
    }

    pub async fn tokens_of(&self, owner_id: u64) -> R<Vec<TokenSummary>> {
        let rows = self
            .rows(
                "SELECT id, name, created_at FROM tokens WHERE owner_id = ?1 \
                 ORDER BY created_at DESC, id",
                args![owner_id],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(TokenSummary {
                    id: text(r, "id")?,
                    name: text(r, "name")?,
                    created_at: text(r, "created_at")?,
                })
            })
            .collect())
    }

    /// False if there is no such token of this owner.
    pub async fn delete_token(&self, id: &str, owner_id: u64) -> R<bool> {
        Ok(self
            .exec(
                "DELETE FROM tokens WHERE id = ?1 AND owner_id = ?2",
                args![id, owner_id],
            )
            .await?
            == 1)
    }

    // ---- uploads --------------------------------------------------------

    pub async fn upload(&self, hash: &str) -> R<Option<UploadRecord>> {
        self.first("SELECT * FROM uploads WHERE hash = ?1", args![hash])
            .await?
            .map(|r| upload_of(&r))
            .transpose()
    }

    pub async fn claim_upload(&self, hash: &str, u: &UploadRecord) -> R<Claim> {
        if self.run(insert_upload(hash, u)).await? == 1 {
            return Ok(Claim::New);
        }
        match self.upload(hash).await? {
            Some(rec) if rec.owner_id == u.owner_id => Ok(Claim::Own),
            Some(_) => Ok(Claim::Other),
            // Withdrawn in between: report it as taken; a retry claims anew.
            None => Ok(Claim::Other),
        }
    }

    pub async fn withdraw_upload(&self, hash: &str, owner_id: u64) -> R<()> {
        self.exec(
            "DELETE FROM uploads WHERE hash = ?1 AND owner_id = ?2",
            args![hash, owner_id],
        )
        .await
        .map(drop)
    }

    // ---- user tasksets --------------------------------------------------

    pub async fn taskset(&self, id: &str) -> R<Option<UserTaskset>> {
        self.first("SELECT * FROM user_tasksets WHERE id = ?1", args![id])
            .await?
            .map(|r| taskset_of(&r))
            .transpose()
    }

    /// False if the id is taken.
    pub async fn add_taskset(&self, u: &UserTaskset) -> R<bool> {
        Ok(self.run(insert_taskset(u)).await? == 1)
    }

    /// Records the packing outcome; false unless it was still `packing`.
    pub async fn finish_packing(&self, u: &UserTaskset) -> R<bool> {
        Ok(self
            .exec(
                "UPDATE user_tasksets SET status = ?2, error = ?3, title = ?4, taskset = ?5, \
                 updated_at = ?6 WHERE id = ?1 AND status = 'packing'",
                args![
                    &u.id,
                    &u.status,
                    u.error.as_ref(),
                    u.title.as_ref(),
                    u.taskset.as_ref().map(json_text),
                    &u.updated_at,
                ],
            )
            .await?
            == 1)
    }

    pub async fn set_taskset_public(&self, id: &str, public: bool, at: &str) -> R<()> {
        self.exec(
            "UPDATE user_tasksets SET public = ?2, updated_at = ?3 WHERE id = ?1",
            args![id, public, at],
        )
        .await
        .map(drop)
    }

    /// Newest first: every upload (`all`), or the viewer's own plus the
    /// public ready ones.
    pub async fn list_tasksets(
        &self,
        viewer: Option<u64>,
        all: bool,
        limit: usize,
    ) -> R<Vec<UserTaskset>> {
        let limit = limit as u64;
        let rows = match (all, viewer) {
            (true, _) => {
                self.rows(
                    "SELECT * FROM user_tasksets ORDER BY created_at DESC, id LIMIT ?1",
                    args![limit],
                )
                .await?
            }
            (false, Some(gid)) => {
                self.rows(
                    "SELECT * FROM user_tasksets \
                     WHERE owner_id = ?1 OR (public = 1 AND status = 'ready') \
                     ORDER BY created_at DESC, id LIMIT ?2",
                    args![gid, limit],
                )
                .await?
            }
            (false, None) => {
                self.rows(
                    "SELECT * FROM user_tasksets WHERE public = 1 AND status = 'ready' \
                     ORDER BY created_at DESC, id LIMIT ?1",
                    args![limit],
                )
                .await?
            }
        };
        rows.iter().map(taskset_of).collect()
    }

    // ---- user plugins ---------------------------------------------------

    pub async fn plugin(&self, id: &str) -> R<Option<UserPluginRecord>> {
        self.first("SELECT * FROM user_plugins WHERE id = ?1", args![id])
            .await?
            .map(|r| plugin_of(&r))
            .transpose()
    }

    /// False if the id is taken.
    pub async fn add_plugin(&self, u: &UserPluginRecord) -> R<bool> {
        Ok(self
            .run(Stmt {
                sql: "INSERT INTO user_plugins (id, owner_id, owner_login, upload_hash, status, \
                      error, public, title, plugin, info, created_at, updated_at) \
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                      ON CONFLICT (id) DO NOTHING",
                args: args![
                    &u.id,
                    u.owner_id,
                    &u.owner_login,
                    &u.upload_hash,
                    &u.status,
                    u.error.as_ref(),
                    u.public,
                    u.title.as_ref(),
                    u.plugin.as_ref().map(json_text),
                    u.info.as_ref().map(json_text),
                    &u.created_at,
                    &u.updated_at,
                ],
            })
            .await?
            == 1)
    }

    /// Records the build outcome; false unless it was still `building`.
    pub async fn finish_plugin(&self, u: &UserPluginRecord) -> R<bool> {
        Ok(self
            .exec(
                "UPDATE user_plugins SET status = ?2, error = ?3, title = ?4, plugin = ?5, \
                 info = ?6, updated_at = ?7, review = ?8 WHERE id = ?1 AND status = 'building'",
                args![
                    &u.id,
                    &u.status,
                    u.error.as_ref(),
                    u.title.as_ref(),
                    u.plugin.as_ref().map(json_text),
                    u.info.as_ref().map(json_text),
                    &u.updated_at,
                    u.review.as_ref().map(json_text),
                ],
            )
            .await?
            == 1)
    }

    /// `approval`: recorded when made public; kept when made private again
    /// (`None` leaves the column as it is).
    pub async fn set_plugin_public(
        &self,
        id: &str,
        public: bool,
        at: &str,
        approval: Option<&Value>,
    ) -> R<()> {
        self.exec(
            "UPDATE user_plugins SET public = ?2, updated_at = ?3, \
             approval = COALESCE(?4, approval) WHERE id = ?1",
            args![id, public, at, approval.map(json_text)],
        )
        .await
        .map(drop)
    }

    /// Newest first: every upload (`all`), or the viewer's own plus the
    /// public ready ones.
    pub async fn list_plugins(
        &self,
        viewer: Option<u64>,
        all: bool,
        limit: usize,
    ) -> R<Vec<UserPluginRecord>> {
        let limit = limit as u64;
        let rows = match (all, viewer) {
            (true, _) => {
                self.rows(
                    "SELECT * FROM user_plugins ORDER BY created_at DESC, id LIMIT ?1",
                    args![limit],
                )
                .await?
            }
            (false, Some(gid)) => {
                self.rows(
                    "SELECT * FROM user_plugins \
                     WHERE owner_id = ?1 OR (public = 1 AND status = 'ready') \
                     ORDER BY created_at DESC, id LIMIT ?2",
                    args![gid, limit],
                )
                .await?
            }
            (false, None) => {
                self.rows(
                    "SELECT * FROM user_plugins WHERE public = 1 AND status = 'ready' \
                     ORDER BY created_at DESC, id LIMIT ?1",
                    args![limit],
                )
                .await?
            }
        };
        rows.iter().map(plugin_of).collect()
    }

    // ---- evals ----------------------------------------------------------

    /// False if the eval id is taken.
    pub async fn add_eval(&self, e: &EvalRecord) -> R<bool> {
        Ok(self.run(insert_eval(e)).await? == 1)
    }

    pub async fn eval(&self, id: &str) -> R<Option<EvalRecord>> {
        self.first("SELECT * FROM evals WHERE eval_id = ?1", args![id])
            .await?
            .map(|r| eval_of(&r))
            .transpose()
    }

    /// Sets the status (and the run, if not known yet) of an eval that has
    /// not settled; false if it had.
    pub async fn set_status(
        &self,
        id: &str,
        status: &str,
        at: &str,
        run: Option<(u64, &str)>,
    ) -> R<bool> {
        Ok(self
            .exec(
                "UPDATE evals SET status = ?2, updated_at = ?3, \
                 run_id = COALESCE(run_id, ?4), run_url = COALESCE(run_url, ?5) \
                 WHERE eval_id = ?1 AND status NOT IN ('done', 'failed')",
                args![id, status, at, run.map(|r| r.0), run.map(|r| r.1)],
            )
            .await?
            == 1)
    }

    /// What a GitHub refresh learned (see `App::refresh`); never touches a
    /// settled eval.
    pub async fn save_refresh(&self, e: &EvalRecord) -> R<()> {
        self.exec(
            "UPDATE evals SET status = ?2, run_id = ?3, run_url = ?4, run_completed_s = ?5, \
             updated_at = ?6 WHERE eval_id = ?1 AND status NOT IN ('done', 'failed')",
            args![
                &e.eval_id,
                &e.status,
                e.run_id,
                e.run_url.as_ref(),
                e.run_completed_s,
                &e.updated_at,
            ],
        )
        .await
        .map(drop)
    }

    pub async fn results(&self, id: &str) -> R<Option<StoredResults>> {
        self.first("SELECT * FROM results WHERE eval_id = ?1", args![id])
            .await?
            .map(|r| results_of(&r))
            .transpose()
    }

    /// Stores posted results (see [`UPSERT_RESULTS`] for what may replace
    /// what).
    pub async fn put_results(&self, id: &str, r: &StoredResults) -> R<()> {
        self.run(results_stmt(UPSERT_RESULTS, id, r))
            .await
            .map(drop)
    }

    /// `GET /evals`: one owner's evals, or everyone's, newest first.
    pub async fn list_evals(&self, owner: Option<u64>, limit: usize) -> R<Vec<EvalSummary>> {
        const COLS: &str = "SELECT e.eval_id, e.mode, e.taskset, e.model, e.created_at, \
             e.status, r.status AS r_status, r.total_score, r.display \
             FROM evals e LEFT JOIN results r ON r.eval_id = e.eval_id";
        let limit = limit as u64;
        let rows = match owner {
            Some(gid) => {
                self.rows(
                    &format!(
                        "{COLS} WHERE e.owner_id = ?1 ORDER BY e.created_s DESC, e.eval_id LIMIT ?2"
                    ),
                    args![gid, limit],
                )
                .await?
            }
            None => {
                self.rows(
                    &format!("{COLS} ORDER BY e.created_s DESC, e.eval_id LIMIT ?1"),
                    args![limit],
                )
                .await?
            }
        };
        rows.iter()
            .map(|r| {
                let status = need(text(r, "status"), "evals", "status")?;
                Ok(EvalSummary {
                    eval_id: need(text(r, "eval_id"), "evals", "eval_id")?,
                    mode: need(
                        text(r, "mode").as_deref().and_then(Mode::parse),
                        "evals",
                        "mode",
                    )?,
                    taskset: text(r, "taskset").unwrap_or_default(),
                    model: text(r, "model"),
                    created_at: text(r, "created_at").unwrap_or_default(),
                    status: shown_status(&status, text(r, "r_status").as_deref()),
                    total_score: real(r, "total_score"),
                    display: json_col(r, "display"),
                })
            })
            .collect()
    }

    // ---- leaderboard ----------------------------------------------------

    /// `GET /leaderboard/:taskset`: the newest public, done, scored evals
    /// (through the partial index `evals_public_taskset`), plus the newest
    /// one's display snapshot (total, stage).
    pub async fn leaderboard_candidates(
        &self,
        taskset: &str,
        limit: usize,
    ) -> R<(Vec<Candidate>, Option<ScoreFormat>, Option<ScoreFormat>)> {
        let rows = self
            .rows(
                &format!(
                    "SELECT e.eval_id, e.owner_id, e.owner_login, e.model, e.created_at, \
                     e.created_s, e.stage_names, r.total_score, r.display, \
                     json_extract(r.manifest, '$.agent.name') AS agent, \
                     json_extract(r.manifest, '$.agent.version') AS agent_version, \
                     json_extract(r.manifest, '$.scoring.display.stage') AS stage_display \
                     {PUBLIC_DONE} AND e.taskset = ?1 \
                     ORDER BY e.created_s DESC, e.eval_id LIMIT ?2"
                ),
                args![taskset, limit as u64],
            )
            .await?;
        let display = rows.first().and_then(|r| json_col(r, "display"));
        let stage_display = rows.first().and_then(|r| json_col(r, "stage_display"));
        let cands = rows
            .iter()
            .filter_map(|r| {
                Some(Candidate {
                    eval_id: text(r, "eval_id")?,
                    owner_id: uint(r, "owner_id")?,
                    login: text(r, "owner_login")?,
                    agent: text(r, "agent").unwrap_or_default(),
                    agent_version: text(r, "agent_version").unwrap_or_default(),
                    model: text(r, "model"),
                    total_score: real(r, "total_score")?,
                    created_at: text(r, "created_at").unwrap_or_default(),
                    created_s: uint(r, "created_s").unwrap_or(0),
                    stage_names: json_col(r, "stage_names").unwrap_or_default(),
                })
            })
            .collect();
        Ok((cands, display, stage_display))
    }

    /// The stored manifests of these evals (at most 100: D1's bound
    /// parameter limit).
    pub async fn manifests(&self, ids: &[&str]) -> R<Vec<(String, Value)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let marks: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let rows = self
            .rows(
                &format!(
                    "SELECT eval_id, manifest FROM results WHERE eval_id IN ({})",
                    marks.join(", ")
                ),
                ids.iter().map(|&i| SqlArg::from(i)).collect(),
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| Some((text(r, "eval_id")?, json_col(r, "manifest")?)))
            .collect())
    }

    /// `GET /leaderboard`: tasksets with public, done, scored evals.
    pub async fn leaderboards(&self) -> R<Vec<BoardInfo>> {
        let rows = self
            .rows(
                &format!(
                    "SELECT e.taskset, COUNT(*) AS n, MAX(e.created_at) AS latest {PUBLIC_DONE} \
                     GROUP BY e.taskset ORDER BY MAX(e.created_s) DESC LIMIT 200"
                ),
                vec![],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(BoardInfo {
                    taskset: text(r, "taskset")?,
                    evals: uint(r, "n")?,
                    latest_at: text(r, "latest").unwrap_or_default(),
                })
            })
            .collect())
    }

    // ---- migration ------------------------------------------------------

    pub async fn batch(&self, stmts: &[Stmt]) -> R<()> {
        if stmts.is_empty() {
            return Ok(());
        }
        self.0.db_batch(stmts).await
    }
}
