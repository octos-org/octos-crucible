//! Typed access to the D1 tables (`migrations/0001_init.sql`).
//!
//! Idempotence and concurrency come from the SQL itself: claims are
//! `INSERT ... ON CONFLICT DO NOTHING` followed by a read, status updates
//! carry `WHERE status NOT IN ('done','failed')`, and one-shot transitions
//! (taskset packing) carry the state they leave in their `WHERE`. Errors are
//! the backend's strings; the handlers turn them into API errors.

use serde_json::Value;

use crate::http::{Backend, Row, SqlArg, Stmt};
use crate::model::{
    Budget, EvalRecord, EvalSummary, Mode, StoredResults, UploadKind, UploadRecord, UserTaskset,
    score_of, shown_status,
};
use crate::tokens::{TokenRecord, TokenSummary};

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

fn token_of(r: &Row) -> R<TokenRecord> {
    Ok(TokenRecord {
        owner_id: need(uint(r, "owner_id"), "tokens", "owner_id")?,
        login: need(text(r, "login"), "tokens", "login")?,
        name: text(r, "name").unwrap_or_default(),
        hash: need(text(r, "hash"), "tokens", "hash")?,
        created_at: text(r, "created_at").unwrap_or_default(),
    })
}

// ---- statements (shared by the handlers and the KV migration) ----------

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

    // ---- migration ------------------------------------------------------

    pub async fn batch(&self, stmts: &[Stmt]) -> R<()> {
        if stmts.is_empty() {
            return Ok(());
        }
        self.0.db_batch(stmts).await
    }
}
