//! Request bodies, their validation, and the records kept in KV.

use crucible_core::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::ApiError;
use crate::keys::{SealError, check_sealed};
use crate::shard::is_hash;
use crate::util::b64_decode;

/// Sealed bytes, as the page checks them before uploading.
pub const MAX_UPLOAD: usize = 25 * 1024 * 1024;
pub const MAX_EVAL_BODY: usize = 64 * 1024;
pub const MAX_CRED: usize = 16 * 1024;
pub const MAX_RESULTS_BODY: usize = 2 * 1024 * 1024;
/// Same limit as the page (the workflow allows more).
pub const MAX_REPLICAS: u32 = 10;
pub const CRED_TTL_S: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Upload an agent package; full evaluation with the user's model key.
    Agent,
    /// Upload a finished artifact; score one stage only.
    App,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Agent => "agent",
            Mode::App => "app",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "agent" => Some(Mode::Agent),
            "app" => Some(Mode::App),
            _ => None,
        }
    }
}

/// `X-Upload-Kind`: what an upload is for. An eval may only use an upload
/// of its own mode; `POST /tasksets` only a `taskset` upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadKind {
    Agent,
    App,
    /// A taskset source zip (see `POST /tasksets`).
    Taskset,
}

impl UploadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadKind::Agent => "agent",
            UploadKind::App => "app",
            UploadKind::Taskset => "taskset",
        }
    }
    pub fn parse(s: &str) -> Option<UploadKind> {
        match s {
            "agent" => Some(UploadKind::Agent),
            "app" => Some(UploadKind::App),
            "taskset" => Some(UploadKind::Taskset),
            _ => None,
        }
    }
}

impl From<Mode> for UploadKind {
    fn from(m: Mode) -> UploadKind {
        match m {
            Mode::Agent => UploadKind::Agent,
            Mode::App => UploadKind::App,
        }
    }
}

// ---- status ------------------------------------------------------------
//
// Fixed values shared with the page:
// `queued | building | running:<stage> | scoring | done | failed`.

pub const QUEUED: &str = "queued";
pub const BUILDING: &str = "building";
pub const SCORING: &str = "scoring";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";

pub fn is_status(s: &str) -> bool {
    match s.strip_prefix("running:") {
        Some(stage) => is_slug(stage),
        None => matches!(s, QUEUED | BUILDING | SCORING | DONE | FAILED),
    }
}

pub fn is_terminal(s: &str) -> bool {
    matches!(s, DONE | FAILED)
}

/// Progress order; status never moves backwards.
pub fn status_rank(s: &str) -> u8 {
    match s {
        QUEUED => 0,
        BUILDING => 1,
        SCORING => 3,
        DONE | FAILED => 4,
        _ => 2, // running:<stage>
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

/// Body of `POST /evals`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalRequest {
    pub mode: Mode,
    pub eval_id: String,
    pub upload_hash: String,
    pub taskset: String,
    #[serde(default)]
    pub stages: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub replicas: Option<u32>,
    #[serde(default)]
    pub budget: Option<Budget>,
    #[serde(default)]
    pub cred_envelope: Option<String>,
    pub score_public: bool,
    pub consent: bool,
}

/// An `EvalRequest` that passed the context-free checks.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidEval {
    pub mode: Mode,
    pub eval_id: String,
    pub upload_hash: String,
    pub taskset: String,
    /// agent: run the first N stages (None = all); app: 1-based stage number.
    pub stages: Option<u32>,
    pub model: Option<String>,
    pub replicas: u32,
    pub budget: Option<Budget>,
    /// Sealed bytes, stored as received (decoded from base64).
    pub cred: Option<Vec<u8>>,
    pub score_public: bool,
}

/// UUID v4, lower case: `xxxxxxxx-xxxx-4xxx-[89ab]xxx-xxxxxxxxxxxx`.
pub fn is_uuid_v4(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
    b.iter().enumerate().all(|(i, &c)| match i {
        8 | 13 | 18 | 23 => c == b'-',
        14 => c == b'4',
        19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
        _ => hex(c),
    })
}

/// `[a-z0-9][a-z0-9-]{0,63}` — crucible-core's rule for taskset/stage names.
pub fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `[A-Za-z0-9][A-Za-z0-9._:/-]{0,79}`: what eval.yml's `crucible plan`
/// accepts (`glm-5.3`, `anthropic/claude-x`, `org/model:tag`).
pub fn is_model(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 80
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._:/-".contains(c))
}

/// GitHub login: `[A-Za-z0-9-]{1,39}`.
pub fn is_login(s: &str) -> bool {
    !s.is_empty() && s.len() <= 39 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

const MODEL_RULE: &str = "model must match [A-Za-z0-9][A-Za-z0-9._:/-]{0,79}";

fn decode_cred(cred: &str, current_key_id: &str) -> Result<Vec<u8>, ApiError> {
    let bad = |m: &str| ApiError::bad_request(m.to_owned());
    if cred.len() > MAX_CRED * 2 {
        return Err(bad("cred_envelope is too large"));
    }
    let sealed = b64_decode(cred).ok_or_else(|| bad("cred_envelope must be standard base64"))?;
    if sealed.len() > MAX_CRED {
        return Err(bad("cred_envelope is too large"));
    }
    match check_sealed(&sealed, current_key_id) {
        Ok(()) => Ok(sealed),
        Err(SealError::WrongKey) => Err(ApiError::new(
            400,
            "wrong_key",
            "cred_envelope is not sealed to the current public key (GET /pubkey)",
        )),
        Err(_) => Err(bad("cred_envelope is not a crucible envelope")),
    }
}

impl EvalRequest {
    pub fn parse(body: &[u8]) -> Result<EvalRequest, ApiError> {
        if body.len() > MAX_EVAL_BODY {
            return Err(ApiError::too_large(MAX_EVAL_BODY));
        }
        serde_json::from_slice(body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))
    }

    /// Checks that need no KV or GitHub lookups.
    pub fn validate(self, current_key_id: &str) -> Result<ValidEval, ApiError> {
        let bad = |m: &str| Err(ApiError::bad_request(m.to_owned()));
        if !self.consent {
            return Err(ApiError::new(
                400,
                "consent_required",
                "the submission terms must be accepted (consent: true)",
            ));
        }
        if !is_uuid_v4(&self.eval_id) {
            return bad("eval_id must be a lower-case UUID v4");
        }
        if !is_hash(&self.upload_hash) {
            return bad("upload_hash must be 64 lower-case hex characters");
        }
        if !is_slug(&self.taskset) {
            return bad("taskset must match [a-z0-9][a-z0-9-]{0,63}");
        }
        if self.stages == Some(0) {
            return bad("stages must be at least 1");
        }
        if let Some(m) = &self.model
            && !is_model(m)
        {
            return bad(MODEL_RULE);
        }
        let cred = match &self.cred_envelope {
            Some(c) => Some(decode_cred(c, current_key_id)?),
            None => None,
        };
        match self.mode {
            Mode::Agent => {
                if self.model.is_none() {
                    return bad("model is required in agent mode");
                }
                let replicas = self.replicas.unwrap_or(1);
                if !(1..=MAX_REPLICAS).contains(&replicas) {
                    return Err(ApiError::bad_request(format!(
                        "replicas must be between 1 and {MAX_REPLICAS}"
                    )));
                }
                if let Some(b) = &self.budget {
                    check_budget(b)?;
                }
                if cred.is_none() {
                    return bad("cred_envelope is required in agent mode");
                }
                Ok(ValidEval {
                    mode: self.mode,
                    eval_id: self.eval_id,
                    upload_hash: self.upload_hash,
                    taskset: self.taskset,
                    stages: self.stages,
                    model: self.model,
                    replicas,
                    budget: self.budget.filter(|b| *b != Budget::default()),
                    cred,
                    score_public: self.score_public,
                })
            }
            Mode::App => {
                if self.stages.is_none() {
                    return bad("stages (1-based stage number) is required in app mode");
                }
                if self.budget.is_some() {
                    return bad("budget is not accepted in app mode");
                }
                if self.replicas.is_some_and(|r| r != 1) {
                    return bad("replicas must be 1 in app mode");
                }
                Ok(ValidEval {
                    mode: self.mode,
                    eval_id: self.eval_id,
                    upload_hash: self.upload_hash,
                    taskset: self.taskset,
                    stages: self.stages,
                    model: self.model,
                    replicas: 1,
                    budget: None,
                    cred,
                    score_public: self.score_public,
                })
            }
        }
    }
}

fn check_budget(b: &Budget) -> Result<(), ApiError> {
    if b.max_requests.is_some_and(|v| v == 0 || v > 10_000_000) {
        return Err(ApiError::bad_request(
            "budget.max_requests must be between 1 and 10000000",
        ));
    }
    if b.max_tokens.is_some_and(|v| v == 0 || v > 10_000_000_000) {
        return Err(ApiError::bad_request(
            "budget.max_tokens must be between 1 and 10000000000",
        ));
    }
    if b.max_cost_usd
        .is_some_and(|v| !v.is_finite() || v <= 0.0 || v > 10_000.0)
    {
        return Err(ApiError::bad_request(
            "budget.max_cost_usd must be > 0 and <= 10000",
        ));
    }
    Ok(())
}

/// KV `upload/<hash>`: who uploaded a blob through the Worker. An eval may
/// only reference the caller's own uploads, so nobody can feed someone
/// else's sealed blob (which the platform would decrypt) into a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRecord {
    pub owner_id: u64,
    pub kind: UploadKind,
    pub size: u64,
    pub created_at: String,
}

// ---- user tasksets -----------------------------------------------------

pub const TS_PACKING: &str = "packing";
pub const TS_READY: &str = "ready";
pub const TS_FAILED: &str = "failed";
pub const MAX_TASKSET_BODY: usize = 256 * 1024;

/// KV `tasksets/<id>`: a user-uploaded taskset. Private to its owner until
/// an admin makes it public; built-in tasksets live in the repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTaskset {
    /// `u-<16 hex>`; also the `name` in its taskset.json and in evals.
    pub id: String,
    pub owner_id: u64,
    pub owner_login: String,
    pub upload_hash: String,
    /// `packing | ready | failed`.
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub public: bool,
    /// The name from the uploaded source.json.
    #[serde(default)]
    pub title: Option<String>,
    /// The packed taskset.json (blob references, no test content).
    #[serde(default)]
    pub taskset: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// KV key metadata of `tasksets/<id>`: enough to decide who sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTasksetMeta {
    pub owner_id: u64,
    pub status: String,
    pub public: bool,
    pub created_at: String,
}

impl UserTaskset {
    pub fn meta(&self) -> UserTasksetMeta {
        UserTasksetMeta {
            owner_id: self.owner_id,
            status: self.status.clone(),
            public: self.public,
            created_at: self.created_at.clone(),
        }
    }

    /// Who may see it in listings and details.
    pub fn visible_to(&self, github_id: u64, is_admin: bool) -> bool {
        is_admin || self.owner_id == github_id || (self.public && self.status == TS_READY)
    }

    /// Who may run evals on it: the owner, or anyone once it is public.
    pub fn usable_by(&self, github_id: u64) -> bool {
        self.status == TS_READY && (self.public || self.owner_id == github_id)
    }

    /// The parsed taskset.json of a ready taskset.
    pub fn parsed(&self) -> Option<crucible_core::TaskSet> {
        serde_json::from_value(self.taskset.clone()?).ok()
    }
}

/// Body of `POST /internal/tasksets/:id` (from the taskset-pack workflow).
pub enum PackResult {
    Ready(Box<crucible_core::TaskSet>),
    Failed(String),
}

pub fn parse_pack_result(body: &[u8], id: &str) -> Result<PackResult, ApiError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        status: String,
        #[serde(default)]
        taskset: Option<crucible_core::TaskSet>,
        #[serde(default)]
        error: Option<String>,
    }
    if body.len() > MAX_TASKSET_BODY {
        return Err(ApiError::too_large(MAX_TASKSET_BODY));
    }
    let b: Body = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
    match (b.status.as_str(), b.taskset, b.error) {
        (TS_READY, Some(ts), None) => {
            ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            if ts.name != id {
                return Err(ApiError::bad_request("taskset.name must be the taskset id"));
            }
            if !crucible_core::taskset::USER_SCORERS.contains(&ts.scorer.name.as_str()) {
                return Err(ApiError::bad_request(
                    "scorer not offered for uploaded tasksets",
                ));
            }
            Ok(PackResult::Ready(Box::new(ts)))
        }
        (TS_FAILED, None, Some(e)) => Ok(PackResult::Failed(e.chars().take(500).collect())),
        _ => Err(ApiError::bad_request(
            "body must be {status: ready, taskset} or {status: failed, error}",
        )),
    }
}

/// KV `evals/<eval_id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRecord {
    pub eval_id: String,
    pub owner_id: u64,
    pub owner_login: String,
    pub mode: Mode,
    pub upload_hash: String,
    pub taskset: String,
    /// agent: number of leading stages run; app: 1-based stage number.
    pub stages: u32,
    /// Names of the stages this eval covers, in order.
    #[serde(default)]
    pub stage_names: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub replicas: u32,
    #[serde(default)]
    pub budget: Option<Budget>,
    pub score_public: bool,
    pub created_at: String,
    pub created_s: u64,
    /// See [`is_status`].
    pub status: String,
    #[serde(default)]
    pub run_id: Option<u64>,
    #[serde(default)]
    pub run_url: Option<String>,
    /// When the run was first seen completed without results.
    #[serde(default)]
    pub run_completed_s: Option<u64>,
    #[serde(default)]
    pub manifest: Option<Value>,
    /// SHA-256 of the password-protected result zip.
    #[serde(default)]
    pub download_sha256: Option<String>,
    pub updated_at: String,
}

/// KV `results/<eval_id>`: what the workflow posted to
/// `POST /internal/results`. Only that endpoint writes this key; refreshing
/// from GitHub writes `evals/<eval_id>` and never touches it, so a refresh
/// that read a stale record cannot erase results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredResults {
    pub manifest: Value,
    #[serde(default)]
    pub download_sha256: Option<String>,
    pub status: String,
    pub updated_at: String,
}

/// Shown by `GET /evals` and kept as KV key metadata (< 1 KiB) so the list
/// needs a single KV call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSummary {
    pub eval_id: String,
    pub mode: Mode,
    pub taskset: String,
    pub model: Option<String>,
    pub created_at: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_score: Option<f64>,
    /// How to show `total_score`: the manifest's `scoring` snapshot; absent
    /// for manifests that predate it (a 0–1 ratio, shown as a percentage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<crucible_core::taskset::ScoreFormat>,
}

impl EvalRecord {
    fn parsed_manifest(&self) -> Option<Manifest> {
        serde_json::from_value::<Manifest>(self.manifest.clone()?).ok()
    }

    /// The eval as shown: posted results win over the record. A terminal
    /// results status is final; otherwise a failed record (the run died
    /// after partial results) stays failed, and anything else takes the
    /// further of the two.
    pub fn with_results(mut self, r: Option<&StoredResults>) -> EvalRecord {
        let Some(r) = r else { return self };
        self.manifest = Some(r.manifest.clone());
        if r.download_sha256.is_some() {
            self.download_sha256.clone_from(&r.download_sha256);
        }
        if is_terminal(&r.status)
            || (!is_terminal(&self.status) && status_rank(&r.status) > status_rank(&self.status))
        {
            self.status.clone_from(&r.status);
        }
        if r.updated_at > self.updated_at {
            self.updated_at.clone_from(&r.updated_at);
        }
        self
    }

    pub fn total_score(&self) -> Option<f64> {
        total_score(&self.parsed_manifest()?)
    }

    pub fn summary(&self) -> EvalSummary {
        EvalSummary {
            eval_id: self.eval_id.clone(),
            mode: self.mode,
            taskset: self.taskset.clone(),
            model: self.model.clone(),
            created_at: self.created_at.clone(),
            status: self.status.clone(),
            total_score: self.total_score(),
            display: self
                .parsed_manifest()
                .and_then(|m| m.scoring?.display.total),
        }
    }
}

/// The manifest's total by its own `scoring` snapshot (old manifests: the
/// 0–1 ratio Σpassed / Σtotal): the same function `crucible manifest` uses.
pub fn total_score(m: &Manifest) -> Option<f64> {
    m.compute_total_score()
}

/// Body of `POST /internal/results/:id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Results {
    /// The manifest as posted (minus `download` and `status`).
    pub manifest: Value,
    pub download: Option<String>,
    /// Default `done`; a partial manifest names the current status.
    pub status: String,
}

/// A Manifest, plus optional top-level `download: {sha256}` (the password
/// zip) and `status` (for partial results while the run continues).
pub fn parse_results(body: &[u8], eval_id: &str) -> Result<Results, ApiError> {
    if body.len() > MAX_RESULTS_BODY {
        return Err(ApiError::too_large(MAX_RESULTS_BODY));
    }
    let mut raw: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("results must be JSON: {e}")))?;
    let obj = raw
        .as_object_mut()
        .ok_or_else(|| ApiError::bad_request("results must be a JSON object"))?;
    let download = match obj.remove("download") {
        None | Some(Value::Null) => None,
        Some(d) => Some(
            d.get("sha256")
                .and_then(Value::as_str)
                .filter(|h| is_hash(h))
                .ok_or_else(|| ApiError::bad_request("download.sha256 must be a SHA-256 hex"))?
                .to_owned(),
        ),
    };
    let status = match obj.remove("status") {
        None | Some(Value::Null) => DONE.to_owned(),
        Some(Value::String(s)) if is_status(&s) && s != QUEUED => s,
        Some(_) => return Err(ApiError::bad_request("status is not a valid eval status")),
    };
    let manifest: Manifest = serde_json::from_value(raw.clone())
        .map_err(|e| ApiError::bad_request(format!("not a manifest: {e}")))?;
    if manifest.eval_id != eval_id {
        return Err(ApiError::bad_request(
            "manifest.eval_id does not match the URL",
        ));
    }
    Ok(Results {
        manifest: raw,
        download,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::current;
    use crate::util::b64_encode;
    use crucible_core::Envelope;

    const EID: &str = "3f2b8c1e-9a4d-4c7e-8b1a-2d3e4f5a6b7c";

    fn sealed_cred() -> String {
        let mut b = Envelope::new(current().key_id.clone()).header_line();
        b.extend_from_slice(b"age-encryption.org/v1\nciphertext");
        b64_encode(&b)
    }

    fn agent_body() -> Value {
        serde_json::json!({
            "mode": "agent",
            "eval_id": EID,
            "upload_hash": "ab".repeat(32),
            "taskset": "github-full",
            "stages": 2,
            "model": "glm-5.3",
            "replicas": 3,
            "budget": {"max_requests": 500, "max_cost_usd": 12.5},
            "cred_envelope": sealed_cred(),
            "score_public": false,
            "consent": true
        })
    }

    fn check(v: &Value) -> Result<ValidEval, ApiError> {
        EvalRequest::parse(&serde_json::to_vec(v).unwrap())?.validate(&current().key_id)
    }

    fn with(mut v: Value, key: &str, val: Value) -> Value {
        if val.is_null() {
            v.as_object_mut().unwrap().remove(key);
        } else {
            v[key] = val;
        }
        v
    }

    #[test]
    fn ids() {
        assert!(is_uuid_v4(EID));
        assert!(!is_uuid_v4(&EID.to_uppercase()));
        assert!(!is_uuid_v4("3f2b8c1e-9a4d-1c7e-8b1a-2d3e4f5a6b7c")); // v1
        assert!(!is_uuid_v4("3f2b8c1e-9a4d-4c7e-cb1a-2d3e4f5a6b7c")); // variant
        assert!(!is_uuid_v4("3f2b8c1e9a4d4c7e8b1a2d3e4f5a6b7c"));
        assert!(!is_uuid_v4("../../../../../../../../../../../../.."));
        assert!(is_model("glm-5.3") && is_model("anthropic/claude-x:v1"));
        assert!(!is_model("a b") && !is_model("-x") && !is_model("x;rm") && !is_model(""));
        assert!(!is_model(&"m".repeat(81)));
        assert!(is_login("octo-cat") && !is_login("a/b") && !is_login(&"a".repeat(40)));
        assert!(is_slug("github-full") && !is_slug("GitHub") && !is_slug("a_b"));
    }

    #[test]
    fn statuses() {
        for s in [
            "queued",
            "building",
            "running:stage-1",
            "scoring",
            "done",
            "failed",
        ] {
            assert!(is_status(s), "{s}");
        }
        for s in [
            "running",
            "running:",
            "running:Stage 1",
            "succeeded",
            "cancelled",
            "",
        ] {
            assert!(!is_status(s), "{s}");
        }
        assert!(is_terminal("done") && is_terminal("failed") && !is_terminal("scoring"));
        assert!(status_rank("building") < status_rank("running:stage-2"));
        assert!(status_rank("running:stage-2") < status_rank("scoring"));
    }

    #[test]
    fn agent_request_ok() {
        let v = check(&agent_body()).unwrap();
        assert_eq!(v.mode, Mode::Agent);
        assert_eq!(v.replicas, 3);
        assert_eq!(v.stages, Some(2));
        assert!(v.cred.unwrap().starts_with(b"{\"crucible_envelope\":1"));
        let v = check(&with(
            with(
                with(agent_body(), "replicas", Value::Null),
                "budget",
                Value::Null,
            ),
            "stages",
            Value::Null,
        ))
        .unwrap();
        assert_eq!((v.replicas, v.stages, v.budget), (1, None, None));
        assert_eq!(
            check(&with(agent_body(), "replicas", 10.into()))
                .unwrap()
                .replicas,
            10
        );
    }

    #[test]
    fn agent_request_rejects() {
        let cases: Vec<(&str, Value, &str)> = vec![
            ("consent", Value::Bool(false), "consent_required"),
            ("consent", Value::Null, "bad_request"),
            ("score_public", Value::Null, "bad_request"),
            ("eval_id", "ev1".into(), "bad_request"),
            ("upload_hash", "AB".repeat(32).into(), "bad_request"),
            ("taskset", "../x".into(), "bad_request"),
            ("stages", 0.into(), "bad_request"),
            ("stages", (-1).into(), "bad_request"),
            ("model", Value::Null, "bad_request"),
            ("model", "$(id)".into(), "bad_request"),
            ("replicas", 0.into(), "bad_request"),
            ("replicas", 11.into(), "bad_request"),
            (
                "budget",
                serde_json::json!({"max_cost_usd": -1}),
                "bad_request",
            ),
            (
                "budget",
                serde_json::json!({"max_requests": 0}),
                "bad_request",
            ),
            ("budget", serde_json::json!({"other": 1}), "bad_request"),
            ("cred_envelope", Value::Null, "bad_request"),
            ("cred_envelope", "!!!".into(), "bad_request"),
            (
                "cred_envelope",
                b64_encode(b"plaintext key").into(),
                "bad_request",
            ),
            ("mode", "both".into(), "bad_request"),
            ("extra", 1.into(), "bad_request"),
        ];
        for (k, val, code) in cases {
            let err = check(&with(agent_body(), k, val.clone())).unwrap_err();
            assert_eq!(err.code, code, "{k}={val}");
        }
        let mut other = Envelope::new("0000000000000000").header_line();
        other.extend_from_slice(b"x");
        let err = check(&with(
            agent_body(),
            "cred_envelope",
            b64_encode(&other).into(),
        ))
        .unwrap_err();
        assert_eq!(err.code, "wrong_key");
        let big = b64_encode(&vec![b'x'; MAX_CRED + 1]);
        assert!(check(&with(agent_body(), "cred_envelope", big.into())).is_err());
    }

    #[test]
    fn app_request() {
        let app = serde_json::json!({
            "mode": "app", "eval_id": EID, "upload_hash": "cd".repeat(32),
            "taskset": "github-full", "stages": 2, "score_public": true, "consent": true
        });
        let v = check(&app).unwrap();
        assert_eq!(
            (v.mode, v.replicas, v.stages, v.cred),
            (Mode::App, 1, Some(2), None)
        );
        // A sealed credential (download password) is optional in app mode.
        assert!(
            check(&with(app.clone(), "cred_envelope", sealed_cred().into()))
                .unwrap()
                .cred
                .is_some()
        );
        assert!(check(&with(app.clone(), "stages", Value::Null)).is_err());
        assert!(check(&with(app.clone(), "replicas", 2.into())).is_err());
        assert!(check(&with(app.clone(), "budget", serde_json::json!({}))).is_err());
        assert!(EvalRequest::parse(&vec![b' '; MAX_EVAL_BODY + 1]).is_err());
    }

    fn manifest() -> Value {
        let usage = serde_json::json!({"requests":1,"prompt_tokens":1,"cached_tokens":0,"completion_tokens":1,"reasoning_tokens":0});
        serde_json::json!({
            "schema": 1, "eval_id": EID, "created_at": "2026-10-03T00:00:00Z",
            "taskset": "github-full", "agent": {"name": "octos", "version": "1"},
            "model": "glm-5.3", "public": false,
            "replicas": [
              {"replica": 1, "stages": [
                {"stage": "stage-1", "score": {"status": "failed", "passed": 27, "total": 30}, "usage": usage},
                {"stage": "stage-2", "score": {"status": "failed", "passed": 20, "total": 29}, "usage": usage}
              ]},
              {"replica": 2, "stages": [
                {"stage": "stage-1", "score": {"status": "passed", "passed": 30, "total": 30}, "usage": usage},
                {"stage": "stage-2", "score": null, "usage": usage}
              ]}
            ]
        })
    }

    #[test]
    fn results_and_score() {
        let mut m = manifest();
        m["download"] = serde_json::json!({"sha256": "ef".repeat(32)});
        let body = serde_json::to_vec(&m).unwrap();
        let r = parse_results(&body, EID).unwrap();
        assert_eq!(r.download.as_deref(), Some("ef".repeat(32).as_str()));
        assert_eq!(r.status, "done");
        assert!(r.manifest.get("download").is_none());
        let man: Manifest = serde_json::from_value(r.manifest).unwrap();
        // (27 + 20 + 30) / (30 + 29 + 30) = 77 / 89
        assert_eq!(total_score(&man), Some(0.8652));

        // A v2 manifest with a `sum` snapshot: the total by that rule.
        let mut v2 = manifest();
        v2["schema"] = 2.into();
        v2["replicas"][0]["stages"][0]["score"] =
            serde_json::json!({"status": "scored", "score": 4458.556});
        v2["replicas"][0]["stages"][1]["score"] = serde_json::json!({"status": "scored", "score": -8.5, "items": [{"name": "x", "score": -8.5}]});
        v2["replicas"][1]["stages"][0]["score"] =
            serde_json::json!({"status": "error", "error": "system"});
        v2["scoring"] = serde_json::json!({"aggregate": {"stages": "sum"},
            "display": {"stage": {"name": "观测得分"}, "total": {"name": "总分", "decimals": 2}},
            "plugins": [{"kind": "scorer", "name": "astro-survey", "version": "2"}]});
        let man: Manifest = serde_json::from_value(v2).unwrap();
        assert_eq!(total_score(&man), Some(4450.056));

        assert!(parse_results(&body, "3f2b8c1e-9a4d-4c7e-8b1a-000000000000").is_err());
        assert!(parse_results(b"{\"eval_id\":1}", EID).is_err());
        assert!(parse_results(b"[]", EID).is_err());
        let mut bad_dl = m.clone();
        bad_dl["download"] = serde_json::json!({"sha256": "../x"});
        assert!(parse_results(&serde_json::to_vec(&bad_dl).unwrap(), EID).is_err());

        let mut partial = manifest();
        partial["status"] = "running:stage-2".into();
        let r = parse_results(&serde_json::to_vec(&partial).unwrap(), EID).unwrap();
        assert_eq!(r.status, "running:stage-2");
        partial["status"] = "succeeded".into();
        assert!(parse_results(&serde_json::to_vec(&partial).unwrap(), EID).is_err());
    }
}
