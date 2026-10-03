//! One line of the meter's `usage.jsonl`. Field names are those of the
//! prototype meter, so old logs and the octos metrics scripts read the same.
//!
//! The record never holds headers, messages, response bodies, the key or
//! the upstream endpoint; adding such a field here is a policy change.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    #[serde(default)]
    pub ts: String,
    /// Request path as sent by the agent (query stripped, max 200 chars).
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub req_model: Option<String>,
    #[serde(default)]
    pub resp_model: Option<String>,
    #[serde(default)]
    pub stream: bool,
    /// HTTP status returned to the agent (upstream's, or the meter's own).
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub ttfb_ms: Option<u64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    /// Prompt tokens served from the provider's cache.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// Same value as `cached_tokens`, under the name octos' metrics sum.
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_tools: Option<u64>,
    /// Status 200 but the response carried no usage.
    #[serde(default)]
    pub usage_missing: bool,
    /// The meter added `stream_options.include_usage` to the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_injected: Option<bool>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub model_rejected: bool,
    /// Which cap was hit: `max_requests`, `max_tokens` or `max_cost_usd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_exceeded: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub client_aborted: bool,
    /// Error class only (never a message: those can name the upstream host).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_error: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl UsageRecord {
    /// The model the tokens are billed under: what the provider says it
    /// served, else what was asked for.
    pub fn billed_model(&self) -> Option<&str> {
        self.resp_model
            .as_deref()
            .filter(|m| !m.is_empty())
            .or(self.req_model.as_deref().filter(|m| !m.is_empty()))
    }

    pub fn is_error(&self) -> bool {
        self.status >= 400
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_prototype_line() {
        let line = r#"{"ts":"2026-10-03T10:00:00Z","path":"/chat/completions","stream":true,"req_bytes":120,"req_model":"glm-5.3-flash","n_tools":2,"usage_injected":true,"resp_model":"glm-5.3-flash","status":200,"elapsed_ms":900,"ttfb_ms":100,"prompt_tokens":2000,"cached_tokens":1500,"prompt_cache_hit_tokens":1500,"completion_tokens":120,"reasoning_tokens":0,"cost_usd":0.0001,"usage_missing":false}"#;
        let r: UsageRecord = serde_json::from_str(line).unwrap();
        assert_eq!(r.prompt_tokens, Some(2000));
        assert_eq!(r.usage_injected, Some(true));
        assert_eq!(r.billed_model(), Some("glm-5.3-flash"));
        let reject: UsageRecord = serde_json::from_str(
            r#"{"path":"/chat/completions","stream":false,"status":429,"elapsed_ms":0,"usage_missing":false,"budget_exceeded":"max_tokens"}"#,
        )
        .unwrap();
        assert!(reject.is_error());
        assert_eq!(reject.budget_exceeded.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn token_fields_serialize_as_null() {
        // Readers that index rec["prompt_tokens"] must find the key.
        let v = serde_json::to_value(UsageRecord::default()).unwrap();
        assert!(v["prompt_tokens"].is_null());
        assert!(v.get("model_rejected").is_none());
    }
}
