//! `agent.json`: how to start an agent package. See docs/agent-contract.md.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub schema: u32,
    /// `[a-z0-9][a-z0-9-]{0,39}`.
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Command run in the container for every stage. `None` keeps the
    /// image's own ENTRYPOINT/CMD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// The agent calls the model with `stream: true`. The meter then adds
    /// `stream_options.include_usage` when the agent leaves it out, so tokens
    /// can still be counted.
    #[serde(default)]
    pub streaming: bool,
}

fn default_version() -> String {
    "0".into()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentSpecError {
    #[error("agent.json: schema must be 1")]
    Schema,
    #[error("agent.json: name must match [a-z0-9][a-z0-9-]{{0,39}}")]
    Name,
    #[error("agent.json: entrypoint must be a non-empty list of non-empty strings")]
    Entrypoint,
}

impl AgentSpec {
    pub fn validate(&self) -> Result<(), AgentSpecError> {
        if self.schema != 1 {
            return Err(AgentSpecError::Schema);
        }
        if !crate::is_slug(&self.name, 40) {
            return Err(AgentSpecError::Name);
        }
        if let Some(ep) = &self.entrypoint
            && (ep.is_empty() || ep.iter().any(String::is_empty))
        {
            return Err(AgentSpecError::Entrypoint);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_and_defaults() {
        let a: AgentSpec = serde_json::from_str(r#"{"schema":1,"name":"my-agent"}"#).unwrap();
        a.validate().unwrap();
        assert_eq!(a.version, "0");
        assert!(!a.streaming);
        assert!(a.entrypoint.is_none());
    }

    #[test]
    fn rejects() {
        for raw in [
            r#"{"schema":2,"name":"x"}"#,
            r#"{"schema":1,"name":"../x"}"#,
            r#"{"schema":1,"name":"x","entrypoint":[]}"#,
            r#"{"schema":1,"name":"x","entrypoint":[""]}"#,
        ] {
            let a: AgentSpec = serde_json::from_str(raw).unwrap();
            assert!(a.validate().is_err(), "{raw}");
        }
    }
}
