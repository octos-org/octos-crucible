//! Price one meter log: request count, token totals, cache hit rate and
//! equivalent cost, priced per billed model (`resp_model`, else
//! `req_model`).
//!
//! Price source: a user-supplied price (applies to every model) or the
//! pricing table. A model with no known price contributes tokens but no
//! cost, and the total cost is then reported as unknown rather than as a
//! silently-too-low number.

use std::collections::BTreeMap;

use crucible_core::UsageRecord;
use crucible_metering::{Price, Pricing, cost_usd};
use serde::{Deserialize, Serialize};

use crate::thousands;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub requests: u64,
    pub errors: u64,
    pub usage_missing: u64,
    pub budget_exceeded: u64,
    pub model_rejected: u64,
    pub client_aborted: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hit_rate: Option<f64>,
    pub cost_usd: Option<f64>,
    pub price_source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelTotals {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: Option<f64>,
    pub price_source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub total: UsageTotals,
    pub by_model: BTreeMap<String, ModelTotals>,
}

const SRC_USER: &str = "user";
const SRC_TABLE: &str = "pricing.json";

/// Parse a JSONL meter log, skipping blank and unparseable lines (a run
/// killed mid-write can leave a torn last line).
pub fn parse_jsonl(text: &str) -> Vec<UsageRecord> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn summarise(
    records: &[UsageRecord],
    pricing: &Pricing,
    user_price: Option<Price>,
) -> UsageSummary {
    let mut t = UsageTotals {
        requests: records.len() as u64,
        ..Default::default()
    };
    let mut by_model: BTreeMap<String, ModelTotals> = BTreeMap::new();
    for r in records {
        t.errors += u64::from(r.is_error());
        t.usage_missing += u64::from(r.usage_missing);
        t.model_rejected += u64::from(r.model_rejected);
        t.client_aborted += u64::from(r.client_aborted);
        t.budget_exceeded += u64::from(r.budget_exceeded.is_some());
        let Some(prompt) = r.prompt_tokens else {
            continue;
        };
        let model = r.billed_model().unwrap_or("unknown").to_lowercase();
        let m = by_model.entry(model).or_default();
        let counts = [
            prompt,
            r.cached_tokens.unwrap_or(0),
            r.completion_tokens.unwrap_or(0),
            r.reasoning_tokens.unwrap_or(0),
        ];
        m.requests += 1;
        m.prompt_tokens += counts[0];
        m.cached_tokens += counts[1];
        m.completion_tokens += counts[2];
        m.reasoning_tokens += counts[3];
        t.prompt_tokens += counts[0];
        t.cached_tokens += counts[1];
        t.completion_tokens += counts[2];
        t.reasoning_tokens += counts[3];
    }

    let mut cost_total = Some(0.0);
    let mut all_priced = true;
    for (model, m) in by_model.iter_mut() {
        let price = user_price.or_else(|| pricing.lookup(Some(model)));
        m.cost_usd = cost_usd(price, m.prompt_tokens, m.cached_tokens, m.completion_tokens);
        m.price_source = price.map(|_| {
            if user_price.is_some() {
                SRC_USER
            } else {
                SRC_TABLE
            }
            .into()
        });
        match m.cost_usd {
            Some(c) => cost_total = cost_total.map(|t| t + c),
            None => {
                cost_total = None;
                all_priced = false;
            }
        }
    }
    t.cache_hit_rate =
        (t.prompt_tokens > 0).then(|| t.cached_tokens as f64 / t.prompt_tokens as f64);
    t.cost_usd = cost_total.map(|c| (c * 1e6).round() / 1e6);
    t.price_source = if user_price.is_some() {
        Some(SRC_USER.into())
    } else if all_priced && !by_model.is_empty() {
        Some(SRC_TABLE.into())
    } else {
        None
    };
    UsageSummary { total: t, by_model }
}

pub fn markdown(s: &UsageSummary) -> String {
    let t = &s.total;
    let rate = t
        .cache_hit_rate
        .map_or("n/a".into(), |r| format!("{:.1}%", r * 100.0));
    let cost = match t.cost_usd {
        None => "未知价格 (price unknown; tokens only)".to_owned(),
        Some(c) if t.price_source.as_deref() == Some(SRC_USER) => {
            format!("${c:.4} (用户提供价格 / user-supplied price)")
        }
        Some(c) => format!("${c:.4}"),
    };
    let mut out = format!(
        "| requests | errors | prompt | cached | completion | reasoning | cache hit | equiv. cost |\n\
         |---:|---:|---:|---:|---:|---:|---:|---|\n\
         | {} | {} | {} | {} | {} | {} | {rate} | {cost} |\n",
        t.requests,
        t.errors,
        thousands(t.prompt_tokens),
        thousands(t.cached_tokens),
        thousands(t.completion_tokens),
        thousands(t.reasoning_tokens),
    );
    let flags: Vec<String> = [
        ("usage_missing", t.usage_missing),
        ("budget_exceeded", t.budget_exceeded),
        ("model_rejected", t.model_rejected),
        ("client_aborted", t.client_aborted),
    ]
    .iter()
    .filter(|(_, n)| *n > 0)
    .map(|(k, n)| format!("{k}={n}"))
    .collect();
    if !flags.is_empty() {
        out.push_str(&format!("\nflags: {}\n", flags.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> Pricing {
        Pricing::from_json(include_str!("../../../config/pricing.json")).unwrap()
    }

    fn rec(model: &str, prompt: u64, cached: u64, completion: u64, reasoning: u64) -> UsageRecord {
        UsageRecord {
            resp_model: Some(model.into()),
            status: 200,
            prompt_tokens: Some(prompt),
            cached_tokens: Some(cached),
            completion_tokens: Some(completion),
            reasoning_tokens: Some(reasoning),
            ..Default::default()
        }
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    // The four cases of the prototype's tools/tests/test_usage_report.py.

    #[test]
    fn multi_model_with_cache() {
        let recs = vec![
            rec("glm-5.3-flash", 1_000_000, 800_000, 100_000, 10),
            rec("GLM-5.3-Flash", 500_000, 0, 50_000, 0),
            rec("glm-5.3", 2_000_000, 1_000_000, 200_000, 0),
            UsageRecord {
                req_model: Some("glm-5.3".into()),
                status: 429,
                ..Default::default()
            },
            UsageRecord {
                req_model: Some("glm-5.3".into()),
                status: 200,
                usage_missing: true,
                ..Default::default()
            },
        ];
        let s = summarise(&recs, &pricing(), None);
        let t = &s.total;
        assert_eq!(t.requests, 5);
        assert_eq!(t.errors, 1);
        assert_eq!(t.usage_missing, 1);
        assert_eq!(t.prompt_tokens, 3_500_000);
        assert_eq!(t.cached_tokens, 1_800_000);
        assert_eq!(t.completion_tokens, 350_000);
        assert_eq!(t.reasoning_tokens, 10);
        approx(t.cache_hit_rate.unwrap(), 1_800_000.0 / 3_500_000.0);
        approx(s.by_model["glm-5.3-flash"].cost_usd.unwrap(), 0.204);
        approx(s.by_model["glm-5.3"].cost_usd.unwrap(), 2.54);
        approx(t.cost_usd.unwrap(), 2.744);
        assert_eq!(t.price_source.as_deref(), Some("pricing.json"));
    }

    #[test]
    fn unknown_price_makes_total_unknown() {
        let s = summarise(
            &[
                rec("glm-5.3-flash", 1000, 0, 10, 0),
                rec("kimi-for-coding", 1000, 0, 10, 0),
            ],
            &pricing(),
            None,
        );
        assert!(s.by_model["kimi-for-coding"].cost_usd.is_none());
        assert!(s.total.cost_usd.is_none());
        assert!(s.total.price_source.is_none());
        assert!(markdown(&s).contains("未知价格"));
    }

    #[test]
    fn user_price() {
        let price = Price {
            input: 2.0,
            cached_input: 0.5,
            output: 8.0,
        };
        let s = summarise(
            &[rec("my-model", 1_000_000, 500_000, 100_000, 0)],
            &pricing(),
            Some(price),
        );
        approx(s.total.cost_usd.unwrap(), 1.0 + 0.25 + 0.8);
        assert_eq!(s.total.price_source.as_deref(), Some("user"));
        assert!(markdown(&s).contains("用户提供价格"));
    }

    #[test]
    fn jsonl_and_markdown() {
        let line = serde_json::to_string(&rec("glm-5", 1_000_000, 0, 1_000_000, 0)).unwrap();
        let recs = parse_jsonl(&format!("{line}\n\n{{torn"));
        assert_eq!(recs.len(), 1);
        let s = summarise(&recs, &pricing(), None);
        approx(s.total.cost_usd.unwrap(), 4.2);
        let md = markdown(&s);
        assert!(md.contains("$4.2000"), "{md}");
        assert!(md.contains("1,000,000"));
        assert!(!md.contains("flags:"));
    }

    #[test]
    fn flags_listed() {
        let recs = [
            UsageRecord {
                status: 403,
                model_rejected: true,
                ..Default::default()
            },
            UsageRecord {
                status: 429,
                budget_exceeded: Some("max_tokens".into()),
                ..Default::default()
            },
        ];
        let s = summarise(&recs, &pricing(), None);
        assert_eq!(s.total.errors, 2);
        assert!(s.total.price_source.is_none());
        assert!(markdown(&s).contains("flags: budget_exceeded=1, model_rejected=1"));
    }
}
