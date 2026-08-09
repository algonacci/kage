//! The role → adapter → backend seam.
//!
//! The engine asks for "the planner" and gets back text plus an exit status. It never learns
//! whether that came from a subscription CLI, an HTTP API, or a local model — which is what makes
//! the same workflow runnable on entirely different setups.

pub mod api;
pub mod cli;
pub mod preflight;
pub mod proc;
pub mod stream;

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::{AdapterKind, Config};

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

    /// Whether the backend writes its own deliverable to disk.
    ///
    /// A spawned harness does; an HTTP endpoint cannot. When this is false the engine writes the
    /// artifact from the returned text, and the prompt asks for the deliverable as a reply rather
    /// than naming a path to create.
    fn writes_own_artifacts(&self) -> bool {
        true
    }

    /// The file a human should tail to watch this backend live, when the raw transcript at
    /// `log_path` cannot serve them.
    ///
    /// A streaming harness's transcript is machine events — kept whole as the forensic record, but
    /// unreadable while it grows — so such a backend answers with a second file holding the
    /// rendered view, and the engine announces that one. `None` means the raw log is already the
    /// readable one; answering with a path anyway would leave two identical files side by side and
    /// a user guessing which to trust.
    fn progress_log(&self, _log_path: &std::path::Path) -> Option<PathBuf> {
        None
    }
}

/// Build the adapter a role is configured to use.
///
/// Takes the whole `Config` rather than just the role because an API-backed role resolves its
/// endpoint through the shared `providers` table.
pub fn build(role: Role, config: &Config) -> Result<Box<dyn AgentAdapter>> {
    let role_config = config.roles.get(role);

    if role_config.adapter == AdapterKind::Api {
        // `Config::validate` has already established that the provider and model are present, so
        // anything missing here is a bug rather than a user error.
        let name = role_config
            .provider
            .as_deref()
            .context("an API-backed role must name a provider")?;
        let provider = config
            .providers
            .get(name)
            .with_context(|| format!("provider `{name}` is not declared"))?;

        return Ok(Box::new(api::ApiAdapter::new(
            role,
            name,
            provider,
            role_config,
        )?));
    }

    Ok(Box::new(cli::CliAdapter::from_config(role, role_config)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdapterKind, RoleConfig};

    #[test]
    fn every_role_builds_an_adapter_from_its_preset() {
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();

        for (role, kind) in [
            (Role::Planner, AdapterKind::ClaudeCode),
            (Role::Executor, AdapterKind::OpenCode),
            (Role::Reviewer, AdapterKind::Codex),
        ] {
            let adapter = build(role, &config).unwrap();
            assert!(adapter.describe().contains(&kind.to_string()));
            assert!(
                adapter.writes_own_artifacts(),
                "a spawned harness writes its own files"
            );
        }
    }

    #[test]
    fn an_api_role_resolves_its_endpoint_through_the_providers_table() {
        let mut config: Config = serde_yaml_ng::from_str("{}").unwrap();
        config.providers.insert(
            "orvix".to_string(),
            crate::config::Provider {
                base_url: "https://api.orvix.id/v1".to_string(),
                api_key_env: "KAGE_BUILD_TEST_KEY".to_string(),
                kind: crate::config::schema::ProviderKind::OpenaiCompatible,
            },
        );
        config.roles.planner = RoleConfig {
            adapter: AdapterKind::Api,
            provider: Some("orvix".to_string()),
            model: Some("anthropic/claude-opus-5".to_string()),
            ..RoleConfig::preset(AdapterKind::Api)
        };
        config.validate().expect("a complete api role is valid");

        // SAFETY: single-threaded test scope with a variable unique to this test.
        unsafe { std::env::set_var("KAGE_BUILD_TEST_KEY", "k") };
        let adapter = build(Role::Planner, &config).unwrap();
        unsafe { std::env::remove_var("KAGE_BUILD_TEST_KEY") };

        assert!(adapter.describe().contains("orvix"));
        assert!(
            !adapter.writes_own_artifacts(),
            "an endpoint cannot touch the filesystem"
        );
    }
}
