//! Token counting and pricing shared by the meter and the reports, so a token
//! is counted and priced the same way everywhere. Pure functions; mirrors
//! the prototype's `tools/metering.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Flat token counts of one response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
}

/// USD per 1M tokens. `input` is for uncached prompt tokens, `output` for
/// completion tokens (reasoning included).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

/// `config/pricing.json`, models in file order (patterns are tried in that
/// order). A `None` price means "known model, price not verified".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pricing {
    pub models: Vec<(String, Option<Price>)>,
}

#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("pricing: {0}")]
    Json(#[from] serde_json::Error),
    #[error("pricing: `models` must be an object")]
    NoModels,
    #[error("price for {0:?} needs numeric input and output (USD per 1M tokens)")]
    Incomplete(String),
    #[error("prices must be >= 0")]
    Negative,
}

/// OpenAI-style `usage` object to flat counts. Cached prompt tokens come
/// from `prompt_tokens_details.cached_tokens` (OpenAI, Z.ai) or, when that is
/// absent, `prompt_cache_hit_tokens` (DeepSeek/Kimi style). `None` when the
/// response carried no usage object at all.
pub fn extract_usage(usage: &Value) -> Option<Usage> {
    let obj = usage.as_object()?;
    let cached = obj
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .filter(|v| !v.is_null())
        .or_else(|| obj.get("prompt_cache_hit_tokens"));
    let reasoning = obj
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"));
    Some(Usage {
        prompt_tokens: count(obj.get("prompt_tokens")),
        cached_tokens: count(cached),
        completion_tokens: count(obj.get("completion_tokens")),
        reasoning_tokens: count(reasoning),
    })
}

/// Missing, null, negative or non-numeric counts as 0 (providers disagree
/// on what they omit; a bogus count must not abort metering).
fn count(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f > 0.0).map(|f| f as u64))
            .unwrap_or(0),
        _ => 0,
    }
}

impl Pricing {
    pub fn from_json(raw: &str) -> Result<Self, PriceError> {
        let root: Value = serde_json::from_str(raw)?;
        let models = root
            .get("models")
            .and_then(Value::as_object)
            .ok_or(PriceError::NoModels)?;
        let mut out = Vec::with_capacity(models.len());
        for (name, v) in models {
            let price = if v.is_null() {
                None
            } else {
                Some(parse_price(v)?.ok_or_else(|| PriceError::Incomplete(name.clone()))?)
            };
            out.push((name.clone(), price));
        }
        Ok(Pricing { models: out })
    }

    /// Exact lower-case key first, then `*`/`?` patterns (`kimi*`) in file
    /// order. A matching key whose price is null yields `None`.
    pub fn lookup(&self, model: Option<&str>) -> Option<Price> {
        let m = model.filter(|m| !m.is_empty())?.to_lowercase();
        if let Some((_, p)) = self.models.iter().find(|(k, _)| *k == m) {
            return *p;
        }
        self.models
            .iter()
            .find(|(k, _)| k.contains('*') && glob_match(&k.to_lowercase(), &m))
            .and_then(|(_, p)| *p)
    }
}

/// A price object `{"input", "cached_input", "output"}` (USD/1M), as given
/// by a user or a pricing.json entry. Missing `cached_input` falls back to
/// `input` (no cache discount assumed). `Ok(None)` when input or output is
/// missing.
pub fn parse_price(raw: &Value) -> Result<Option<Price>, PriceError> {
    let num = |k: &str| raw.get(k).and_then(Value::as_f64);
    let (Some(input), Some(output)) = (num("input"), num("output")) else {
        return Ok(None);
    };
    let cached_input = num("cached_input").unwrap_or(input);
    if input < 0.0 || output < 0.0 || cached_input < 0.0 {
        return Err(PriceError::Negative);
    }
    Ok(Some(Price {
        input,
        cached_input,
        output,
    }))
}

/// `(prompt − cached) × input + cached × cached_input + completion × output`,
/// prices per 1M tokens. `None` when the price is unknown.
pub fn cost_usd(price: Option<Price>, prompt: u64, cached: u64, completion: u64) -> Option<f64> {
    let p = price?;
    let cached = cached.min(prompt);
    Some(
        ((prompt - cached) as f64 * p.input
            + cached as f64 * p.cached_input
            + completion as f64 * p.output)
            / 1_000_000.0,
    )
}

/// fnmatch-style match supporting `*` and `?` (the only wildcards used in
/// pricing keys). Iterative with single backtrack point: linear-ish, no
/// recursion blowup.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PRICING: &str = include_str!("../../../config/pricing.json");

    fn pricing() -> Pricing {
        Pricing::from_json(PRICING).unwrap()
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn usage_openai_style() {
        let u = extract_usage(&json!({
            "prompt_tokens": 1000, "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 600},
            "completion_tokens_details": {"reasoning_tokens": 7}
        }))
        .unwrap();
        assert_eq!(
            u,
            Usage {
                prompt_tokens: 1000,
                cached_tokens: 600,
                completion_tokens: 50,
                reasoning_tokens: 7
            }
        );
    }

    #[test]
    fn usage_cache_hit_fallback_and_missing() {
        let u = extract_usage(
            &json!({"prompt_tokens": 10, "completion_tokens": 2, "prompt_cache_hit_tokens": 4}),
        )
        .unwrap();
        assert_eq!(u.cached_tokens, 4);
        // details present but null cached_tokens: still fall back.
        let u = extract_usage(&json!({"prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": null}, "prompt_cache_hit_tokens": 3})).unwrap();
        assert_eq!(u.cached_tokens, 3);
        // details win when both are given.
        let u = extract_usage(&json!({"prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": 5}, "prompt_cache_hit_tokens": 3})).unwrap();
        assert_eq!(u.cached_tokens, 5);
        assert_eq!(extract_usage(&Value::Null), None);
        assert_eq!(extract_usage(&json!([1])), None);
        assert_eq!(extract_usage(&json!({})), Some(Usage::default()));
    }

    #[test]
    fn lookup_rules() {
        let p = pricing();
        assert_eq!(p.lookup(Some("glm-5.3-flash")).unwrap().input, 0.15);
        // Case-insensitive on the model id.
        assert_eq!(p.lookup(Some("GLM-5.3-Flash")).unwrap().output, 0.5);
        // No prefix guessing: exact keys only, plus explicit patterns.
        assert!(p.lookup(Some("glm-5.3-flash-preview")).is_none());
        // kimi* is a pattern whose price is unverified (null).
        assert!(p.lookup(Some("kimi-for-coding")).is_none());
        assert!(p.lookup(Some("gpt-9")).is_none());
        assert!(p.lookup(None).is_none());
        assert!(p.lookup(Some("")).is_none());
    }

    #[test]
    fn pattern_with_price() {
        let p = Pricing::from_json(
            r#"{"models": {"a": null, "foo-*": {"input": 1, "output": 2}, "*": {"input": 9, "output": 9}}}"#,
        )
        .unwrap();
        assert_eq!(p.lookup(Some("FOO-bar")).unwrap().cached_input, 1.0);
        assert_eq!(p.lookup(Some("zzz")).unwrap().input, 9.0);
        // Exact null wins over the catch-all pattern.
        assert!(p.lookup(Some("a")).is_none());
    }

    #[test]
    fn glob() {
        assert!(glob_match("kimi*", "kimi-for-coding"));
        assert!(glob_match("kimi*", "kimi"));
        assert!(!glob_match("kimi*", "xkimi"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(glob_match("gl?-5", "glm-5"));
    }

    #[test]
    fn costs_match_prototype_report() {
        let p = pricing();
        // Per-model sums from the prototype's test_multi_model_with_cache.
        // flash: (1.5M-0.8M)*0.15 + 0.8M*0.03 + 0.15M*0.5 = 0.204
        let flash = cost_usd(p.lookup(Some("glm-5.3-flash")), 1_500_000, 800_000, 150_000).unwrap();
        approx(flash, 0.204);
        // 5.3: 1M*1.4 + 1M*0.26 + 0.2M*4.4 = 2.54
        let big = cost_usd(p.lookup(Some("glm-5.3")), 2_000_000, 1_000_000, 200_000).unwrap();
        approx(big, 2.54);
        approx(flash + big, 2.744);
        // glm-5: 1M*1 + 1M*3.2 = 4.2 (prototype test_cli)
        approx(
            cost_usd(p.lookup(Some("glm-5")), 1_000_000, 0, 1_000_000).unwrap(),
            4.2,
        );
        // Meter's non-streaming record: (1000-600)*0.15 + 600*0.03 + 50*0.5 per 1M
        approx(
            cost_usd(p.lookup(Some("glm-5.3-flash")), 1000, 600, 50).unwrap(),
            (400.0 * 0.15 + 600.0 * 0.03 + 50.0 * 0.5) / 1e6,
        );
        assert!(cost_usd(None, 1, 0, 1).is_none());
        // cached > prompt is clamped, never negative.
        approx(
            cost_usd(p.lookup(Some("glm-5")), 10, 20, 0).unwrap(),
            10.0 * 0.2 / 1e6,
        );
    }

    #[test]
    fn user_price() {
        let p = parse_price(&json!({"input": 2.0, "cached_input": 0.5, "output": 8.0}))
            .unwrap()
            .unwrap();
        approx(
            cost_usd(Some(p), 1_000_000, 500_000, 100_000).unwrap(),
            1.0 + 0.25 + 0.8,
        );
        let p = parse_price(&json!({"input": 1, "output": 2}))
            .unwrap()
            .unwrap();
        assert_eq!(p.cached_input, 1.0);
        assert!(parse_price(&json!({"input": 1})).unwrap().is_none());
        assert!(parse_price(&json!({"input": -1, "output": 2})).is_err());
    }
}
