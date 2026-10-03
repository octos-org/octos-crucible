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
    /// For `web-app` stages whose work dir has no root Dockerfile: the CMD
    /// of the generated Dockerfile, run from `backend/`. Default
    /// `["npm", "start"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_start_cmd: Option<Vec<String>>,
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
    #[error("agent.json: app_start_cmd must be a non-empty list of non-empty strings")]
    AppStartCmd,
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
        if let Some(c) = &self.app_start_cmd
            && (c.is_empty() || c.iter().any(String::is_empty))
        {
            return Err(AgentSpecError::AppStartCmd);
        }
        Ok(())
    }

    pub fn app_start_cmd(&self) -> Vec<String> {
        self.app_start_cmd
            .clone()
            .unwrap_or_else(|| vec!["npm".into(), "start".into()])
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
            r#"{"schema":1,"name":"x","app_start_cmd":[]}"#,
        ] {
            let a: AgentSpec = serde_json::from_str(raw).unwrap();
            assert!(a.validate().is_err(), "{raw}");
        }
    }
}
