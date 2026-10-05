//! Per-user quotas (docs/api.md "配额"): uploads, evals, plugin and taskset
//! registrations. Usage is counted from the tables themselves (the last 24
//! hours, or evals not yet settled), so there are no counters to keep in
//! step. Defaults come from `wrangler.toml` `QUOTA_*`; an administrator can
//! override them per user, or exempt a user (administrators are exempt
//! unless their row says otherwise).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::ApiError;
use crate::util::rfc3339;

pub const DAY_S: u64 = 86_400;

/// One limited thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    UploadsPerDay,
    UploadBytesPerDay,
    EvalsRunning,
    EvalsPerDay,
    PluginsPerDay,
    TasksetsPerDay,
}

pub const ITEMS: [Item; 6] = [
    Item::UploadsPerDay,
    Item::UploadBytesPerDay,
    Item::EvalsRunning,
    Item::EvalsPerDay,
    Item::PluginsPerDay,
    Item::TasksetsPerDay,
];

impl Item {
    /// The API name, also the `quotas` column.
    pub fn name(self) -> &'static str {
        match self {
            Item::UploadsPerDay => "uploads_per_day",
            Item::UploadBytesPerDay => "upload_bytes_per_day",
            Item::EvalsRunning => "evals_running",
            Item::EvalsPerDay => "evals_per_day",
            Item::PluginsPerDay => "plugins_per_day",
            Item::TasksetsPerDay => "tasksets_per_day",
        }
    }

    /// `wrangler.toml` var of the default.
    pub fn var(self) -> String {
        format!("QUOTA_{}", self.name().to_ascii_uppercase())
    }

    pub fn default_limit(self) -> u64 {
        match self {
            Item::UploadsPerDay => 50,
            Item::UploadBytesPerDay => 500 << 20,
            Item::EvalsRunning => 3,
            Item::EvalsPerDay => 20,
            Item::PluginsPerDay => 10,
            Item::TasksetsPerDay => 10,
        }
    }

    fn zh(self) -> &'static str {
        match self {
            Item::UploadsPerDay => "24 小时内上传次数",
            Item::UploadBytesPerDay => "24 小时内上传字节数",
            Item::EvalsRunning => "同时进行中的评测数",
            Item::EvalsPerDay => "24 小时内评测数",
            Item::PluginsPerDay => "24 小时内插件登记数",
            Item::TasksetsPerDay => "24 小时内题目包登记数",
        }
    }

    fn en(self) -> &'static str {
        match self {
            Item::UploadsPerDay => "uploads in 24 h",
            Item::UploadBytesPerDay => "upload bytes in 24 h",
            Item::EvalsRunning => "evals in progress",
            Item::EvalsPerDay => "evals in 24 h",
            Item::PluginsPerDay => "plugin registrations in 24 h",
            Item::TasksetsPerDay => "taskset registrations in 24 h",
        }
    }

    fn idx(self) -> usize {
        ITEMS.iter().position(|i| *i == self).expect("listed")
    }
}

/// The defaults (one per [`ITEMS`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults(pub [u64; 6]);

impl Default for Defaults {
    fn default() -> Self {
        Defaults(ITEMS.map(Item::default_limit))
    }
}

impl Defaults {
    /// From `QUOTA_*` vars; a missing var keeps the built-in default.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Defaults, String> {
        let mut d = Defaults::default();
        for i in ITEMS {
            if let Some(v) = get(&i.var())
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
            {
                d.0[i.idx()] = v
                    .parse()
                    .map_err(|_| format!("{} must be a non-negative integer", i.var()))?;
            }
        }
        Ok(d)
    }
}

/// A user's override row (`quotas`); `None` keeps the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(default)]
    pub uploads_per_day: Option<u64>,
    #[serde(default)]
    pub upload_bytes_per_day: Option<u64>,
    #[serde(default)]
    pub evals_running: Option<u64>,
    #[serde(default)]
    pub evals_per_day: Option<u64>,
    #[serde(default)]
    pub plugins_per_day: Option<u64>,
    #[serde(default)]
    pub tasksets_per_day: Option<u64>,
    /// `None`: administrators exempt, others not.
    #[serde(default)]
    pub exempt: Option<bool>,
}

impl Override {
    pub fn get(&self, i: Item) -> Option<u64> {
        match i {
            Item::UploadsPerDay => self.uploads_per_day,
            Item::UploadBytesPerDay => self.upload_bytes_per_day,
            Item::EvalsRunning => self.evals_running,
            Item::EvalsPerDay => self.evals_per_day,
            Item::PluginsPerDay => self.plugins_per_day,
            Item::TasksetsPerDay => self.tasksets_per_day,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Override::default()
    }
}

/// What a user has used: a count, and when the oldest counted thing leaves
/// the 24-hour window (unix seconds; `None` for evals in progress).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Used {
    pub used: u64,
    pub frees_at: Option<u64>,
}

/// A user's usage, one per [`ITEMS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage(pub [Used; 6]);

/// Limits and usage of one user.
pub struct Quota {
    pub exempt: bool,
    pub limits: [u64; 6],
    pub usage: Usage,
}

impl Quota {
    pub fn new(d: &Defaults, o: Option<&Override>, is_admin: bool, usage: Usage) -> Quota {
        let mut limits = d.0;
        if let Some(o) = o {
            for i in ITEMS {
                if let Some(v) = o.get(i) {
                    limits[i.idx()] = v;
                }
            }
        }
        let exempt = o.and_then(|o| o.exempt).unwrap_or(is_admin);
        Quota {
            exempt,
            limits,
            usage,
        }
    }

    /// Refuse when `adding` more of `item` would go over the limit.
    pub fn check(&self, item: Item, adding: u64) -> Result<(), ApiError> {
        if self.exempt {
            return Ok(());
        }
        let k = item.idx();
        let (limit, u) = (self.limits[k], self.usage.0[k]);
        if u.used.saturating_add(adding) <= limit {
            return Ok(());
        }
        let when = match (item, u.frees_at) {
            (Item::EvalsRunning, _) => {
                "when one of your evals in progress finishes / 进行中的评测结束一个后恢复"
                    .to_owned()
            }
            (_, Some(t)) => format!("from {} (UTC) / {} 后恢复", rfc3339(t), rfc3339(t)),
            (_, None) => "in 24 h / 24 小时后恢复".to_owned(),
        };
        Err(ApiError::new(
            429,
            "quota_exceeded",
            format!(
                "quota exceeded: {} ({}), limit {limit}, used {}; {when}. \
                 超出额度：{}上限 {limit}，已用 {}。如需提高请联系管理员。",
                item.name(),
                item.en(),
                u.used,
                item.zh(),
                u.used,
            ),
        ))
    }

    /// `GET /quota`.
    pub fn view(&self) -> Value {
        let items: Vec<Value> = ITEMS
            .iter()
            .map(|i| {
                let k = i.idx();
                let u = self.usage.0[k];
                let mut v = json!({
                    "name": i.name(),
                    "description": i.zh(),
                    "limit": self.limits[k],
                    "used": u.used,
                    "remaining": self.limits[k].saturating_sub(u.used),
                });
                if let Some(t) = u.frees_at {
                    v["frees_at"] = json!(rfc3339(t));
                }
                v
            })
            .collect();
        json!({"exempt": self.exempt, "window_s": DAY_S, "items": items})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_exemption() {
        let d = Defaults::default();
        let mut usage = Usage::default();
        usage.0[Item::EvalsPerDay.idx()] = Used {
            used: 20,
            frees_at: Some(86_400),
        };
        let q = Quota::new(&d, None, false, usage.clone());
        let e = q.check(Item::EvalsPerDay, 1).unwrap_err();
        assert_eq!((e.status, e.code), (429, "quota_exceeded"));
        assert!(e.message.contains("evals_per_day") && e.message.contains("1970-01-02T00:00:00Z"));
        assert!(q.check(Item::EvalsRunning, 1).is_ok());
        // Admins are exempt by default; a row can say otherwise.
        assert!(
            Quota::new(&d, None, true, usage.clone())
                .check(Item::EvalsPerDay, 1)
                .is_ok()
        );
        let o = Override {
            exempt: Some(false),
            evals_per_day: Some(30),
            ..Default::default()
        };
        let q = Quota::new(&d, Some(&o), true, usage.clone());
        assert!(q.check(Item::EvalsPerDay, 10).is_ok());
        assert!(q.check(Item::EvalsPerDay, 11).is_err());
        let v = q.view();
        assert_eq!(v["exempt"], false);
        assert_eq!(v["items"][3]["remaining"], 10);
    }

    #[test]
    fn defaults_from_vars() {
        let d =
            Defaults::from_lookup(|n| (n == "QUOTA_EVALS_RUNNING").then(|| "5".into())).unwrap();
        assert_eq!(d.0[Item::EvalsRunning.idx()], 5);
        assert_eq!(d.0[Item::EvalsPerDay.idx()], 20);
        assert!(
            Defaults::from_lookup(|n| (n == "QUOTA_EVALS_PER_DAY").then(|| "x".into())).is_err()
        );
    }
}
