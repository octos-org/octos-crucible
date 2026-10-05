//! Routing and handlers. See `docs/api.md` for the contract.

use serde::Deserialize;
use serde_json::json;

use crate::authz::{self, Principal};
use crate::config::Config;
use crate::dispatch;
use crate::github::{GitHub, RunView, TasksetInfo, UploadOutcome, view_run};
use crate::http::{ApiError, Backend, Req, Resp};
use crate::keys;
use crate::leaderboard;
use crate::model::{
    BUILDING, CRED_TTL_S, DONE, EvalRecord, EvalRequest, FAILED, MAX_UPLOAD, Mode, QUEUED,
    StoredResults, UploadRecord, ValidEval, is_login, is_status, is_terminal, is_uuid_v4,
    parse_results, status_rank,
};
use crate::model::{
    PL_BUILDING, PL_FAILED, PL_READY, PackResult, PluginResult, TS_FAILED, TS_PACKING, TS_READY,
    UploadKind, UserPluginRecord, UserTaskset, parse_pack_result, parse_plugin_result,
};
use crate::session;
use crate::shard::{release_tag, sha256_hex};
use crate::store::{Claim, Db};
use crate::tokens::{self, TokenRecord, TokenSummary};
use crate::util::{ct_eq, parse_query, query_get, rfc3339};
use crucible_core::taskset::is_user_taskset_id;

const OAUTH_COOKIE: &str = "crucible_oauth";
const TASKSETS_CACHE: &str = "tasksets";
const TASKSETS_TTL_S: u64 = 300;
const LIST_LIMIT: usize = 1000;
const LEADERBOARD_TTL_S: u64 = 300;
const MAX_LISTED_USER_TASKSETS: usize = 50;
/// How long a run may be finished before missing results count as failure.
const RESULTS_GRACE_S: u64 = 600;

type Result<T> = std::result::Result<T, ApiError>;

/// Entry point shared by the Worker and the tests.
pub async fn handle<B: Backend>(b: &B, cfg: &Config, req: &Req) -> Resp {
    let internal = req.path.starts_with("/internal/");
    let mut resp = if req.method == "OPTIONS" && !internal {
        Resp::empty(204)
    } else {
        let app = App { b, cfg };
        match app.route(req).await {
            Ok(r) => r,
            Err(e) => e.into_resp(),
        }
    };
    if !internal {
        cors(cfg, req, &mut resp);
    }
    if resp.header("cache-control").is_none() {
        resp.headers
            .push(("cache-control".into(), "no-store".into()));
    }
    resp.headers
        .push(("x-content-type-options".into(), "nosniff".into()));
    // Path only: query strings can carry OAuth codes.
    b.log(&format!("{} {} -> {}", req.method, req.path, resp.status));
    resp
}

/// Response for a Worker whose configuration does not load.
pub fn misconfigured() -> Resp {
    ApiError::internal("server misconfigured").into_resp()
}

fn cors(cfg: &Config, req: &Req, resp: &mut Resp) {
    if req.header("origin") != Some(cfg.pages_origin.as_str()) {
        return;
    }
    let h = &mut resp.headers;
    h.push((
        "access-control-allow-origin".into(),
        cfg.pages_origin.clone(),
    ));
    h.push(("vary".into(), "Origin".into()));
    if req.method == "OPTIONS" {
        h.push((
            "access-control-allow-methods".into(),
            "GET, POST, DELETE, OPTIONS".into(),
        ));
        h.push((
            "access-control-allow-headers".into(),
            "Authorization, Content-Type, X-Upload-Kind".into(),
        ));
        h.push(("access-control-max-age".into(), "600".into()));
    }
}

/// How a user taskset appears in `GET /tasksets`.
fn user_taskset_info(u: &UserTaskset) -> TasksetInfo {
    let stages = u
        .parsed()
        .map(|ts| {
            ts.stages
                .into_iter()
                .map(|s| crate::github::StageInfo {
                    name: s.id,
                    time_limit_s: s.time_limit_s,
                    total: s.expected_total,
                })
                .collect()
        })
        .unwrap_or_default();
    let display = u.parsed().map(|ts| ts.display).filter(|d| !d.is_empty());
    TasksetInfo {
        display,
        model_required: crate::github::model_required(u.parsed().and_then(|ts| ts.model).as_ref()),
        name: u.id.clone(),
        version: "upload".into(),
        stages,
        title: u.title.clone(),
        owner_login: Some(u.owner_login.clone()),
        public: Some(u.public),
        status: Some(u.status.clone()),
        error: u.error.clone(),
    }
}

struct App<'a, B: Backend> {
    b: &'a B,
    cfg: &'a Config,
}

impl<'a, B: Backend> App<'a, B> {
    fn gh(&self) -> GitHub<'a, B> {
        GitHub {
            b: self.b,
            cfg: self.cfg,
        }
    }

    async fn route(&self, req: &Req) -> Result<Resp> {
        let segs: Vec<&str> = req.path.trim_end_matches('/').split('/').skip(1).collect();
        let m = req.method.as_str();
        match (m, segs.as_slice()) {
            ("GET", [] | [""]) => Ok(Resp::json(200, &json!({"service": "crucible-worker"}))),
            ("GET", ["auth", "login"]) => self.login(req),
            ("GET", ["auth", "callback"]) => Ok(self.callback(req).await),
            ("GET", ["auth", "dev-login"]) => self.dev_login(req).await,
            ("GET", ["me"]) => {
                let p = self.principal(req).await?;
                Ok(Resp::json(
                    200,
                    &json!({"github_id": p.github_id, "login": p.login, "is_admin": p.is_admin}),
                ))
            }
            ("GET", ["pubkey"]) => Ok(Resp::json(200, keys::current())),
            ("GET", ["leaderboard"]) => self.leaderboards().await,
            ("GET", ["leaderboard", ts]) => self.leaderboard(ts).await,
            ("GET", ["tasksets"]) => self.list_tasksets(req).await,
            ("POST", ["tasksets"]) => self.create_taskset(req).await,
            ("GET", ["tasksets", id]) => self.get_taskset(req, id).await,
            ("POST", ["tasksets", id, "public"]) => self.set_taskset_public(req, id).await,
            ("GET", ["internal", "tasksets", id]) => self.internal_get_taskset(req, id).await,
            ("POST", ["internal", "tasksets", id]) => self.internal_taskset_result(req, id).await,
            ("GET", ["plugins"]) => self.list_plugins(req).await,
            ("POST", ["plugins"]) => self.create_plugin(req).await,
            ("GET", ["plugins", id]) => self.get_plugin(req, id).await,
            ("POST", ["plugins", id, "public"]) => self.set_plugin_public(req, id).await,
            ("GET", ["internal", "plugins", id]) => self.internal_get_plugin(req, id).await,
            ("POST", ["internal", "plugins", id]) => self.internal_plugin_result(req, id).await,
            ("POST", ["uploads"]) => self.upload(req).await,
            ("POST", ["evals"]) => self.create_eval(req).await,
            ("GET", ["evals"]) => self.list_evals(req).await,
            ("GET", ["evals", id]) => self.get_eval(req, id).await,
            ("GET", ["evals", id, "download"]) => self.download(req, id).await,
            ("GET", ["internal", "cred", id]) => self.internal_get_cred(req, id).await,
            ("DELETE", ["internal", "cred", id]) => self.internal_delete_cred(req, id).await,
            ("POST", ["internal", "results", id]) => self.internal_results(req, id).await,
            ("POST", ["internal", "status", id]) => self.internal_status(req, id).await,
            ("POST", ["tokens"]) => self.create_token(req).await,
            ("GET", ["tokens"]) => self.list_tokens(req).await,
            ("DELETE", ["tokens", id]) => self.delete_token(req, id).await,
            ("POST", ["admin", "ban"]) => self.ban(req, true).await,
            ("POST", ["admin", "unban"]) => self.ban(req, false).await,
            (
                _,
                ["auth", ..]
                | ["me"]
                | ["pubkey"]
                | ["tasksets"]
                | ["plugins", ..]
                | ["uploads"]
                | ["evals", ..]
                | ["tokens", ..]
                | ["internal", ..]
                | ["admin", ..],
            ) => Err(ApiError::new(
                405,
                "method_not_allowed",
                "method not allowed",
            )),
            _ => Err(ApiError::not_found("no such endpoint")),
        }
    }

    // ---- storage helpers ------------------------------------------------

    fn db(&self) -> Db<'a, B> {
        Db(self.b)
    }

    pub(crate) fn storage_err(&self, e: String) -> ApiError {
        self.b.log(&format!("storage error: {e}"));
        let l = e.to_ascii_lowercase();
        if (l.contains("exceed") && l.contains("limit")) || l.contains("quota") {
            // A daily free-plan quota (D1 rows written); resets at 00:00
            // UTC. Not a bug in the request.
            return ApiError::new(
                503,
                "storage_quota",
                "storage write quota exhausted for today (resets 00:00 UTC)",
            );
        }
        ApiError::internal("storage error")
    }

    async fn is_banned(&self, github_id: u64) -> Result<bool> {
        self.db()
            .is_banned(github_id)
            .await
            .map_err(|e| self.storage_err(e))
    }

    async fn load_record(&self, id: &str) -> Result<EvalRecord> {
        if !is_uuid_v4(id) {
            return Err(ApiError::bad_request(
                "eval id must be a lower-case UUID v4",
            ));
        }
        self.db()
            .eval(id)
            .await
            .map_err(|e| self.storage_err(e))?
            .ok_or_else(|| ApiError::not_found("no such eval"))
    }

    async fn load_results(&self, id: &str) -> Result<Option<StoredResults>> {
        self.db().results(id).await.map_err(|e| self.storage_err(e))
    }

    /// Deletes the sealed credential once the eval is over (usually the
    /// workflow has deleted it already; deleting nothing writes nothing).
    async fn drop_cred(&self, id: &str) {
        let _ = self.db().delete_cred(id).await;
    }

    // ---- auth -----------------------------------------------------------

    /// The caller, by web session or personal API token. A token never
    /// carries admin rights, so `/admin/*` and `?all=1` stay session-only.
    async fn principal(&self, req: &Req) -> Result<Principal> {
        let token = req
            .bearer()
            .ok_or_else(|| ApiError::unauthorized("missing Authorization: Bearer token"))?;
        let Some(id) = tokens::parse(token) else {
            return self.session_principal(req).await;
        };
        let bad = || ApiError::unauthorized("invalid or revoked API token");
        let rec: TokenRecord = self
            .db()
            .token(id)
            .await
            .map_err(|e| self.storage_err(e))?
            .ok_or_else(bad)?;
        if !tokens::matches(&rec, token) {
            return Err(bad());
        }
        let banned = self.is_banned(rec.owner_id).await?;
        authz::admit(rec.owner_id, &rec.login, banned, false)
    }

    /// The caller by web session only (token management).
    async fn session_principal(&self, req: &Req) -> Result<Principal> {
        let token = req
            .bearer()
            .ok_or_else(|| ApiError::unauthorized("missing Authorization: Bearer token"))?;
        if tokens::parse(token).is_some() {
            return Err(ApiError::forbidden(
                "API tokens are managed from the website, not with a token",
            ));
        }
        let s = session::verify_session(&self.cfg.session_key, token, self.b.now_s())
            .map_err(|_| ApiError::unauthorized("invalid or expired session"))?;
        let banned = self.is_banned(s.gid).await?;
        authz::admit(s.gid, &s.login, banned, self.cfg.is_admin(s.gid))
    }

    fn internal_auth(&self, req: &Req) -> Result<()> {
        let ok = req
            .bearer()
            .is_some_and(|t| ct_eq(t.as_bytes(), self.cfg.worker_token.as_bytes()));
        if ok {
            Ok(())
        } else {
            Err(ApiError::unauthorized("internal endpoint"))
        }
    }

    fn redirect_uri(&self, req: &Req) -> String {
        format!("{}/auth/callback", req.origin)
    }

    fn login(&self, req: &Req) -> Result<Resp> {
        let nonce = hex::encode(self.b.random_bytes(16));
        let state = session::issue_state(&self.cfg.session_key, &nonce, self.b.now_s());
        let url = self.gh().authorize_url(&state, &self.redirect_uri(req));
        // The cookie only binds the OAuth round trip to this browser
        // (login CSRF); API calls authenticate with the Bearer token.
        Ok(Resp::redirect(&url).with_header(
            "set-cookie",
            &format!(
                "{OAUTH_COOKIE}={nonce}; Max-Age={}; Path=/auth/callback; HttpOnly; Secure; SameSite=Lax",
                session::STATE_TTL_S
            ),
        ))
    }

    fn to_pages(&self, fragment: &str) -> Resp {
        Resp::redirect(&format!("{}#{fragment}", self.cfg.pages_url))
            .with_header(
                "set-cookie",
                &format!("{OAUTH_COOKIE}=; Max-Age=0; Path=/auth/callback; HttpOnly; Secure; SameSite=Lax"),
            )
            .with_header("referrer-policy", "no-referrer")
    }

    /// Always answers with a redirect to the page: `#token=` or `#error=`.
    async fn callback(&self, req: &Req) -> Resp {
        match self.callback_inner(req).await {
            Ok(token) => self.to_pages(&format!("token={token}")),
            Err(e) => {
                self.b
                    .log(&format!("oauth callback: {} ({})", e.code, e.message));
                self.to_pages(&format!("error={}", e.code))
            }
        }
    }

    async fn callback_inner(&self, req: &Req) -> Result<String> {
        let q = parse_query(&req.query);
        if query_get(&q, "error").is_some() {
            return Err(ApiError::new(
                401,
                "oauth_denied",
                "authorization was denied",
            ));
        }
        let state_err = || ApiError::new(400, "oauth_state", "login expired, please retry");
        let code = query_get(&q, "code")
            .filter(|c| {
                !c.is_empty()
                    && c.len() <= 128
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or_else(|| ApiError::bad_request("missing code"))?;
        let state = query_get(&q, "state").ok_or_else(state_err)?;
        let nonce = session::verify_state(&self.cfg.session_key, state, self.b.now_s())
            .map_err(|_| state_err())?;
        let cookie = req.cookie(OAUTH_COOKIE).ok_or_else(state_err)?;
        if !ct_eq(cookie.as_bytes(), nonce.as_bytes()) {
            return Err(state_err());
        }
        let gh = self.gh();
        let user_token = gh.oauth_token(code, &self.redirect_uri(req)).await?;
        let (id, login) = gh.user(&user_token).await?;
        if self.is_banned(id).await? {
            return Err(ApiError::banned());
        }
        Ok(session::issue_session(
            &self.cfg.session_key,
            id,
            &login,
            self.b.now_s(),
        ))
    }

    /// Dev only: a session without GitHub, for local end-to-end tests.
    async fn dev_login(&self, req: &Req) -> Result<Resp> {
        let local = matches!(req.host.as_str(), "localhost" | "127.0.0.1" | "[::1]");
        if !(self.cfg.dev_auth && local) {
            return Err(ApiError::not_found("no such endpoint"));
        }
        let q = parse_query(&req.query);
        let id = query_get(&q, "github_id")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| ApiError::bad_request("github_id must be a positive integer"))?;
        let login = query_get(&q, "login")
            .filter(|l| is_login(l))
            .ok_or_else(|| ApiError::bad_request("login must be a GitHub login"))?;
        if self.is_banned(id).await? {
            return Err(ApiError::banned());
        }
        let token = session::issue_session(&self.cfg.session_key, id, login, self.b.now_s());
        Ok(Resp::json(200, &json!({"token": token})))
    }

    // ---- tasksets -------------------------------------------------------

    async fn tasksets(&self) -> Result<Vec<TasksetInfo>> {
        let now = self.b.now_s();
        if let Ok(Some(raw)) = self.db().cache_get(TASKSETS_CACHE, now).await
            && let Ok(list) = serde_json::from_str::<Vec<TasksetInfo>>(&raw)
        {
            return Ok(list);
        }
        let list = self.gh().tasksets().await?;
        let _ = self
            .db()
            .cache_put(
                TASKSETS_CACHE,
                &serde_json::to_string(&list).expect("json"),
                Some(now + TASKSETS_TTL_S),
            )
            .await;
        Ok(list)
    }

    // ---- leaderboard ----------------------------------------------------

    /// A JSON value from the D1 cache, or computed and cached for
    /// [`LEADERBOARD_TTL_S`].
    async fn cached<F: std::future::Future<Output = Result<serde_json::Value>>>(
        &self,
        key: &str,
        compute: F,
    ) -> Result<Resp> {
        let now = self.b.now_s();
        if let Ok(Some(raw)) = self.db().cache_get(key, now).await
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            return Ok(Resp::json(200, &v));
        }
        let v = compute.await?;
        let _ = self
            .db()
            .cache_put(key, &v.to_string(), Some(now + LEADERBOARD_TTL_S))
            .await;
        Ok(Resp::json(200, &v))
    }

    /// `GET /leaderboard`: tasksets that have public results.
    async fn leaderboards(&self) -> Result<Resp> {
        self.cached("leaderboard", async {
            let list = self
                .db()
                .leaderboards()
                .await
                .map_err(|e| self.storage_err(e))?;
            Ok(json!(list))
        })
        .await
    }

    /// `GET /leaderboard/:taskset`: the best public eval of each (owner,
    /// agent), see `crate::leaderboard`.
    async fn leaderboard(&self, taskset: &str) -> Result<Resp> {
        if !crate::model::is_slug(taskset) {
            return Err(ApiError::not_found("no such taskset"));
        }
        self.cached(&format!("leaderboard/{taskset}"), async {
            let db = self.db();
            let (cands, display, stage_display) = db
                .leaderboard_candidates(taskset, leaderboard::MAX_CANDIDATES)
                .await
                .map_err(|e| self.storage_err(e))?;
            let direction = display.as_ref().map(|d| d.direction).unwrap_or_default();
            let all = self.taskset_stage_names(taskset).await?;
            let (complete, groups) = leaderboard::split(cands, all.as_deref());
            let entries = self.board_entries(complete, direction).await?;
            let mut partial = Vec::with_capacity(groups.len());
            for (stages, cands) in groups {
                partial.push(leaderboard::PartialGroup {
                    stages,
                    entries: self.board_entries(cands, direction).await?,
                });
            }
            Ok(json!(leaderboard::Board {
                taskset: taskset.to_owned(),
                direction,
                display,
                stage_display,
                entries,
                partial,
            }))
        })
        .await
    }

    /// Ranked entries of these candidates, with their manifests' details.
    async fn board_entries(
        &self,
        cands: Vec<leaderboard::Candidate>,
        direction: crucible_core::taskset::Direction,
    ) -> Result<Vec<leaderboard::Entry>> {
        let ranked = leaderboard::rank(cands, direction);
        let ids: Vec<&str> = ranked.iter().map(|(_, c)| c.eval_id.as_str()).collect();
        let manifests = self
            .db()
            .manifests(&ids)
            .await
            .map_err(|e| self.storage_err(e))?;
        Ok(ranked
            .into_iter()
            .map(|(r, c)| {
                let m = manifests
                    .iter()
                    .find(|(id, _)| *id == c.eval_id)
                    .and_then(|(_, m)| serde_json::from_value(m.clone()).ok());
                leaderboard::entry(r, c, m.as_ref())
            })
            .collect())
    }

    /// All stage names of a taskset (built-in or uploaded); `None` when it
    /// is unknown.
    async fn taskset_stage_names(&self, taskset: &str) -> Result<Option<Vec<String>>> {
        let info = if is_user_taskset_id(taskset) {
            self.load_user_taskset(taskset)
                .await?
                .map(|u| user_taskset_info(&u))
        } else {
            self.tasksets()
                .await?
                .into_iter()
                .find(|t| t.name == taskset)
        };
        Ok(info
            .map(|t| t.stages.into_iter().map(|s| s.name).collect::<Vec<_>>())
            .filter(|v| !v.is_empty()))
    }

    // ---- user tasksets --------------------------------------------------

    async fn load_user_taskset(&self, id: &str) -> Result<Option<UserTaskset>> {
        if !is_user_taskset_id(id) {
            return Ok(None);
        }
        self.db().taskset(id).await.map_err(|e| self.storage_err(e))
    }

    /// `GET /tasksets`: built-in tasksets for everyone; with a valid token
    /// also the caller's own uploads and public ones (admins: `?all=1`
    /// lists every upload). An invalid token is treated as anonymous.
    async fn list_tasksets(&self, req: &Req) -> Result<Resp> {
        let mut list = self.tasksets().await?;
        let p = match req.bearer() {
            Some(_) => self.principal(req).await.ok(),
            None => None,
        };
        let all = query_get(&parse_query(&req.query), "all") == Some("1")
            && p.as_ref().is_some_and(|p| p.is_admin);
        let users = self
            .db()
            .list_tasksets(
                p.as_ref().map(|p| p.github_id),
                all,
                MAX_LISTED_USER_TASKSETS,
            )
            .await
            .map_err(|e| self.storage_err(e))?;
        list.extend(users.iter().map(user_taskset_info));
        Ok(Resp::json(200, &list))
    }

    /// `POST /tasksets {"upload_hash"}`: register an uploaded taskset zip.
    async fn create_taskset(&self, req: &Req) -> Result<Resp> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            upload_hash: String,
        }
        let p = self.principal(req).await?;
        if req.body.len() > 4096 {
            return Err(ApiError::too_large(4096));
        }
        let body: Body = serde_json::from_slice(&req.body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        if !crate::shard::is_hash(&body.upload_hash) {
            return Err(ApiError::bad_request(
                "upload_hash must be 64 lower-case hex characters",
            ));
        }
        let upload = self.own_upload(&p, &body.upload_hash).await?;
        if upload.kind != UploadKind::Taskset {
            return Err(ApiError::bad_request(format!(
                "upload_hash was uploaded as {}, not taskset",
                upload.kind.as_str()
            )));
        }
        let id = format!("u-{}", hex::encode(self.b.random_bytes(8)));
        let now = rfc3339(self.b.now_s());
        let mut u = UserTaskset {
            id: id.clone(),
            owner_id: p.github_id,
            owner_login: p.login.clone(),
            upload_hash: body.upload_hash.clone(),
            status: TS_PACKING.into(),
            error: None,
            public: false,
            title: None,
            taskset: None,
            created_at: now.clone(),
            updated_at: now,
        };
        if !self
            .db()
            .add_taskset(&u)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            return Err(ApiError::conflict("taskset id collision; retry"));
        }
        let worker_url = self
            .cfg
            .worker_url
            .clone()
            .unwrap_or_else(|| req.origin.clone());
        let inputs = json!({
            "taskset_id": id,
            "source": format!("blob:{}", body.upload_hash),
            "results_url": format!("{worker_url}/internal/tasksets/{id}"),
        });
        if let Err(e) = self
            .gh()
            .dispatch(&self.cfg.taskset_workflow, &inputs)
            .await
        {
            u.status = TS_FAILED.into();
            u.error = Some("could not start packing; please retry".into());
            u.updated_at = rfc3339(self.b.now_s());
            let _ = self.db().finish_packing(&u).await;
            return Err(e);
        }
        Ok(Resp::json(201, &json!({"id": id, "status": TS_PACKING})))
    }

    async fn get_taskset(&self, req: &Req, id: &str) -> Result<Resp> {
        let p = self.principal(req).await?;
        let u = self
            .load_user_taskset(id)
            .await?
            .filter(|u| u.visible_to(p.github_id, p.is_admin))
            .ok_or_else(|| ApiError::not_found("no such taskset"))?;
        let mut out = serde_json::to_value(user_taskset_info(&u)).expect("json");
        out["created_at"] = json!(u.created_at);
        out["updated_at"] = json!(u.updated_at);
        Ok(Resp::json(200, &out))
    }

    /// `POST /tasksets/:id/public {"public": bool}` (admins only).
    async fn set_taskset_public(&self, req: &Req, id: &str) -> Result<Resp> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            public: bool,
        }
        let p = self.principal(req).await?;
        authz::require_admin(&p)?;
        let body: Body = serde_json::from_slice(&req.body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        let mut u = self
            .load_user_taskset(id)
            .await?
            .ok_or_else(|| ApiError::not_found("no such taskset"))?;
        if body.public && u.status != TS_READY {
            return Err(ApiError::conflict(
                "only a ready taskset can be made public",
            ));
        }
        u.public = body.public;
        u.updated_at = rfc3339(self.b.now_s());
        self.db()
            .set_taskset_public(&u.id, u.public, &u.updated_at)
            .await
            .map_err(|e| self.storage_err(e))?;
        self.b.log(&format!(
            "admin {} set {id} public={}",
            p.github_id, u.public
        ));
        Ok(Resp::json(200, &json!({"id": u.id, "public": u.public})))
    }

    fn internal_taskset_id<'i>(&self, req: &Req, id: &'i str) -> Result<&'i str> {
        self.internal_auth(req)?;
        if !is_user_taskset_id(id) {
            return Err(ApiError::bad_request("taskset id must be u-<16 hex>"));
        }
        Ok(id)
    }

    /// `GET /internal/tasksets/:id?github_id=N`: the taskset.json, only if
    /// that user may use it (the workflow's own permission check).
    async fn internal_get_taskset(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_taskset_id(req, id)?;
        let gid = query_get(&parse_query(&req.query), "github_id")
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| ApiError::bad_request("github_id is required"))?;
        let u = self
            .load_user_taskset(id)
            .await?
            .ok_or_else(|| ApiError::not_found("no such taskset"))?;
        if u.status != TS_READY {
            return Err(ApiError::conflict(format!("taskset is {}", u.status)));
        }
        if !u.usable_by(gid) {
            return Err(ApiError::forbidden("this user may not use this taskset"));
        }
        Ok(Resp::json(200, u.taskset.as_ref().expect("ready")))
    }

    /// `POST /internal/tasksets/:id`: the taskset-pack outcome.
    async fn internal_taskset_result(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_taskset_id(req, id)?;
        let mut u = self
            .load_user_taskset(id)
            .await?
            .ok_or_else(|| ApiError::not_found("no such taskset"))?;
        if u.status != TS_PACKING {
            return Err(ApiError::conflict(format!(
                "taskset is already {}",
                u.status
            )));
        }
        match parse_pack_result(&req.body, id)? {
            PackResult::Ready(ts) => {
                // Uploaded plugins: as registered, and usable by its owner.
                for up in &ts.user_plugins {
                    let rec = self.load_plugin(&up.name).await?;
                    let ok = rec
                        .as_ref()
                        .is_some_and(|r| r.usable_by(u.owner_id) && r.plugin.as_ref() == Some(up));
                    if !ok {
                        return Err(ApiError::bad_request(format!(
                            "plugin {} is not a ready plugin the owner may use",
                            up.name
                        )));
                    }
                }
                u.title = ts.title.clone();
                u.taskset = Some(serde_json::to_value(&ts).expect("json"));
                u.status = TS_READY.into();
            }
            PackResult::Failed(e) => {
                u.error = Some(e);
                u.status = TS_FAILED.into();
            }
        }
        u.updated_at = rfc3339(self.b.now_s());
        if !self
            .db()
            .finish_packing(&u)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            // Another delivery got there first.
            return Err(ApiError::conflict("taskset is already packed"));
        }
        Ok(Resp::json(200, &json!({"ok": true, "status": u.status})))
    }

    // ---- user plugins ---------------------------------------------------

    async fn load_plugin(&self, id: &str) -> Result<Option<UserPluginRecord>> {
        if !crucible_core::plugins::is_user_plugin_id(id) {
            return Ok(None);
        }
        self.db().plugin(id).await.map_err(|e| self.storage_err(e))
    }

    /// `GET /plugins`: the caller's own uploads and the public ones
    /// (anonymous: public only; admins with `?all=1`: every upload).
    async fn list_plugins(&self, req: &Req) -> Result<Resp> {
        let p = match req.bearer() {
            Some(_) => self.principal(req).await.ok(),
            None => None,
        };
        let all = query_get(&parse_query(&req.query), "all") == Some("1")
            && p.as_ref().is_some_and(|p| p.is_admin);
        let list = self
            .db()
            .list_plugins(
                p.as_ref().map(|p| p.github_id),
                all,
                MAX_LISTED_USER_TASKSETS,
            )
            .await
            .map_err(|e| self.storage_err(e))?;
        let out: Vec<serde_json::Value> = list.iter().map(UserPluginRecord::view).collect();
        Ok(Resp::json(200, &out))
    }

    /// `POST /plugins {"upload_hash"}`: register an uploaded plugin package.
    async fn create_plugin(&self, req: &Req) -> Result<Resp> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            upload_hash: String,
        }
        let p = self.principal(req).await?;
        if req.body.len() > 4096 {
            return Err(ApiError::too_large(4096));
        }
        let body: Body = serde_json::from_slice(&req.body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        if !crate::shard::is_hash(&body.upload_hash) {
            return Err(ApiError::bad_request(
                "upload_hash must be 64 lower-case hex characters",
            ));
        }
        let upload = self.own_upload(&p, &body.upload_hash).await?;
        if upload.kind != UploadKind::Plugin {
            return Err(ApiError::bad_request(format!(
                "upload_hash was uploaded as {}, not plugin",
                upload.kind.as_str()
            )));
        }
        let id = format!("u-{}", hex::encode(self.b.random_bytes(8)));
        let now = rfc3339(self.b.now_s());
        let mut u = UserPluginRecord {
            id: id.clone(),
            owner_id: p.github_id,
            owner_login: p.login.clone(),
            upload_hash: body.upload_hash.clone(),
            status: PL_BUILDING.into(),
            error: None,
            public: false,
            title: None,
            plugin: None,
            info: None,
            created_at: now.clone(),
            updated_at: now,
        };
        if !self
            .db()
            .add_plugin(&u)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            return Err(ApiError::conflict("plugin id collision; retry"));
        }
        let worker_url = self
            .cfg
            .worker_url
            .clone()
            .unwrap_or_else(|| req.origin.clone());
        let inputs = json!({
            "plugin_id": id,
            "source": format!("blob:{}", body.upload_hash),
            "results_url": format!("{worker_url}/internal/plugins/{id}"),
        });
        if let Err(e) = self.gh().dispatch(&self.cfg.plugin_workflow, &inputs).await {
            u.status = PL_FAILED.into();
            u.error = Some("could not start building; please retry".into());
            u.updated_at = rfc3339(self.b.now_s());
            let _ = self.db().finish_plugin(&u).await;
            return Err(e);
        }
        Ok(Resp::json(201, &json!({"id": id, "status": PL_BUILDING})))
    }

    async fn get_plugin(&self, req: &Req, id: &str) -> Result<Resp> {
        let p = self.principal(req).await?;
        let u = self
            .load_plugin(id)
            .await?
            .filter(|u| u.visible_to(p.github_id, p.is_admin))
            .ok_or_else(|| ApiError::not_found("no such plugin"))?;
        Ok(Resp::json(200, &u.view()))
    }

    /// `POST /plugins/:id/public {"public": bool}` (admins only).
    async fn set_plugin_public(&self, req: &Req, id: &str) -> Result<Resp> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            public: bool,
        }
        let p = self.principal(req).await?;
        authz::require_admin(&p)?;
        let body: Body = serde_json::from_slice(&req.body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        let u = self
            .load_plugin(id)
            .await?
            .ok_or_else(|| ApiError::not_found("no such plugin"))?;
        if body.public && u.status != PL_READY {
            return Err(ApiError::conflict("only a ready plugin can be made public"));
        }
        self.db()
            .set_plugin_public(&u.id, body.public, &rfc3339(self.b.now_s()))
            .await
            .map_err(|e| self.storage_err(e))?;
        self.b.log(&format!(
            "admin {} set plugin {id} public={}",
            p.github_id, body.public
        ));
        Ok(Resp::json(200, &json!({"id": u.id, "public": body.public})))
    }

    fn internal_plugin_id<'i>(&self, req: &Req, id: &'i str) -> Result<&'i str> {
        self.internal_auth(req)?;
        if !crucible_core::plugins::is_user_plugin_id(id) {
            return Err(ApiError::bad_request("plugin id must be u-<16 hex>"));
        }
        Ok(id)
    }

    /// `GET /internal/plugins/:id?taskset=T`: the pinned plugin, only if
    /// the owner of user taskset `T` may use it (taskset-pack's check).
    async fn internal_get_plugin(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_plugin_id(req, id)?;
        let tid = query_get(&parse_query(&req.query), "taskset")
            .filter(|t| is_user_taskset_id(t))
            .ok_or_else(|| ApiError::bad_request("taskset=u-<16 hex> is required"))?
            .to_owned();
        let ts = self
            .load_user_taskset(&tid)
            .await?
            .ok_or_else(|| ApiError::not_found("no such taskset"))?;
        let u = self
            .load_plugin(id)
            .await?
            .ok_or_else(|| ApiError::not_found("no such plugin"))?;
        if u.status != PL_READY {
            return Err(ApiError::conflict(format!("plugin is {}", u.status)));
        }
        if !u.usable_by(ts.owner_id) {
            return Err(ApiError::forbidden(
                "this plugin is private to another user",
            ));
        }
        Ok(Resp::json(200, u.plugin.as_ref().expect("ready")))
    }

    /// `POST /internal/plugins/:id`: the plugin-pack outcome.
    async fn internal_plugin_result(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_plugin_id(req, id)?;
        let mut u = self
            .load_plugin(id)
            .await?
            .ok_or_else(|| ApiError::not_found("no such plugin"))?;
        if u.status != PL_BUILDING {
            return Err(ApiError::conflict(format!(
                "plugin is already {}",
                u.status
            )));
        }
        match parse_plugin_result(&req.body, id)? {
            PluginResult::Ready {
                plugin,
                title,
                info,
            } => {
                if plugin.blob.sha256 != u.upload_hash {
                    return Err(ApiError::bad_request("plugin.blob must be the upload"));
                }
                u.plugin = Some(*plugin);
                u.title = Some(title).filter(|t| !t.is_empty());
                u.info = Some(info);
                u.status = PL_READY.into();
            }
            PluginResult::Failed(e) => {
                u.error = Some(e);
                u.status = PL_FAILED.into();
            }
        }
        u.updated_at = rfc3339(self.b.now_s());
        if !self
            .db()
            .finish_plugin(&u)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            return Err(ApiError::conflict("plugin is already built"));
        }
        Ok(Resp::json(200, &json!({"ok": true, "status": u.status})))
    }

    // ---- uploads --------------------------------------------------------

    async fn upload(&self, req: &Req) -> Result<Resp> {
        let p = self.principal(req).await?;
        let kind = req
            .header("x-upload-kind")
            .and_then(UploadKind::parse)
            .ok_or_else(|| {
                ApiError::bad_request("X-Upload-Kind must be agent, app, taskset or plugin")
            })?;
        if req.body.len() > MAX_UPLOAD {
            return Err(ApiError::too_large(MAX_UPLOAD));
        }
        let key = keys::current();
        match keys::check_sealed(&req.body, &key.key_id) {
            Ok(()) => {}
            Err(keys::SealError::WrongKey) => {
                return Err(ApiError::new(
                    400,
                    "wrong_key",
                    "upload is not sealed to the current public key (GET /pubkey)",
                ));
            }
            Err(_) => {
                return Err(ApiError::new(
                    400,
                    "not_sealed",
                    "upload must be a crucible envelope (encrypt in the browser first)",
                ));
            }
        }
        let hash = sha256_hex(&req.body);
        let tag = release_tag(&hash).expect("sha256 hex");
        let gh = self.gh();
        let release = gh.release(&tag).await?;
        // The D1 row is the claim and is written before the bytes reach
        // GitHub, so a request cut short after its GitHub upload can be
        // retried: the owner's retry finds its claim and accepts the asset
        // already there once its bytes check out. Bytes in the store without
        // a claim are someone else's (e.g. workflow outputs) and stay
        // unclaimable. The primary key makes concurrent claims safe.
        let rec = UploadRecord {
            owner_id: p.github_id,
            kind,
            size: req.body.len() as u64,
            created_at: rfc3339(self.b.now_s()),
        };
        let claim = self
            .db()
            .claim_upload(&hash, &rec)
            .await
            .map_err(|e| self.storage_err(e))?;
        let stored = match claim {
            Claim::Other => return Err(ApiError::conflict("this blob belongs to another user")),
            Claim::Own | Claim::New => gh.upload_asset(&release, &hash, req.body.clone()).await?,
        };
        match (claim, stored) {
            (Claim::New, UploadOutcome::Created) => {
                return Ok(Resp::json(201, &json!({"hash": hash})));
            }
            (Claim::New, UploadOutcome::AlreadyExists) => {
                // Stored earlier but not through this user's upload: withdraw
                // the claim and refuse, so nobody can claim someone else's
                // sealed blob.
                let _ = self.db().withdraw_upload(&hash, p.github_id).await;
                return Err(ApiError::conflict(
                    "a blob with these bytes already exists; encrypt again and re-upload",
                ));
            }
            (_, UploadOutcome::AlreadyExists) if !gh.asset_matches(&tag, &hash).await? => {
                return Err(ApiError::upstream("stored asset does not match its name"));
            }
            _ => {}
        }
        Ok(Resp::json(200, &json!({"hash": hash})))
    }

    /// The caller's own upload record of `hash`.
    async fn own_upload(&self, p: &Principal, hash: &str) -> Result<UploadRecord> {
        let upload = self
            .db()
            .upload(hash)
            .await
            .map_err(|e| self.storage_err(e))?
            .ok_or_else(|| ApiError::bad_request("upload_hash is unknown; upload first"))?;
        if upload.owner_id != p.github_id {
            return Err(ApiError::forbidden("upload_hash belongs to another user"));
        }
        Ok(upload)
    }

    // ---- evals ----------------------------------------------------------

    async fn create_eval(&self, req: &Req) -> Result<Resp> {
        let p = self.principal(req).await?;
        let v: ValidEval = EvalRequest::parse(&req.body)?.validate(&keys::current().key_id)?;

        let upload = self.own_upload(&p, &v.upload_hash).await?;
        if upload.kind != UploadKind::from(v.mode) {
            return Err(ApiError::bad_request(format!(
                "upload_hash was uploaded as {}, not {}",
                upload.kind.as_str(),
                v.mode.as_str()
            )));
        }
        let ts = if is_user_taskset_id(&v.taskset) {
            // A user taskset: its owner's, or public (the workflow checks
            // again with the Worker before using it).
            let u = self
                .load_user_taskset(&v.taskset)
                .await?
                .filter(|u| u.usable_by(p.github_id))
                .ok_or_else(|| ApiError::bad_request("unknown taskset"))?;
            user_taskset_info(&u)
        } else {
            self.tasksets()
                .await?
                .into_iter()
                .find(|t| t.name == v.taskset)
                .ok_or_else(|| ApiError::bad_request("unknown taskset"))?
        };
        let names: Vec<String> = ts.stages.iter().map(|s| s.name.clone()).collect();
        let n = names.len() as u32;
        let (stages, stage_names) = match v.mode {
            // First N stages.
            Mode::Agent => {
                let k = v.stages.unwrap_or(n);
                if k > n {
                    return Err(ApiError::bad_request(format!(
                        "taskset {} has {n} stages",
                        ts.name
                    )));
                }
                (k, names[..k as usize].to_vec())
            }
            // One stage, 1-based.
            Mode::App => {
                if ts.model_required && v.cred.is_none() {
                    return Err(ApiError::bad_request(format!(
                        "taskset {} scores with a model: cred_envelope is required",
                        ts.name
                    )));
                }
                let i = v.stages.unwrap_or(1);
                if i > n {
                    return Err(ApiError::bad_request(format!(
                        "taskset {} has stages 1..={n}",
                        ts.name
                    )));
                }
                (i, vec![names[i as usize - 1].clone()])
            }
        };

        let now = self.b.now_s();
        let mut rec = EvalRecord {
            eval_id: v.eval_id.clone(),
            owner_id: p.github_id,
            owner_login: p.login.clone(),
            mode: v.mode,
            upload_hash: v.upload_hash.clone(),
            taskset: v.taskset.clone(),
            stages,
            stage_names,
            model: v.model.clone(),
            replicas: v.replicas,
            budget: v.budget.clone(),
            score_public: v.score_public,
            created_at: rfc3339(now),
            created_s: now,
            status: QUEUED.into(),
            run_id: None,
            run_url: None,
            run_completed_s: None,
            manifest: None,
            download_sha256: None,
            updated_at: rfc3339(now),
        };
        // The primary key claims the eval id.
        if !self
            .db()
            .add_eval(&rec)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            return Err(ApiError::conflict("eval_id already used"));
        }
        let fail = |rec: &mut EvalRecord| {
            rec.status = FAILED.into();
            rec.updated_at = rfc3339(self.b.now_s());
        };
        if let Some(cred) = &v.cred {
            // Stored exactly as sealed by the page; returned unchanged by
            // GET /internal/cred/:id until it expires.
            let put = match std::str::from_utf8(cred) {
                Ok(text) => self.db().put_cred(&v.eval_id, text, now + CRED_TTL_S).await,
                Err(_) => Err("sealed credential is not UTF-8".to_owned()),
            };
            if let Err(e) = put {
                fail(&mut rec);
                let _ = self.db().save_refresh(&rec).await;
                return Err(self.storage_err(e));
            }
        }

        let worker_url = self
            .cfg
            .worker_url
            .clone()
            .unwrap_or_else(|| req.origin.clone());
        let (workflow, inputs) = dispatch::inputs(self.cfg, &rec, v.cred.is_some(), &worker_url);
        if let Err(e) = self.gh().dispatch(&workflow, &inputs).await {
            if v.cred.is_some() {
                self.drop_cred(&rec.eval_id).await;
            }
            fail(&mut rec);
            let _ = self.db().save_refresh(&rec).await;
            return Err(e);
        }
        Ok(Resp::json(201, &json!({"eval_id": rec.eval_id})))
    }

    async fn list_evals(&self, req: &Req) -> Result<Resp> {
        let p = self.principal(req).await?;
        let q = parse_query(&req.query);
        let owner = if query_get(&q, "all") == Some("1") {
            authz::require_admin(&p)?;
            None
        } else {
            Some(p.github_id)
        };
        let list = self
            .db()
            .list_evals(owner, LIST_LIMIT)
            .await
            .map_err(|e| self.storage_err(e))?;
        Ok(Resp::json(200, &list))
    }

    async fn get_eval(&self, req: &Req, id: &str) -> Result<Resp> {
        let p = self.principal(req).await?;
        let mut rec = self.load_record(id).await?;
        if !authz::can_view_eval(&p, rec.owner_id) {
            return Err(ApiError::forbidden("not your eval"));
        }
        let results = self.load_results(id).await?;
        if !is_terminal(&rec.clone().with_results(results.as_ref()).status) {
            self.refresh(&mut rec).await;
        }
        let rec = rec.with_results(results.as_ref());
        let mut out = json!({
            "eval_id": rec.eval_id,
            "status": rec.status,
            "mode": rec.mode,
            "taskset": rec.taskset,
            "stages": rec.stages,
            "stage_names": rec.stage_names,
            "model": rec.model,
            "replicas": rec.replicas,
            "score_public": rec.score_public,
            "owner_login": rec.owner_login,
            "created_at": rec.created_at,
            "updated_at": rec.updated_at,
            "download_available": rec.download_sha256.is_some(),
        });
        let o = out.as_object_mut().expect("object");
        if let Some(u) = &rec.run_url {
            o.insert("run_url".into(), json!(u));
        }
        if let Some(m) = &rec.manifest {
            o.insert("manifest".into(), m.clone());
        }
        if let Some(s) = rec.total_score() {
            o.insert("total_score".into(), json!(s));
        }
        // Ran every stage of the taskset (the taskset's total name applies
        // only then). Absent when the taskset is unknown.
        if let Ok(Some(all)) = self.taskset_stage_names(&rec.taskset).await {
            o.insert(
                "complete".into(),
                json!(all.iter().all(|s| rec.stage_names.contains(s))),
            );
        }
        Ok(Resp::json(200, &out))
    }

    fn workflow_of(&self, mode: Mode) -> &str {
        match mode {
            Mode::Agent => &self.cfg.eval_workflow,
            Mode::App => &self.cfg.score_workflow,
        }
    }

    /// Poll GitHub for an eval that has not finished and estimate its
    /// status. Status only moves forward, so a precise status reported by
    /// the workflow is never replaced by the coarser estimate. The estimate
    /// is shown, not stored: the record is written only when the run turns
    /// out to be over (or first seen finished without results, which
    /// starts the grace period). GitHub errors leave it as it was.
    async fn refresh(&self, rec: &mut EvalRecord) {
        if is_terminal(&rec.status) {
            return;
        }
        let gh = self.gh();
        let found = match rec.run_id {
            None => gh
                .find_run(self.workflow_of(rec.mode), &rec.eval_id, rec.created_s)
                .await
                .transpose(),
            Some(id) => Some(gh.run(id).await),
        };
        let run = match found {
            None => return,
            Some(Ok(run)) => run,
            Some(Err(e)) => {
                self.b
                    .log(&format!("refresh {}: {}", rec.eval_id, e.message));
                return;
            }
        };
        let completed_before = rec.run_completed_s;
        let job = if run.status == "in_progress" {
            gh.current_job(run.id).await.ok().flatten()
        } else {
            None
        };
        let now = self.b.now_s();
        let estimate = match view_run(
            &run,
            job.as_deref(),
            rec.stage_names.first().map(String::as_str),
        ) {
            RunView::Status(s) => Some(s),
            RunView::CompletedOk => {
                // Results are posted from inside the run; allow for a late
                // delivery before calling it failed.
                let since = *rec.run_completed_s.get_or_insert(now);
                (now.saturating_sub(since) >= RESULTS_GRACE_S).then(|| FAILED.to_owned())
            }
        };
        if let Some(s) = estimate
            && status_rank(&s) > status_rank(&rec.status)
        {
            rec.status = s;
        }
        rec.run_id = Some(run.id);
        rec.run_url = Some(run.html_url);
        if is_terminal(&rec.status) || rec.run_completed_s != completed_before {
            rec.updated_at = rfc3339(now);
            if let Err(e) = self.db().save_refresh(rec).await {
                self.b.log(&format!("refresh save failed: {e}"));
            }
        }
        if is_terminal(&rec.status) {
            // The run is over; the key has no further use.
            self.drop_cred(&rec.eval_id).await;
        }
    }

    async fn download(&self, req: &Req, id: &str) -> Result<Resp> {
        let p = self.principal(req).await?;
        let rec = self.load_record(id).await?;
        if !authz::can_view_eval(&p, rec.owner_id) {
            return Err(ApiError::forbidden("not your eval"));
        }
        let rec = rec.with_results(self.load_results(id).await?.as_ref());
        let hash = rec
            .download_sha256
            .ok_or_else(|| ApiError::new(404, "not_ready", "no download for this eval yet"))?;
        let tag = release_tag(&hash).ok_or_else(|| ApiError::internal("bad stored hash"))?;
        let url = self.gh().asset_download_url(&tag, &hash);
        let wants_json = req
            .header("accept")
            .is_some_and(|a| a.contains("application/json"));
        if wants_json {
            Ok(Resp::json(200, &json!({"url": url})))
        } else {
            Ok(Resp::redirect(&url))
        }
    }

    // ---- internal (GitHub Actions) --------------------------------------

    fn internal_id<'i>(&self, req: &Req, id: &'i str) -> Result<&'i str> {
        self.internal_auth(req)?;
        if !is_uuid_v4(id) {
            return Err(ApiError::bad_request(
                "eval id must be a lower-case UUID v4",
            ));
        }
        Ok(id)
    }

    async fn internal_get_cred(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_id(req, id)?;
        let cred = self
            .db()
            .cred(id, self.b.now_s())
            .await
            .map_err(|e| self.storage_err(e))?
            .ok_or_else(|| ApiError::not_found("no credential for this eval"))?;
        Ok(Resp::bytes(cred.into_bytes()))
    }

    async fn internal_delete_cred(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_id(req, id)?;
        self.db()
            .delete_cred(id)
            .await
            .map_err(|e| self.storage_err(e))?;
        Ok(Resp::empty(204))
    }

    async fn internal_results(&self, req: &Req, id: &str) -> Result<Resp> {
        let id = self.internal_id(req, id)?;
        let rec = self.load_record(id).await?;
        let results = parse_results(&req.body, id)?;
        let prev = self.load_results(id).await?;
        let new = StoredResults {
            manifest: results.manifest,
            download_sha256: results
                .download
                .or_else(|| prev.as_ref().and_then(|p| p.download_sha256.clone())),
            status: results.status,
            updated_at: rfc3339(self.b.now_s()),
        };
        // A repeated delivery (the poster retries when an answer is lost)
        // writes nothing; nor does a late partial delivery after final
        // results (the upsert's WHERE enforces the same under races).
        let stored = match prev {
            Some(p)
                if (p.manifest == new.manifest
                    && p.download_sha256 == new.download_sha256
                    && p.status == new.status)
                    || (is_terminal(&p.status) && !is_terminal(&new.status)) =>
            {
                p
            }
            _ => {
                self.db()
                    .put_results(id, &new)
                    .await
                    .map_err(|e| self.storage_err(e))?;
                new
            }
        };
        if is_terminal(&stored.status) {
            self.drop_cred(id).await;
        }
        let status = rec.with_results(Some(&stored)).status;
        Ok(Resp::json(200, &json!({"ok": true, "status": status})))
    }

    /// Progress from the workflow: `{"status": "building" | "running:<stage>"
    /// | "scoring" | "failed"}`. `done` comes with the results. One row
    /// update per change; `building` is not stored at all (the GitHub
    /// estimate shown by `GET /evals/:id` says the same).
    async fn internal_status(&self, req: &Req, id: &str) -> Result<Resp> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            status: String,
        }
        let id = self.internal_id(req, id)?;
        let rec = self.load_record(id).await?;
        if req.body.len() > 4096 {
            return Err(ApiError::too_large(4096));
        }
        let body: Body = serde_json::from_slice(&req.body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        if !is_status(&body.status) || body.status == QUEUED || body.status == DONE {
            return Err(ApiError::bad_request(
                "status must be building, running:<stage>, scoring or failed",
            ));
        }
        let results = self.load_results(id).await?;
        let shown = rec.clone().with_results(results.as_ref()).status;
        if is_terminal(&shown) {
            return Err(ApiError::conflict(format!("eval is already {shown}")));
        }
        if rec.status == body.status || body.status == BUILDING {
            // Nothing to write.
            return Ok(Resp::json(200, &json!({"ok": true, "status": shown})));
        }
        // The first stored report also records the run, so queries need not
        // search for it.
        let run = match rec.run_id {
            Some(_) => None,
            None => self
                .gh()
                .find_run(self.workflow_of(rec.mode), id, rec.created_s)
                .await
                .ok()
                .flatten(),
        };
        let updated = self
            .db()
            .set_status(
                id,
                &body.status,
                &rfc3339(self.b.now_s()),
                run.as_ref().map(|r| (r.id, r.html_url.as_str())),
            )
            .await
            .map_err(|e| self.storage_err(e))?;
        if !updated {
            return Err(ApiError::conflict("eval is already settled"));
        }
        if is_terminal(&body.status) {
            self.drop_cred(id).await;
        }
        Ok(Resp::json(200, &json!({"ok": true, "status": body.status})))
    }

    // ---- personal API tokens --------------------------------------------

    async fn create_token(&self, req: &Req) -> Result<Resp> {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct Body {
            #[serde(default)]
            name: Option<String>,
        }
        let p = self.session_principal(req).await?;
        if req.body.len() > 4096 {
            return Err(ApiError::too_large(4096));
        }
        let body: Body = if req.body.iter().all(u8::is_ascii_whitespace) {
            Body::default()
        } else {
            serde_json::from_slice(&req.body)
                .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?
        };
        let name = tokens::clean_name(body.name.as_deref()).ok_or_else(|| {
            ApiError::bad_request(format!(
                "name: at most {} characters, no control characters",
                tokens::MAX_NAME
            ))
        })?;
        let existing = self
            .db()
            .token_count(p.github_id)
            .await
            .map_err(|e| self.storage_err(e))?;
        if existing >= tokens::MAX_PER_USER as u64 {
            return Err(ApiError::conflict(format!(
                "at most {} tokens; revoke one first",
                tokens::MAX_PER_USER
            )));
        }
        let (id, token) = tokens::mint(&self.b.random_bytes(40));
        let created_at = rfc3339(self.b.now_s());
        let rec = TokenRecord {
            owner_id: p.github_id,
            login: p.login.clone(),
            name: name.clone(),
            hash: tokens::hash(&token),
            created_at: created_at.clone(),
        };
        let summary = TokenSummary {
            id: id.clone(),
            name,
            created_at,
        };
        if !self
            .db()
            .add_token(&id, &rec)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            return Err(ApiError::conflict("token id collision; retry"));
        }
        let mut out = serde_json::to_value(&summary).expect("json");
        out["token"] = json!(token);
        Ok(Resp::json(201, &out))
    }

    async fn list_tokens(&self, req: &Req) -> Result<Resp> {
        let p = self.session_principal(req).await?;
        let list = self
            .db()
            .tokens_of(p.github_id)
            .await
            .map_err(|e| self.storage_err(e))?;
        Ok(Resp::json(200, &list))
    }

    async fn delete_token(&self, req: &Req, id: &str) -> Result<Resp> {
        let p = self.session_principal(req).await?;
        if !tokens::is_id(id) {
            return Err(ApiError::bad_request("token id must be 16 hex characters"));
        }
        if !self
            .db()
            .delete_token(id, p.github_id)
            .await
            .map_err(|e| self.storage_err(e))?
        {
            return Err(ApiError::not_found("no such token"));
        }
        Ok(Resp::empty(204))
    }

    // ---- admin ----------------------------------------------------------

    async fn ban(&self, req: &Req, ban: bool) -> Result<Resp> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Body {
            github_id: u64,
            #[serde(default)]
            reason: Option<String>,
        }
        let p = self.principal(req).await?;
        authz::require_admin(&p)?;
        if req.body.len() > 4096 {
            return Err(ApiError::too_large(4096));
        }
        let body: Body = serde_json::from_slice(&req.body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        if ban {
            authz::check_ban_target(&p, body.github_id, self.cfg.is_admin(body.github_id))?;
            let reason: Option<String> = body.reason.map(|r| r.chars().take(200).collect());
            self.db()
                .ban(
                    body.github_id,
                    p.github_id,
                    &rfc3339(self.b.now_s()),
                    reason.as_deref(),
                )
                .await
                .map_err(|e| self.storage_err(e))?;
        } else {
            self.db()
                .unban(body.github_id)
                .await
                .map_err(|e| self.storage_err(e))?;
        }
        self.b.log(&format!(
            "admin {} {} {}",
            p.github_id,
            if ban { "banned" } else { "unbanned" },
            body.github_id
        ));
        Ok(Resp::json(
            200,
            &json!({"ok": true, "github_id": body.github_id, "banned": ban}),
        ))
    }
}

/// Cron Trigger (wrangler.toml `[triggers]`): deletes expired credentials
/// and cache entries.
pub async fn scheduled<B: Backend>(b: &B) {
    match Db(b).purge_expired(b.now_s()).await {
        Ok(n) => b.log(&format!("cron: purged {n} expired rows")),
        Err(e) => b.log(&format!("cron: purge failed: {e}")),
    }
}
