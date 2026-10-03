//! Shared types of octos-crucible.
//!
//! Modules only talk to each other through files; this crate is the schema
//! of those files. It does no IO so the same definitions compile into the
//! native tools and the Cloudflare Worker (wasm32).

pub mod agent;
pub mod blob;
pub mod envelope;
pub mod manifest;
pub mod netpolicy;
pub mod score;
pub mod taskset;
pub mod usage;

pub use agent::AgentSpec;
pub use blob::BlobRef;
pub use envelope::Envelope;
pub use manifest::Manifest;
pub use score::{ScoreResult, ScoreStatus};
pub use taskset::TaskSet;
pub use usage::UsageRecord;

/// Names used for agents, tasksets and stages: safe as path components,
/// release asset names and shell words.
pub fn is_slug(s: &str, max_len: usize) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max_len
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

#[cfg(test)]
mod tests {
    #[test]
    fn slug() {
        assert!(super::is_slug("my-agent", 40));
        assert!(super::is_slug("a1", 40));
        assert!(!super::is_slug("-x", 40));
        assert!(!super::is_slug("A", 40));
        assert!(!super::is_slug("../x", 40));
        assert!(!super::is_slug("", 40));
        assert!(!super::is_slug(&"a".repeat(41), 40));
    }
}
