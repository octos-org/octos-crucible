//! Request bodies, their validation, and the records kept in KV.

use crucible_core::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::ApiError;
use crate::keys::{SealError, check_sealed};
use crate::shard::is_hash;
use crate::util::b64_decode;

pub const MAX_UPLOAD: usize = 25 * 1024 * 1024;
pub const MAX_EVAL_BODY: usize = 64 * 1024;
pub const MAX_CRED: usize = 16 * 1024;
pub const MAX_RESULTS_BODY: usize = 2 * 1024 * 1024;
pub const MAX_REPLICAS: u32 = 5;
pub const CRED_TTL_S: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Upload an agent package; full evaluation with the user's model key.
    Agent,
    /// Upload a finished artifact; score only.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Succeeded | Status::Failed | Status::Cancelled)
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
    pub stages: Option<u32>,
    pub model: Option<String>,
    pub replicas: u32,
    pub budget: Option<Budget>,
    /// Sealed bytes (agent mode only).
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

/// `[a-z0-9][a-z0-9-]{0,63}` — same rule as crucible-core's taskset names.
pub fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `[A-Za-z0-9][A-Za-z0-9._:/@+-]{0,127}`: provider model names such as
/// `gpt-5.1`, `glm-5.3`, `anthropic/claude-x`, `org/model:tag`.
pub fn is_model(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._:/@+-".contains(c))
}

/// GitHub login: `[A-Za-z0-9-]{1,39}`.
pub fn is_login(s: &str) -> bool {
    !s.is_empty() && s.len() <= 39 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
        match self.mode {
            Mode::Agent => {
                let Some(model) = &self.model else {
                    return bad("model is required in agent mode");
                };
                if !is_model(model) {
                    return bad("model must match [A-Za-z0-9][A-Za-z0-9._:/@+-]{0,127}");
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
                let Some(cred) = &self.cred_envelope else {
                    return bad("cred_envelope is required in agent mode");
                };
                if cred.len() > MAX_CRED * 2 {
                    return bad("cred_envelope is too large");
                }
                let Some(sealed) = b64_decode(cred) else {
                    return bad("cred_envelope must be standard base64");
                };
                if sealed.len() > MAX_CRED {
                    return bad("cred_envelope is too large");
                }
                match check_sealed(&sealed, current_key_id) {
                    Ok(()) => {}
                    Err(SealError::WrongKey) => {
                        return Err(ApiError::new(
                            400,
                            "wrong_key",
                            "cred_envelope is not sealed to the current public key (GET /pubkey)",
                        ));
                    }
                    Err(_) => return bad("cred_envelope is not a crucible envelope"),
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
                    cred: Some(sealed),
                    score_public: self.score_public,
                })
            }
            Mode::App => {
                if self.cred_envelope.is_some() {
                    return bad("cred_envelope is not accepted in app mode");
                }
                if self.budget.is_some() {
                    return bad("budget is not accepted in app mode");
                }
                if self.replicas.is_some_and(|r| r != 1) {
                    return bad("replicas must be 1 in app mode");
                }
                if let Some(m) = &self.model
                    && !is_model(m)
                {
                    return bad("model must match [A-Za-z0-9][A-Za-z0-9._:/@+-]{0,127}");
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
                    cred: None,
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
    pub kind: Mode,
    pub size: u64,
    pub created_at: String,
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
    pub stages: u32,
    #[serde(default)]
    pub model: Option<String>,
    pub replicas: u32,
    #[serde(default)]
    pub budget: Option<Budget>,
    pub score_public: bool,
    pub created_at: String,
    pub created_s: u64,
    pub status: Status,
    /// Coarse progress: the running job's name, or a short note.
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub run_id: Option<u64>,
    #[serde(default)]
    pub run_url: Option<String>,
    #[serde(default)]
    pub manifest: Option<Value>,
    /// SHA-256 of the password-protected result zip.
    #[serde(default)]
    pub download_sha256: Option<String>,
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
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_score: Option<u64>,
}

impl EvalRecord {
    pub fn summary(&self) -> EvalSummary {
        let (total_score, max_score) = self
            .manifest
            .as_ref()
            .and_then(|m| serde_json::from_value::<Manifest>(m.clone()).ok())
            .map(|m| score_of(&m))
            .unwrap_or((None, None));
        EvalSummary {
            eval_id: self.eval_id.clone(),
            mode: self.mode,
            taskset: self.taskset.clone(),
            model: self.model.clone(),
            created_at: self.created_at.clone(),
            status: self.status,
            total_score,
            max_score,
        }
    }
}

/// `total_score` = mean over replicas of the summed `passed` of every
/// scored stage; `max_score` = summed `total` of the first replica.
pub fn score_of(m: &Manifest) -> (Option<f64>, Option<u64>) {
    let per_replica: Vec<u64> = m
        .replicas
        .iter()
        .filter(|r| r.stages.iter().any(|s| s.score.is_some()))
        .map(|r| {
            r.stages
                .iter()
                .filter_map(|s| s.score)
                .map(|s| u64::from(s.passed))
                .sum()
        })
        .collect();
    if per_replica.is_empty() {
        return (None, None);
    }
    let mean = per_replica.iter().sum::<u64>() as f64 / per_replica.len() as f64;
    let max = m.replicas.first().map(|r| {
        r.stages
            .iter()
            .filter_map(|s| s.score)
            .map(|s| u64::from(s.total))
            .sum()
    });
    (Some((mean * 100.0).round() / 100.0), max)
}

/// Body of `POST /internal/results/:id`: a Manifest, plus the optional
/// top-level `download` naming the password zip.
pub fn parse_results(body: &[u8], eval_id: &str) -> Result<(Value, Option<String>), ApiError> {
    if body.len() > MAX_RESULTS_BODY {
        return Err(ApiError::too_large(MAX_RESULTS_BODY));
    }
    let raw: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("results must be JSON: {e}")))?;
    let manifest: Manifest = serde_json::from_value(raw.clone())
        .map_err(|e| ApiError::bad_request(format!("not a manifest: {e}")))?;
    if manifest.eval_id != eval_id {
        return Err(ApiError::bad_request(
            "manifest.eval_id does not match the URL",
        ));
    }
    let download = match raw.get("download") {
        None | Some(Value::Null) => None,
        Some(d) => {
            let h = d
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|h| is_hash(h))
                .ok_or_else(|| ApiError::bad_request("download.sha256 must be a SHA-256 hex"))?;
            Some(h.to_owned())
        }
    };
    Ok((raw, download))
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
        assert!(is_model("glm-5.3") && is_model("anthropic/claude-x:v1@2"));
        assert!(!is_model("a b") && !is_model("-x") && !is_model("x;rm") && !is_model(""));
        assert!(!is_model(&"m".repeat(129)));
        assert!(is_login("octo-cat") && !is_login("a/b") && !is_login(&"a".repeat(40)));
        assert!(is_slug("github-full") && !is_slug("GitHub") && !is_slug("a_b"));
    }

    #[test]
    fn agent_request_ok() {
        let v = check(&agent_body()).unwrap();
        assert_eq!(v.mode, Mode::Agent);
        assert_eq!(v.replicas, 3);
        assert_eq!(v.stages, Some(2));
        assert!(v.cred.unwrap().starts_with(b"{\"crucible_envelope\":1"));
        // Defaults.
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
            ("replicas", 6.into(), "bad_request"),
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
            "taskset": "github-full", "score_public": true, "consent": true
        });
        let v = check(&app).unwrap();
        assert_eq!((v.mode, v.replicas, v.cred), (Mode::App, 1, None));
        assert!(check(&with(app.clone(), "cred_envelope", sealed_cred().into())).is_err());
        assert!(check(&with(app.clone(), "replicas", 2.into())).is_err());
        assert!(check(&with(app.clone(), "budget", serde_json::json!({}))).is_err());
        assert!(EvalRequest::parse(&vec![b' '; MAX_EVAL_BODY + 1]).is_err());
    }

    #[test]
    fn results_and_score() {
        let m = serde_json::json!({
            "schema": 1, "eval_id": EID, "created_at": "2026-10-03T00:00:00Z",
            "taskset": "github-full", "agent": {"name": "octos", "version": "1"},
            "model": "glm-5.3", "public": false,
            "replicas": [
              {"replica": 1, "stages": [
                {"stage": "stage-1", "score": {"status": "failed", "passed": 27, "total": 30}, "usage": {"requests":1,"prompt_tokens":1,"cached_tokens":0,"completion_tokens":1,"reasoning_tokens":0}},
                {"stage": "stage-2", "score": {"status": "failed", "passed": 20, "total": 29}, "usage": {"requests":1,"prompt_tokens":1,"cached_tokens":0,"completion_tokens":1,"reasoning_tokens":0}}
              ]},
              {"replica": 2, "stages": [
                {"stage": "stage-1", "score": {"status": "passed", "passed": 30, "total": 30}, "usage": {"requests":1,"prompt_tokens":1,"cached_tokens":0,"completion_tokens":1,"reasoning_tokens":0}},
                {"stage": "stage-2", "score": null, "usage": {"requests":0,"prompt_tokens":0,"cached_tokens":0,"completion_tokens":0,"reasoning_tokens":0}}
              ]}
            ],
            "download": {"sha256": "ef".repeat(32)}
        });
        let body = serde_json::to_vec(&m).unwrap();
        let (raw, dl) = parse_results(&body, EID).unwrap();
        assert_eq!(dl.as_deref(), Some("ef".repeat(32).as_str()));
        let man: Manifest = serde_json::from_value(raw).unwrap();
        assert_eq!(score_of(&man), (Some(38.5), Some(59)));

        assert!(parse_results(&body, "3f2b8c1e-9a4d-4c7e-8b1a-000000000000").is_err());
        assert!(parse_results(b"{\"eval_id\":1}", EID).is_err());
        let mut bad_dl = m.clone();
        bad_dl["download"] = serde_json::json!({"sha256": "../x"});
        assert!(parse_results(&serde_json::to_vec(&bad_dl).unwrap(), EID).is_err());
    }
}
