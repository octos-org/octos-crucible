//! Reports built from meter logs and scorer results.
//!
//! - [`usage`]: one usage.jsonl → token totals, cache hit rate and
//!   equivalent cost per model (the prototype's `usage_report.py`).
//! - [`runs`]: several replicas × stages → per-stage and overall mean,
//!   sample standard deviation, min and max, with failed replicas counted
//!   separately.

pub mod runs;
pub mod usage;

/// `1234567` → `1,234,567`.
pub(crate) fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(1000), "1,000");
        assert_eq!(super::thousands(3_500_000), "3,500,000");
    }
}
