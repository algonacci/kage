//! Serde shapes for `.kage/config.yaml`.
//!
//! The workflow engine never names a model or a vendor. It names *roles*, and a role is bound to
//! an adapter here. That indirection is the whole point: swapping DeepSeek for a local model is a
//! config edit, not a code change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Top-level `.kage/config.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub roles: Roles,
    /// `loop` is a Rust keyword, so the field is renamed rather than raw-identified.
    #[serde(rename = "loop", default)]
    pub loop_config: LoopConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub validation: Validation,
}

fn default_version() -> u32 {
    1
}

/// The three roles of the v0.1 vertical slice: one planner, one executor, one reviewer.
///
/// Extra roles (architect, plan critic, security reviewer) belong to later versions; adding them
/// here means adding a phase to the state machine, so they are deliberately absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    #[serde(default = "default_planner")]
    pub planner: RoleConfig,
    #[serde(default = "default_executor")]
    pub executor: RoleConfig,
    #[serde(default = "default_reviewer")]
    pub reviewer: RoleConfig,
}

impl Default for Roles {
    fn default() -> Self {
        Self {
            planner: default_planner(),
            executor: default_executor(),
            reviewer: default_reviewer(),
        }
    }
}

fn default_planner() -> RoleConfig {
    RoleConfig::preset(AdapterKind::ClaudeCode)
}

fn default_executor() -> RoleConfig {
    RoleConfig::preset(AdapterKind::OpenCode)
}

fn default_reviewer() -> RoleConfig {
    RoleConfig::preset(AdapterKind::Codex)
}

/// Which harness fills a role, and how to invoke it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    pub adapter: AdapterKind,
    /// Passed through the adapter's model flag. `None` means "let the harness pick its default",
    /// which is what subscription CLIs usually want.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Full argv override, required for `adapter: command`. Supports `{prompt}` and `{prompt_file}`
    /// placeholders. The first element is the program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub prompt_delivery: PromptDelivery,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Appended verbatim after the generated arguments. This is the escape hatch for harness flags
    /// Kage does not model (`--permission-mode`, `--sandbox`, and friends).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

fn default_timeout_secs() -> u64 {
    1800
}

impl RoleConfig {
    /// A role wired to a known harness with that harness's conventional flags.
    pub fn preset(adapter: AdapterKind) -> Self {
        Self {
            adapter,
            model: None,
            command: None,
            prompt_delivery: PromptDelivery::default(),
            timeout_secs: default_timeout_secs(),
            extra_args: adapter.default_extra_args(),
            env: BTreeMap::new(),
        }
    }
}

/// The CLI harnesses Kage knows how to spawn.
///
/// `Command` is the generic escape hatch: any binary that accepts a prompt and edits files can fill
/// a role without Kage shipping a preset for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    ClaudeCode,
    Codex,
    /// Spelled as one word by the tool itself, so kebab-case would give the wrong key.
    #[serde(rename = "opencode")]
    OpenCode,
    Kamui,
    Command,
}

impl AdapterKind {
    /// Flags a harness needs to run unattended, which Kage supplies so the user does not have to.
    ///
    /// Without these the harness stops mid-run waiting for a human to approve an edit, and the
    /// orchestrator hangs until its timeout — the exact failure the loop exists to avoid.
    fn default_extra_args(self) -> Vec<String> {
        match self {
            // Kamui gates every tool call behind a prompt unless told otherwise.
            Self::Kamui => vec!["--auto-approve".to_string()],
            _ => Vec::new(),
        }
    }
}

impl std::fmt::Display for AdapterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Kamui => "kamui",
            Self::Command => "command",
        };
        f.write_str(name)
    }
}

/// How the prompt text reaches the harness process.
///
/// `File` is the default because prompts carry whole artifacts (a PLAN.md can run to thousands of
/// lines) and Windows caps a command line at ~32k characters. Writing the prompt to disk and
/// passing a one-line pointer keeps the argv small no matter how large the context grows — and
/// leaves the exact instruction on disk for debugging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptDelivery {
    #[default]
    File,
    Arg,
    Stdin,
}

/// Bounds on the fix/review cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopConfig {
    /// How many fix attempts are allowed after the first review fails. Exhausting them fails the
    /// run rather than looping forever.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

fn default_max_iterations() -> usize {
    3
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
        }
    }
}

/// Git isolation for agent-authored changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    /// Run agents inside a dedicated worktree instead of the user's working tree. On by default:
    /// an autonomous executor with file-write access should not be pointed at uncommitted work.
    #[serde(default = "default_true")]
    pub isolate: bool,
    /// Commit-ish the worktree branches from. `None` uses the current HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            isolate: true,
            base: None,
        }
    }
}

/// Commands that decide, mechanically, whether the executor's work stands up.
///
/// These run in Kage's own process rather than inside an agent, because an agent reporting "tests
/// pass" is a claim and an exit code is evidence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validation {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default = "default_validation_timeout")]
    pub timeout_secs: u64,
}

fn default_validation_timeout() -> u64 {
    900
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_falls_back_to_the_documented_defaults() {
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();

        assert_eq!(config.version, 1);
        assert_eq!(config.loop_config.max_iterations, 3);
        assert!(config.git.isolate);
        assert_eq!(config.roles.planner.adapter, AdapterKind::ClaudeCode);
        assert_eq!(config.roles.executor.adapter, AdapterKind::OpenCode);
        assert_eq!(config.roles.reviewer.adapter, AdapterKind::Codex);
    }

    #[test]
    fn a_partially_specified_role_keeps_its_own_adapter() {
        // Regression guard: a role given only a model must not silently inherit some other role's
        // adapter through a shared `Default` impl.
        let config: Config = serde_yaml_ng::from_str(
            "roles:\n  executor:\n    adapter: kamui\n    model: deepseek-v4-flash\n",
        )
        .unwrap();

        assert_eq!(config.roles.executor.adapter, AdapterKind::Kamui);
        assert_eq!(
            config.roles.executor.model.as_deref(),
            Some("deepseek-v4-flash")
        );
        assert_eq!(config.roles.planner.adapter, AdapterKind::ClaudeCode);
        assert_eq!(config.roles.executor.timeout_secs, 1800);
    }

    #[test]
    fn a_misspelled_key_is_rejected_rather_than_ignored() {
        // Silently dropping `max_iteration` would let a run loop three times when the user asked
        // for one, so unknown keys are a hard error.
        let error = serde_yaml_ng::from_str::<Config>("loop:\n  max_iteration: 1\n").unwrap_err();

        assert!(error.to_string().contains("max_iteration"));
    }

    #[test]
    fn kamui_is_preset_to_run_unattended() {
        let role = RoleConfig::preset(AdapterKind::Kamui);

        assert_eq!(role.extra_args, vec!["--auto-approve".to_string()]);
    }
}
