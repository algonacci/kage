//! The role → adapter → backend seam.
//!
//! The engine asks for "the planner" and gets back text plus an exit status. It never learns
//! whether that came from a subscription CLI, an HTTP API, or a local model — which is what makes
//! the same workflow runnable on entirely different setups.

pub mod cli;
pub mod proc;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::RoleConfig;

/// The roles in the v0.1 loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Planner,
    Executor,
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Executor => "executor",
            Self::Reviewer => "reviewer",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One unit of work handed to a backend.
pub struct AgentRequest {
    /// The full instruction, already assembled with whatever artifacts it needs.
    pub prompt: String,
    /// Where the prompt is written when delivery is `File` — also the debugging record of exactly
    /// what the agent was told.
    pub prompt_file: PathBuf,
    /// Directory the agent runs in: the isolated worktree, or the project root.
    pub workdir: PathBuf,
    pub log_path: PathBuf,
    /// Short tag for streamed output lines, e.g. `executor#2`.
    pub label: String,
}

/// What a backend produced.
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_secs: u64,
}

impl AgentResult {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }
}

/// A backend that can carry out a role.
///
/// v0.1 ships only `CliAdapter`. The trait exists so that an API-backed adapter can be added
/// without the engine changing at all — the mixed CLI/API setups in the design notes depend on
/// both kinds being interchangeable here.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    async fn run(&self, request: AgentRequest) -> Result<AgentResult>;

    /// Human-readable identification for logs and `kage doctor`.
    fn describe(&self) -> String;
}

/// Build the adapter a role is configured to use.
pub fn build(role: Role, config: &RoleConfig) -> Result<Box<dyn AgentAdapter>> {
    Ok(Box::new(cli::CliAdapter::from_config(role, config)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdapterKind;

    #[test]
    fn every_role_builds_an_adapter_from_its_preset() {
        for (role, kind) in [
            (Role::Planner, AdapterKind::ClaudeCode),
            (Role::Executor, AdapterKind::OpenCode),
            (Role::Reviewer, AdapterKind::Codex),
        ] {
            let adapter = build(role, &RoleConfig::preset(kind)).unwrap();
            assert!(adapter.describe().contains(&kind.to_string()));
        }
    }
}
