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
    /// Commands that prepare a freshly created worktree before any phase runs.
    #[serde(default)]
    pub setup: Setup,
    #[serde(default)]
    pub validation: Validation,
    /// Where to announce that a run stopped. Absent means nobody is told, which is right for a run
    /// someone is watching and wrong for one left going overnight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<Notify>,
    #[serde(default)]
    pub shape: ShapeConfig,
    /// Commands that hand finished work over — push the branch, open a pull request.
    #[serde(default)]
    pub finish: Finish,
}

/// What `kage shape` may show the shaper beyond the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeConfig {
    /// Notes inlined into every shaping prompt, relative to the project root. A directory
    /// contributes its `README.md` only; a file contributes itself.
    ///
    /// An allowlist, never a whole tree. A notes vault worth reading is a vault that also holds
    /// secrets — the one this was built for tracks a `Secrets/` folder on purpose — and everything
    /// inlined here is sent to whichever model fills the shaper. Naming each entry keeps that a
    /// decision rather than an accident of recursion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    /// How many previous turns are replayed verbatim. The PRD and tickets carry everything older;
    /// replaying the whole conversation would grow every turn's cost without bound.
    #[serde(default = "default_history_turns")]
    pub history_turns: usize,
}

fn default_history_turns() -> usize {
    6
}

/// Hand-written for the same reason as [`Validation`]: an omitted `shape:` block must replay the
/// documented number of turns, not the zero a derived `Default` would give.
impl Default for ShapeConfig {
    fn default() -> Self {
        Self {
            context: Vec::new(),
            history_turns: default_history_turns(),
        }
    }
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
    /// Fills `kage shape`, not the loop. Turns a conversation into a PRD and tickets the loop can
    /// run, so it stands before the loop rather than inside it.
    #[serde(default = "default_shaper")]
    pub shaper: RoleConfig,
}

impl Roles {
    /// The configuration backing one role.
    pub fn get(&self, role: crate::adapters::Role) -> &RoleConfig {
        match role {
            crate::adapters::Role::Planner => &self.planner,
            crate::adapters::Role::Executor => &self.executor,
            crate::adapters::Role::Reviewer => &self.reviewer,
            crate::adapters::Role::Shaper => &self.shaper,
        }
    }
}

impl Default for Roles {
    fn default() -> Self {
        Self {
            planner: default_planner(),
            executor: default_executor(),
            reviewer: default_reviewer(),
            shaper: default_shaper(),
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

/// The planner's harness, because shaping is the same kind of reading-and-deciding work. A shorter
/// budget than a loop phase: a conversation turn that takes half an hour is not a conversation.
fn default_shaper() -> RoleConfig {
    RoleConfig {
        timeout_secs: 900,
        ..RoleConfig::preset(AdapterKind::ClaudeCode)
    }
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
    /// How long the harness may produce no output at all before it is presumed hung and the
    /// phase is aborted early. `0` disables the check.
    ///
    /// Total silence is the one mechanical "this will not finish" signal available: every harness
    /// Kage spawns prints as it works, so a process that says nothing for this long is stuck on a
    /// hidden prompt or a dead connection — and without this check it bills its entire timeout
    /// before anything says so.
    #[serde(default = "default_stall_secs")]
    pub stall_secs: u64,
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

/// Generous against the longest silence a working harness has shown, small against the budget the
/// check exists to protect.
fn default_stall_secs() -> u64 {
    600
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
            stall_secs: default_stall_secs(),
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

    /// The flags appended when this config fills `role`.
    ///
    /// The shaper gets none of the write permissions by default. Kage writes every shaping artifact
    /// from the reply, so the harness only needs to read — and a harness left without its write
    /// flag cannot edit the repository while someone is merely talking about it. `claude --print`
    /// denies writes on its own and codex's default sandbox is read-only, so withholding the flags
    /// is the whole mechanism; no read-only flag is guessed at.
    pub fn resolved_extra_args_for(&self, role: crate::adapters::Role) -> Vec<String> {
        if role == crate::adapters::Role::Shaper {
            return self.extra_args.clone().unwrap_or_default();
        }
        self.resolved_extra_args()
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

        // Without its permission flag kamui stops to ask before every tool call and hangs; opencode
        // edits without asking at all, so withholding flags cannot make it read-only. Either would
        // break the promise that talking about a change never makes one.
        if matches!(
            self.roles.shaper.adapter,
            AdapterKind::Kamui | AdapterKind::OpenCode
        ) {
            return Err(format!(
                "the shaper cannot use `adapter: {}`\n\
                 shaping must not edit the repository, and that harness cannot be run read-only \
                 without stopping for approval or writing freely\n\
                 use claude-code, codex, or an `api` provider for `roles.shaper`",
                self.roles.shaper.adapter
            ));
        }

        for (role, config) in [
            ("planner", &self.roles.planner),
            ("executor", &self.roles.executor),
            ("reviewer", &self.roles.reviewer),
            ("shaper", &self.roles.shaper),
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

        if let Some(notify) = &self.notify {
            notify.validate()?;
        }

        Ok(())
    }
}

/// A webhook told when a run stops.
///
/// Deliberately a plain HTTP POST and nothing more specific. Chat delivery belongs to whatever sits
/// behind the URL — a Telegram bot, a Slack relay — so Kage carries no messaging integration of its
/// own and any receiver that accepts JSON will do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    pub url: String,
    /// Name of the environment variable holding the shared secret, never the secret itself — for
    /// the same reason as [`Provider::api_key_env`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_env: Option<String>,
    /// The header the secret travels in. Required alongside `secret_env` rather than defaulted:
    /// receivers disagree on the name, and a guessed one fails as a 401 at the end of a long run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_header: Option<String>,
    /// Which endings are worth a message.
    #[serde(default = "default_notify_on")]
    pub on: Vec<NotifyEvent>,
    /// Static JSON merged into every body, for whatever the receiver wants that Kage cannot know: a
    /// chat id, or `parse_mode: null` to stop a Markdown parser choking on the underscores in a run
    /// id. Kage's own keys win a collision so a stray field cannot replace the message.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, serde_json::Value>,
    #[serde(default = "default_notify_timeout")]
    pub timeout_secs: u64,
}

fn default_notify_on() -> Vec<NotifyEvent> {
    vec![
        NotifyEvent::Completed,
        NotifyEvent::Failed,
        NotifyEvent::Blocked,
    ]
}

/// Short, because the run is already over and the only thing left waiting is the exit code.
fn default_notify_timeout() -> u64 {
    10
}

impl Notify {
    fn validate(&self) -> Result<(), String> {
        let url = self.url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!(
                "`notify.url` must be an http:// or https:// URL, got `{}`",
                self.url
            ));
        }

        match (&self.secret_env, &self.secret_header) {
            (Some(_), None) => Err(
                "`notify.secret_env` is set but `notify.secret_header` is not\n\
                 name the header the receiver reads the secret from, e.g. `X-Notify-Secret`"
                    .to_string(),
            ),
            (None, Some(_)) => Err(
                "`notify.secret_header` is set but `notify.secret_env` is not\n\
                 name the environment variable that holds the secret"
                    .to_string(),
            ),
            _ if self.timeout_secs == 0 => {
                Err("`notify.timeout_secs` must be greater than zero".to_string())
            }
            _ => Ok(()),
        }
    }
}

/// The run endings a webhook can subscribe to. One per terminal phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyEvent {
    Completed,
    Failed,
    Blocked,
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
///
/// Two budgets rather than one, because the loop meets two different kinds of failure. A build
/// that does not compile is mechanical: an exit code catches it and the fix is usually small. A
/// review rejection is judgment: it costs a premium model to obtain and is the loop's real
/// currency. When both drew on one budget, three failed builds could exhaust the run before the
/// reviewer had seen the code at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopConfig {
    /// How many times the reviewer may reject the work before the run fails. Exhausting these
    /// fails the run rather than looping forever.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// How many attempts the executor gets to make validation pass before each review. The budget
    /// refills when a review rejection opens a new fix cycle: every review's findings are a fresh
    /// implementation job with the same right to a compiling result.
    #[serde(default = "default_max_repairs")]
    pub max_repairs: usize,
    /// How long, in total, a run may wait for a harness's usage limit to reset before it fails.
    /// A subscription window refills on its own; a queue left overnight should sleep through it
    /// rather than stop. `0` fails at once, as before.
    #[serde(default = "default_limit_wait_secs")]
    pub limit_wait_secs: u64,
}

fn default_max_iterations() -> usize {
    3
}

fn default_max_repairs() -> usize {
    3
}

/// Six hours: one five-hour window, the longest a subscription harness asks for, with slack.
fn default_limit_wait_secs() -> u64 {
    6 * 60 * 60
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_repairs: default_max_repairs(),
            limit_wait_secs: default_limit_wait_secs(),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Hand-written rather than derived so the whole-block default matches the per-field one. Serde
/// applies `default = "default_validation_timeout"` only when `validation:` is present in the YAML;
/// with the block omitted the *struct* default is used, and a derived one would hand the gate a
/// zero-second budget that kills its first command as "timed out after 0s".
impl Default for Validation {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout_secs: default_validation_timeout(),
        }
    }
}

/// Commands that make a freshly created worktree usable.
///
/// A worktree is a clean checkout of tracked files, and every language that keeps its dependencies
/// outside the repository arrives there with none: `node_modules` is gitignored, so `npm test` in a
/// new worktree fails for reasons that have nothing to do with the change under test. Cargo hides
/// this by sharing a global registry cache, which is why the gap stayed invisible until Kage was
/// pointed at its first JavaScript project.
///
/// These run once, after the worktree is created and before the first phase, and a failure aborts
/// the run before an agent is spawned — a gate that cannot run is not a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default = "default_setup_timeout")]
    pub timeout_secs: u64,
}

fn default_setup_timeout() -> u64 {
    900
}

/// Hand-written for the same reason as [`Validation`]: a `setup:` block left out of the config must
/// still get the documented timeout, not the zero a derived `Default` would produce.
impl Default for Setup {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout_secs: default_setup_timeout(),
        }
    }
}

/// Commands run once a run's work is committed and nothing will be stacked on it.
///
/// Kage stops at a branch on purpose — pushing and opening a pull request need the owner's
/// credentials, which Kage never holds. These commands are the owner's own tools (`git push`,
/// `gh pr create`) run with the run's facts in `KAGE_*` environment variables, so a morning's review
/// starts from a pull request rather than from hunting for `kage/run_…` branches. The facts travel
/// as environment, never substituted into the command text: a task title is free text, and pasting
/// it into a shell line would execute whatever it happened to contain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finish {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default = "default_finish_timeout")]
    pub timeout_secs: u64,
}

fn default_finish_timeout() -> u64 {
    300
}

/// Hand-written like [`Setup`]: an omitted `finish:` block still gets the documented timeout.
impl Default for Finish {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout_secs: default_finish_timeout(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_omitted_finish_block_runs_nothing_and_keeps_its_timeout() {
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();
        assert!(config.finish.commands.is_empty());
        assert_eq!(config.finish.timeout_secs, 300);

        let config: Config = serde_yaml_ng::from_str("finish:\n  commands: [\"true\"]\n").unwrap();
        assert_eq!(config.finish.timeout_secs, 300);
    }

    #[test]
    fn empty_config_falls_back_to_the_documented_defaults() {
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();

        assert_eq!(config.version, 1);
        assert_eq!(config.loop_config.max_iterations, 3);
        assert_eq!(config.loop_config.max_repairs, 3);
        assert!(config.git.isolate);
        assert_eq!(config.roles.planner.adapter, AdapterKind::ClaudeCode);
        assert_eq!(config.roles.executor.adapter, AdapterKind::OpenCode);
        assert_eq!(config.roles.reviewer.adapter, AdapterKind::Codex);
    }

    #[test]
    fn an_omitted_setup_or_validation_block_still_gets_its_timeout() {
        // The bug this guards: both structs derived `Default` while their `timeout_secs` carried a
        // `#[serde(default = "...")]`. Serde only consults the field default when the block is
        // present, so a config without `setup:` or `validation:` deserialized to a zero-second
        // budget and the first command run under it died with "timed out after 0s".
        let config: Config = serde_yaml_ng::from_str("version: 1").unwrap();

        assert_eq!(config.setup.timeout_secs, 900);
        assert_eq!(config.validation.timeout_secs, 900);
        assert_eq!(Setup::default().timeout_secs, 900);
        assert_eq!(Validation::default().timeout_secs, 900);
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
    fn a_notify_block_defaults_to_every_ending_and_a_short_timeout() {
        let config: Config =
            serde_yaml_ng::from_str("notify:\n  url: https://example.test/notify\n").unwrap();

        let notify = config.notify.as_ref().unwrap();
        assert_eq!(
            notify.on,
            vec![
                NotifyEvent::Completed,
                NotifyEvent::Failed,
                NotifyEvent::Blocked
            ]
        );
        assert_eq!(notify.timeout_secs, 10);
        assert!(config.validate().is_ok());
        assert!(
            serde_yaml_ng::from_str::<Config>("{}")
                .unwrap()
                .notify
                .is_none(),
            "no block means nobody is told"
        );
    }

    #[test]
    fn a_notify_secret_and_its_header_come_as_a_pair() {
        // Defaulting the header would turn a wrong guess into a 401 discovered only when a long run
        // ends and its message never arrives.
        let base = "notify:\n  url: https://example.test/notify\n";

        let env_only: Config =
            serde_yaml_ng::from_str(&format!("{base}  secret_env: S\n")).unwrap();
        let error = env_only.validate().unwrap_err();
        assert!(error.contains("secret_header"), "{error}");
        assert!(
            error.contains("X-Notify-Secret"),
            "the remedy names an example: {error}"
        );

        let header_only: Config =
            serde_yaml_ng::from_str(&format!("{base}  secret_header: X\n")).unwrap();
        assert!(header_only.validate().unwrap_err().contains("secret_env"));

        let both: Config =
            serde_yaml_ng::from_str(&format!("{base}  secret_env: S\n  secret_header: X\n"))
                .unwrap();
        assert!(both.validate().is_ok());
    }

    #[test]
    fn a_notify_url_must_be_http() {
        let config: Config = serde_yaml_ng::from_str("notify:\n  url: beepbot.local\n").unwrap();

        assert!(config.validate().unwrap_err().contains("http://"));
    }

    #[test]
    fn an_explicit_null_field_survives_parsing() {
        // `parse_mode: null` is how a Telegram relay is told to send plain text; dropping the null
        // would let its Markdown default choke on the underscores in every run id.
        let config: Config = serde_yaml_ng::from_str(
            "notify:\n  url: https://example.test/\n  fields:\n    parse_mode: null\n",
        )
        .unwrap();

        let fields = &config.notify.unwrap().fields;
        assert!(
            fields
                .get("parse_mode")
                .is_some_and(|value| value.is_null())
        );
    }

    #[test]
    fn the_shaper_defaults_to_claude_without_its_write_permission() {
        // Shaping reads and talks; Kage writes every artifact from the reply. `claude --print`
        // without `--permission-mode acceptEdits` denies writes, so withholding the flag is what
        // keeps a conversation from editing the repository.
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();

        assert_eq!(config.roles.shaper.adapter, AdapterKind::ClaudeCode);
        assert_eq!(config.roles.shaper.timeout_secs, 900);
        assert!(
            config
                .roles
                .shaper
                .resolved_extra_args_for(crate::adapters::Role::Shaper)
                .is_empty()
        );
        assert_eq!(
            config
                .roles
                .planner
                .resolved_extra_args_for(crate::adapters::Role::Planner),
            vec!["--permission-mode".to_string(), "acceptEdits".to_string()],
            "the loop's roles keep their flags"
        );
    }

    #[test]
    fn a_shaper_spelled_out_in_yaml_still_gets_no_write_flag() {
        // The trap `extra_args` already fell into once: naming the adapter must not quietly bring
        // the adapter's defaults back.
        let config: Config =
            serde_yaml_ng::from_str("roles:\n  shaper:\n    adapter: claude-code\n").unwrap();

        assert!(
            config
                .roles
                .shaper
                .resolved_extra_args_for(crate::adapters::Role::Shaper)
                .is_empty()
        );
    }

    #[test]
    fn a_shaper_that_cannot_run_read_only_is_rejected() {
        for adapter in ["kamui", "opencode"] {
            let config: Config =
                serde_yaml_ng::from_str(&format!("roles:\n  shaper:\n    adapter: {adapter}\n"))
                    .unwrap();

            let error = config.validate().unwrap_err();
            assert!(error.contains("shaper cannot use"), "{error}");
            assert!(
                error.contains("claude-code"),
                "the remedy names a way out: {error}"
            );
        }
    }

    #[test]
    fn the_shaper_may_be_backed_by_an_endpoint() {
        let config = api_config("shaper", "    provider: orvix\n    model: m\n");

        assert!(config.validate().is_ok());
    }

    #[test]
    fn an_omitted_shape_block_still_replays_recent_turns() {
        // The same trap as `setup:` and `validation:`: a derived Default would replay zero turns.
        let config: Config = serde_yaml_ng::from_str("{}").unwrap();

        assert_eq!(config.shape.history_turns, 6);
        assert!(config.shape.context.is_empty());
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
