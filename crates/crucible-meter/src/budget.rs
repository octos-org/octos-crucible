//! Optional per-run caps. Off by default.

use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    pub max_requests: Option<u64>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
}

impl Limits {
    pub fn is_unlimited(&self) -> bool {
        self.max_requests.is_none() && self.max_tokens.is_none() && self.max_cost_usd.is_none()
    }
}

#[derive(Default)]
struct Totals {
    requests: u64,
    tokens: u64,
    cost: f64,
}

/// Requests are counted before forwarding; tokens and cost once a
/// response's usage is known. Concurrent in-flight requests can therefore
/// overshoot a token/cost cap by at most one response each.
pub(crate) struct Budget {
    limits: Limits,
    totals: Mutex<Totals>,
}

impl Budget {
    pub(crate) fn new(limits: Limits) -> Self {
        Budget {
            limits,
            totals: Mutex::new(Totals::default()),
        }
    }

    /// Reserve one request, or name the cap that is already reached.
    pub(crate) fn reserve(&self) -> Option<&'static str> {
        let mut t = self.totals.lock().unwrap();
        if self.limits.max_requests.is_some_and(|m| t.requests >= m) {
            return Some("max_requests");
        }
        if self.limits.max_tokens.is_some_and(|m| t.tokens >= m) {
            return Some("max_tokens");
        }
        if self.limits.max_cost_usd.is_some_and(|m| t.cost >= m) {
            return Some("max_cost_usd");
        }
        t.requests += 1;
        None
    }

    pub(crate) fn add(&self, tokens: u64, cost: Option<f64>) {
        let mut t = self.totals.lock().unwrap();
        t.tokens += tokens;
        t.cost += cost.unwrap_or(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps() {
        let b = Budget::new(Limits::default());
        for _ in 0..1000 {
            assert_eq!(b.reserve(), None);
        }
        let b = Budget::new(Limits {
            max_requests: Some(2),
            ..Default::default()
        });
        assert_eq!(
            (b.reserve(), b.reserve(), b.reserve()),
            (None, None, Some("max_requests"))
        );
        let b = Budget::new(Limits {
            max_cost_usd: Some(1.0),
            ..Default::default()
        });
        assert_eq!(b.reserve(), None);
        b.add(10, Some(0.6));
        assert_eq!(b.reserve(), None);
        b.add(10, Some(0.6));
        assert_eq!(b.reserve(), Some("max_cost_usd"));
        // Unknown cost never trips the cost cap.
        let b = Budget::new(Limits {
            max_cost_usd: Some(1.0),
            ..Default::default()
        });
        b.add(1 << 40, None);
        assert_eq!(b.reserve(), None);
    }
}
