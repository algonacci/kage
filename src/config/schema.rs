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
    /// API endpoints a role can be pointed at, keyed by the name roles refer to.
    ///
    /// Generic rather than one variant per vendor: every provider worth supporting speaks the
    /// OpenAI shape, and the ones that do not are better served by a CLI harness than by a
    /// special case here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Provider>,
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

impl Roles {
    /// The configuration backing one role.
    pub fn get(&self, role: crate::adapters::Role) -> &RoleConfig {
        match role {
            crate::adapters::Role::Planner => &self.planner,
            crate::adapters::Role::Executor => &self.executor,
            crate::adapters::Role::Reviewer => &self.reviewer,
        }
    }
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
    /// Which entry of `providers` backs this role. Required by, and only meaningful for,
    /// `adapter: api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub prompt_delivery: PromptDelivery,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Appended verbatim after the generated arguments. This is the escape hatch for harness flags
    /// Kage does not model.
    ///
    /// Left out, the adapter's own defaults apply — the flags each harness needs to run unattended.
    /// An explicit `extra_args: []` means "none", which is how a user turns those off. A plain
    /// `Vec` could not tell those two apart, and every config that spells its roles out would have
    /// silently lost its permission flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
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
            provider: None,
            prompt_delivery: PromptDelivery::default(),
            timeout_secs: default_timeout_secs(),
            extra_args: None,
            env: BTreeMap::new(),
        }
    }
}

impl RoleConfig {
    /// The flags actually appended to this role's argv.
    ///
    /// Falls back to the adapter's unattended-run defaults when the config says nothing, so a user
    /// who spells out `adapter: claude-code` still gets the permission flag that lets it write.
    pub fn resolved_extra_args(&self) -> Vec<String> {
        self.extra_args
            .clone()
            .unwrap_or_else(|| self.adapter.default_extra_args())
    }
}

impl Config {
    /// Reject configurations that parse but cannot work.
    ///
    /// Checked when the config loads rather than when a role is first used: discovering that the
    /// reviewer is unreachable at the review phase means a planner and an executor have already
    /// been paid for.
    pub fn validate(&self) -> Result<(), String> {
        // The executor reads, edits, compiles, and re-runs tests. A completions endpoint returns
        // text and nothing else, so backing the executor with one means Kage growing its own tool
        // loop — which is to say, rebuilding the coding agents it exists to orchestrate.
        if self.roles.executor.adapter == AdapterKind::Api {
            return Err("the executor cannot be backed by `adapter: api`\n\
                 it has to read, edit, and re-run tests, and an API endpoint only returns text — \
                 supporting it would mean Kage growing its own tool loop, which is the one thing \
                 it exists not to do\n\
                 bind the executor to a CLI harness (claude-code, codex, opencode, kamui) and keep \
                 the API adapter for the planner and reviewer"
                .to_string());
        }

        for (role, config) in [
            ("planner", &self.roles.planner),
            ("executor", &self.roles.executor),
            ("reviewer", &self.roles.reviewer),
        ] {
            if config.adapter != AdapterKind::Api {
                continue;
            }

            let Some(name) = &config.provider else {
                return Err(format!(
                    "role `{role}` uses `adapter: api` but names no `provider`\n\
                     add one under `providers:` and point the role at it"
                ));
            };

            let Some(provider) = self.providers.get(name) else {
                let known: Vec<&str> = self.providers.keys().map(String::as_str).collect();
                return Err(format!(
                    "role `{role}` names provider `{name}`, which is not declared under \
                     `providers:` (known: {})",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                ));
            };

            if config.model.is_none() {
                return Err(format!(
                    "role `{role}` uses `adapter: api` but names no `model`\n\
                     unlike a CLI harness, an endpoint has no configured default to fall back on"
                ));
            }

            if provider.base_url.trim().is_empty() {
                return Err(format!("provider `{name}` has an empty `base_url`"));
            }

            // One wire format exists today. Reading `kind` here is what keeps the field honest:
            // adding a variant will fail to compile until the adapter learns to speak it, rather
            // than silently sending OpenAI-shaped requests to something that is not.
            match provider.kind {
                ProviderKind::OpenaiCompatible => {}
            }
        }

        Ok(())
    }
}

/// One API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Endpoint root, without the `/chat/completions` suffix.
    pub base_url: String,
    /// Name of the environment variable holding the key.
    ///
    /// The key itself is never written here. A config file gets committed, pasted into issues, and
    /// read by the agents this tool spawns; a secret in it would leak through all three.
    pub api_key_env: String,
    /// Reserved for a second wire format. Only the OpenAI shape exists today, so this is accepted
    /// and checked rather than silently ignored.
    #[serde(default)]
    pub kind: ProviderKind,
}

/// Wire formats a provider can speak.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    OpenaiCompatible,
}

/// The CLI harnesses Kage knows how to spawn.
///
/// `Command` is the generic escape hatch: any binary that accepts a prompt and edits files can fill
/// a role without Kage shipping a preset for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    /// An HTTP endpoint rather than a local process. Only for roles whose deliverable is prose.
    Api,
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
    pub fn default_extra_args(self) -> Vec<String> {
        match self {
            // Without this the planner cannot write PLAN.md: `claude --print` denies file writes
            // and there is nobody to approve them, so the run dies having produced nothing.
            // `acceptEdits` permits edits without touching the shell-command guard.
            Self::ClaudeCode => vec!["--permission-mode".to_string(), "acceptEdits".to_string()],
            // Codex defaults to a read-only sandbox; writing is scoped to the worktree it runs in.
            Self::Codex => vec!["--sandbox".to_string(), "workspace-write".to_string()],
            // Kamui gates every tool call behind a prompt unless told otherwise.
            Self::Kamui => vec!["--auto-approve".to_string()],
            // OpenCode carries out its own edits in `run` mode with no permission flag, and an API
            // call has no argv at all.
            Self::Api | Self::OpenCode | Self::Command => Vec::new(),
        }
    }
}

impl std::fmt::Display for AdapterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Api => "api",
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
    fn a_role_spelled_out_in_yaml_still_gets_its_permission_flags() {
        // The bug this guards: `extra_args` used to be a plain Vec, so a config that named its
        // roles explicitly deserialized to an empty list and lost the flag that lets the harness
        // write its deliverable. Every hand-written config was silently broken.
        let config: Config = serde_yaml_ng::from_str(
            "roles:
  planner:
    adapter: claude-code
",
        )
        .unwrap();

        assert_eq!(
            config.roles.planner.resolved_extra_args(),
            vec!["--permission-mode".to_string(), "acceptEdits".to_string()]
        );
    }

    #[test]
    fn an_explicit_empty_list_turns_the_defaults_off() {
        let config: Config = serde_yaml_ng::from_str(
            "roles:
  planner:
    adapter: claude-code
    extra_args: []
",
        )
        .unwrap();

        assert!(config.roles.planner.resolved_extra_args().is_empty());
    }

    /// A config with one provider declared and one role bound to it over the API.
    fn api_config(role: &str, extra: &str) -> Config {
        let yaml = format!(
            "providers:\n  \
             orvix:\n    \
             base_url: 'https://api.orvix.id/v1'\n    \
             api_key_env: K\n\
             roles:\n  \
             {role}:\n    \
             adapter: api\n\
             {extra}"
        );
        serde_yaml_ng::from_str(&yaml).unwrap()
    }

    #[test]
    fn the_executor_cannot_be_backed_by_an_endpoint() {
        // The executor reads, edits, compiles and re-runs tests. Supporting it over a completions
        // endpoint means Kage growing its own tool loop, which is the thing it exists not to do —
        // so this is rejected outright rather than half-supported.
        let config = api_config("executor", "    provider: orvix\n    model: m\n");

        let error = config.validate().unwrap_err();

        assert!(error.contains("executor cannot be backed"), "{error}");
        assert!(error.contains("tool loop"), "{error}");
        assert!(error.contains("opencode"), "the remedy must name a way out");
    }

    #[test]
    fn planner_and_reviewer_may_be_backed_by_an_endpoint() {
        for role in ["planner", "reviewer"] {
            let config = api_config(role, "    provider: orvix\n    model: m\n");
            assert!(config.validate().is_ok(), "{role} should be allowed");
        }
    }

    #[test]
    fn an_api_role_must_name_a_provider_that_exists() {
        let missing = api_config("planner", "    model: m\n");
        assert!(
            missing
                .validate()
                .unwrap_err()
                .contains("names no `provider`")
        );

        let unknown = api_config("planner", "    provider: nope\n    model: m\n");
        let error = unknown.validate().unwrap_err();
        assert!(error.contains("not declared"), "{error}");
        assert!(
            error.contains("known: orvix"),
            "the message must list what is available"
        );
    }

    #[test]
    fn an_api_role_must_name_a_model() {
        // Unlike a CLI harness, an endpoint has no configured default to fall back on.
        let config = api_config("planner", "    provider: orvix\n");

        assert!(config.validate().unwrap_err().contains("names no `model`"));
    }

    #[test]
    fn a_cli_only_config_needs_no_providers() {
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();

        assert!(config.validate().is_ok());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn a_misspelled_key_is_rejected_rather_than_ignored() {
        // Silently dropping `max_iteration` would let a run loop three times when the user asked
        // for one, so unknown keys are a hard error.
        let error = serde_yaml_ng::from_str::<Config>("loop:\n  max_iteration: 1\n").unwrap_err();

        assert!(error.to_string().contains("max_iteration"));
    }

    #[test]
    fn every_harness_that_needs_permission_is_preset_to_run_unattended() {
        // A harness that stops to ask permission cannot write its deliverable, and there is nobody
        // in a non-interactive run to answer. Each of these was read from the tool's own --help.
        assert_eq!(
            RoleConfig::preset(AdapterKind::ClaudeCode).resolved_extra_args(),
            vec!["--permission-mode".to_string(), "acceptEdits".to_string()]
        );
        assert_eq!(
            RoleConfig::preset(AdapterKind::Codex).resolved_extra_args(),
            vec!["--sandbox".to_string(), "workspace-write".to_string()]
        );
        assert_eq!(
            RoleConfig::preset(AdapterKind::Kamui).resolved_extra_args(),
            vec!["--auto-approve".to_string()]
        );
        // OpenCode edits without asking, so an invented flag would only break the spawn.
        assert!(
            RoleConfig::preset(AdapterKind::OpenCode)
                .resolved_extra_args()
                .is_empty()
        );
    }
}
