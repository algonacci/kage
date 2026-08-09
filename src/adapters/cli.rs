//! CLI-backed adapters: spawn an existing coding agent as a child process.
//!
//! CLI access matters more than API access for the setups Kage targets. A Claude or ChatGPT
//! subscription grants use through the official CLI without API billing, so spawning `claude` is
//! often the only way to put a premium model in a role at a price that makes sense.
//!
//! Kage owns none of these tools' credentials. Authentication stays each harness's business; if
//! `codex` is not logged in, that is a `kage doctor` finding, not something Kage tries to fix.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use crate::adapters::proc::{self, Spawn};
use crate::adapters::stream;
use crate::adapters::{AgentAdapter, AgentRequest, AgentResult, Role};
use crate::config::{AdapterKind, PromptDelivery, RoleConfig};

/// Placeholder replaced with the prompt text (or a pointer to it) in an argv template.
const PROMPT_PLACEHOLDER: &str = "{prompt}";
/// Placeholder replaced with the absolute path of the prompt file.
const PROMPT_FILE_PLACEHOLDER: &str = "{prompt_file}";

/// Spawns a coding-agent CLI and captures what it did.
#[derive(Debug)]
pub struct CliAdapter {
    role: Role,
    kind: AdapterKind,
    /// argv template; element 0 is the program. May contain the placeholders above.
    template: Vec<String>,
    delivery: PromptDelivery,
    timeout: Duration,
    /// `None` when the config's `stall_secs` is 0: silence is then never presumed on.
    stall: Option<Duration>,
    env: Vec<(String, String)>,
    /// How to interpret this argv's stdout, decided from the command line that will actually run.
    stdout_format: stream::OutputFormat,
}

impl CliAdapter {
    pub fn from_config(role: Role, config: &RoleConfig) -> Result<Self> {
        let template = match (&config.command, config.adapter) {
            (Some(command), _) => {
                if command.is_empty() {
                    bail!("role `{role}` sets an empty `command`");
                }
                command.clone()
            }
            (None, AdapterKind::Command) => bail!(
                "role `{role}` uses `adapter: command` but sets no `command` — \
                 give it an argv list such as [\"my-agent\", \"{PROMPT_PLACEHOLDER}\"]"
            ),
            (None, kind) => preset_template(kind, config.model.as_deref()),
        };

        let mut template = template;
        template.extend(config.resolved_extra_args());

        Ok(Self {
            role,
            kind: config.adapter,
            stdout_format: output_format(&template),
            template,
            delivery: config.prompt_delivery,
            timeout: Duration::from_secs(config.timeout_secs),
            stall: (config.stall_secs > 0).then(|| Duration::from_secs(config.stall_secs)),
            env: config
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        })
    }
}

/// The conventional non-interactive invocation for each known harness.
///
/// These are the flags that make a harness run one prompt and exit rather than opening a REPL.
/// They are the piece most likely to drift as the tools evolve, which is why any role can override
/// the whole argv with `command:`.
fn preset_template(kind: AdapterKind, model: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = match kind {
        // `--verbose` is not optional: `claude --print --output-format stream-json` exits with an
        // error without it. `stream-json` is what lets the phase be observed while it runs.
        AdapterKind::ClaudeCode => vec![
            "claude".into(),
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ],
        AdapterKind::Codex => vec!["codex".into(), "exec".into()],
        AdapterKind::OpenCode => vec!["opencode".into(), "run".into()],
        // Verified against Kamui's own argument parser: `kamui -p <prompt> [--auto-approve]`.
        AdapterKind::Kamui => vec!["kamui".into(), "-p".into()],
        // Reached only through a `command:` override; an API role never builds a CliAdapter.
        AdapterKind::Api | AdapterKind::Command => Vec::new(),
    };

    if let Some(model) = model {
        let flag = match kind {
            AdapterKind::ClaudeCode => Some("--model"),
            AdapterKind::Codex | AdapterKind::OpenCode => Some("-m"),
            // Kamui picks its model from its own config; there is no per-invocation flag.
            AdapterKind::Kamui | AdapterKind::Api | AdapterKind::Command => None,
        };
        if let Some(flag) = flag {
            argv.push(flag.to_string());
            argv.push(model.to_string());
        }
    }

    argv.push(PROMPT_PLACEHOLDER.to_string());
    argv
}

/// Which renderer this argv's stdout needs.
///
/// Decided from the command line that will actually run, not from the adapter kind: a user who
/// overrides `command:` may spawn claude with or without streaming, and a different tool that
/// grows an `--output-format stream-json` of its own must never be decoded with claude's schema.
fn output_format(template: &[String]) -> stream::OutputFormat {
    let asks_for_stream = template
        .windows(2)
        .any(|pair| pair[0] == "--output-format" && pair[1] == "stream-json")
        || template
            .iter()
            .any(|arg| arg == "--output-format=stream-json");

    if asks_for_stream && template.first().is_some_and(|program| is_claude(program)) {
        stream::OutputFormat::ClaudeStreamJson
    } else {
        stream::OutputFormat::Passthrough
    }
}

/// Whether an argv's program is the claude CLI, whatever its spelling.
///
/// The stem comparison is case-insensitive so `claude.cmd`, `claude.CMD`, and an absolute path all
/// match — the shim resolution that actually runs the program happens later, in `proc::run`.
fn is_claude(program: &str) -> bool {
    std::path::Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("claude"))
}

#[async_trait]
impl AgentAdapter for CliAdapter {
    async fn run(&self, request: AgentRequest) -> Result<AgentResult> {
        // The prompt is always written to disk, whatever the delivery mode. When a run goes wrong
        // the first question is "what exactly was the agent told?", and this answers it.
        if let Some(parent) = request.prompt_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(&request.prompt_file, &request.prompt)
            .with_context(|| format!("cannot write {}", request.prompt_file.display()))?;

        let prompt_file = crate::paths::canonical(&request.prompt_file);

        let argv = self.render(&request.prompt, &prompt_file);
        let (program, args) = argv.split_first().context("empty command template")?;

        let stdin = matches!(self.delivery, PromptDelivery::Stdin).then(|| request.prompt.clone());

        let progress_path = self.progress_log(&request.log_path);
        let outcome = proc::run(Spawn {
            program: program.clone(),
            args: args.to_vec(),
            workdir: request.workdir,
            env: self.env.clone(),
            stdin,
            raw_command: None,
            timeout: self.timeout,
            stream_prefix: Some(format!("  [{}]", request.label)),
            stdout_format: self.stdout_format,
            // A thinking planner can sit silent for minutes; the heartbeat is what tells a working
            // run from a frozen one.
            heartbeat: Some(proc::HEARTBEAT_INTERVAL),
            stall: self.stall,
            log_path: Some(request.log_path),
            progress_path,
        })
        .await?;

        Ok(AgentResult {
            code: outcome.code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            timed_out: outcome.timed_out,
            stalled: outcome.stalled,
            duration_secs: outcome.duration.as_secs(),
        })
    }

    fn describe(&self) -> String {
        format!(
            "{} via {} (`{}`)",
            self.role,
            self.kind,
            self.template.first().map(String::as_str).unwrap_or("?")
        )
    }

    /// Only a streaming argv gets a rendered view; for every other harness the raw log is already
    /// what a human would read, and a twin file would only raise "which one is real?".
    fn progress_log(&self, log_path: &std::path::Path) -> Option<std::path::PathBuf> {
        (self.stdout_format != stream::OutputFormat::Passthrough)
            .then(|| proc::progress_path(log_path))
    }
}

impl CliAdapter {
    /// Substitute placeholders in the argv template.
    fn render(&self, prompt: &str, prompt_file: &std::path::Path) -> Vec<String> {
        let file_display = prompt_file.to_string_lossy().into_owned();

        let prompt_value = match self.delivery {
            PromptDelivery::Arg => prompt.to_string(),
            PromptDelivery::File => pointer_prompt(&file_display),
            // The text arrives on stdin, so the placeholder argument itself must disappear.
            PromptDelivery::Stdin => String::new(),
        };

        self.template
            .iter()
            .filter_map(|part| {
                if matches!(self.delivery, PromptDelivery::Stdin) && part == PROMPT_PLACEHOLDER {
                    return None;
                }
                Some(
                    part.replace(PROMPT_PLACEHOLDER, &prompt_value)
                        .replace(PROMPT_FILE_PLACEHOLDER, &file_display),
                )
            })
            .collect()
    }
}

/// The one-line instruction that stands in for a full prompt in file-delivery mode.
///
/// Keeping argv tiny is what makes arbitrarily large plans survivable: Windows caps a command line
/// at roughly 32k characters, and an embedded PLAN.md blows past that on a real feature.
fn pointer_prompt(path: &str) -> String {
    format!(
        "Read the file at {path} and carry out the instructions in it exactly. \
         That file is your complete task specification. Begin by reading it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(config: &RoleConfig) -> CliAdapter {
        CliAdapter::from_config(Role::Executor, config).unwrap()
    }

    fn rendered(config: &RoleConfig, prompt: &str) -> Vec<String> {
        adapter(config).render(prompt, std::path::Path::new("/tmp/p.md"))
    }

    #[test]
    fn file_delivery_keeps_the_prompt_out_of_argv() {
        let argv = rendered(
            &RoleConfig::preset(AdapterKind::ClaudeCode),
            "a very long plan",
        );

        assert_eq!(argv[0], "claude");
        assert_eq!(argv[1], "--print");
        // Not `last()`: permission flags follow the prompt in the rendered argv.
        let prompt_arg = argv.iter().find(|a| a.contains("/tmp/p.md")).unwrap();
        assert!(!prompt_arg.contains("a very long plan"));
        assert!(!argv.iter().any(|a| a.contains("a very long plan")));
    }

    #[test]
    fn arg_delivery_inlines_the_prompt() {
        let mut config = RoleConfig::preset(AdapterKind::Codex);
        config.prompt_delivery = PromptDelivery::Arg;

        let argv = rendered(&config, "do the thing");

        assert_eq!(
            argv,
            vec![
                "codex",
                "exec",
                "do the thing",
                "--sandbox",
                "workspace-write"
            ]
        );
    }

    #[test]
    fn stdin_delivery_drops_the_placeholder_argument() {
        let mut config = RoleConfig::preset(AdapterKind::OpenCode);
        config.prompt_delivery = PromptDelivery::Stdin;

        let argv = rendered(&config, "do the thing");

        assert_eq!(argv, vec!["opencode", "run"]);
    }

    #[test]
    fn a_model_is_passed_with_each_harness_own_flag() {
        let mut claude = RoleConfig::preset(AdapterKind::ClaudeCode);
        claude.model = Some("opus-5".into());
        let mut codex = RoleConfig::preset(AdapterKind::Codex);
        codex.model = Some("gpt-5.6-sol".into());

        assert!(rendered(&claude, "x").contains(&"--model".to_string()));
        assert!(rendered(&claude, "x").contains(&"opus-5".to_string()));
        assert!(rendered(&codex, "x").contains(&"-m".to_string()));
    }

    #[test]
    fn claude_is_given_write_permission_before_the_prompt() {
        // Regression guard: without a permission flag the planner runs, thinks, and exits having
        // written nothing, which the loop can only report as a mysterious empty PLAN.md.
        let argv = rendered(&RoleConfig::preset(AdapterKind::ClaudeCode), "x");

        let flag = argv.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(argv[flag + 1], "acceptEdits");
    }

    #[test]
    fn kamui_runs_unattended_and_ignores_the_model_flag() {
        let mut config = RoleConfig::preset(AdapterKind::Kamui);
        config.model = Some("ignored-by-kamui".into());

        let argv = rendered(&config, "x");

        assert_eq!(argv[0], "kamui");
        assert_eq!(argv[1], "-p");
        assert!(argv.contains(&"--auto-approve".to_string()));
        assert!(!argv.contains(&"ignored-by-kamui".to_string()));
    }

    #[test]
    fn a_custom_command_can_place_the_prompt_anywhere() {
        let mut config = RoleConfig::preset(AdapterKind::Command);
        config.command = Some(vec![
            "my-agent".into(),
            "--task".into(),
            PROMPT_FILE_PLACEHOLDER.into(),
            "--yolo".into(),
        ]);

        let argv = rendered(&config, "x");

        assert_eq!(argv, vec!["my-agent", "--task", "/tmp/p.md", "--yolo"]);
    }

    #[test]
    fn a_stall_of_zero_means_silence_is_never_presumed_on() {
        let mut config = RoleConfig::preset(AdapterKind::OpenCode);
        assert_eq!(
            adapter(&config).stall,
            Some(std::time::Duration::from_secs(600)),
            "the default allowance applies unless turned off"
        );

        config.stall_secs = 0;
        assert_eq!(
            adapter(&config).stall,
            None,
            "0 must disable the check, not create a zero-length allowance that kills every spawn"
        );
    }

    #[test]
    fn adapter_command_without_a_command_is_rejected_at_load_time() {
        // Better to fail before spawning anything than to discover the gap mid-run.
        let config = RoleConfig::preset(AdapterKind::Command);

        let error = CliAdapter::from_config(Role::Executor, &config).unwrap_err();

        assert!(error.to_string().contains("no `command`"));
    }

    #[test]
    fn extra_args_land_after_the_generated_arguments() {
        let mut config = RoleConfig::preset(AdapterKind::ClaudeCode);
        config.extra_args = Some(vec!["--permission-mode".into(), "acceptEdits".into()]);

        let argv = rendered(&config, "x");

        assert_eq!(argv[argv.len() - 2], "--permission-mode");
        assert_eq!(argv[argv.len() - 1], "acceptEdits");
    }

    #[tokio::test]
    async fn running_an_adapter_writes_the_prompt_file() {
        let dir = std::env::temp_dir().join(format!("kage-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = RoleConfig::preset(AdapterKind::Command);
        config.command = Some(vec!["cmd".into(), "/C".into(), "echo done".into()]);
        if !cfg!(windows) {
            config.command = Some(vec!["sh".into(), "-c".into(), "echo done".into()]);
        }

        let prompt_file = dir.join("prompt.md");
        let result = adapter(&config)
            .run(AgentRequest {
                prompt: "the full instruction".into(),
                prompt_file: prompt_file.clone(),
                workdir: dir.clone(),
                log_path: dir.join("log.txt"),
                label: "executor".into(),
            })
            .await
            .unwrap();

        assert!(result.success());
        assert_eq!(
            std::fs::read_to_string(&prompt_file).unwrap(),
            "the full instruction"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_is_asked_to_stream_its_output() {
        // Without these two flags the terminal shows nothing for the whole phase: claude buffers
        // everything until exit unless `--output-format stream-json` is requested, and `--verbose`
        // is required alongside it under `--print`.
        let argv = rendered(&RoleConfig::preset(AdapterKind::ClaudeCode), "x");

        let position = argv.iter().position(|a| a == "--output-format").unwrap();
        assert_eq!(argv[position + 1], "stream-json");
        assert!(argv.contains(&"--verbose".to_string()));
    }

    #[test]
    fn a_streaming_claude_has_its_events_decoded() {
        let argv = rendered(&RoleConfig::preset(AdapterKind::ClaudeCode), "x");

        assert_eq!(output_format(&argv), stream::OutputFormat::ClaudeStreamJson);
    }

    #[test]
    fn only_a_streaming_harness_asks_for_a_progress_view() {
        // A streaming transcript is machine events, so the file a human tails must be its rendered
        // twin. A passthrough harness's raw log is already readable, and answering with a path
        // anyway would leave two identical files side by side.
        let log = std::path::Path::new("logs/planner.log");

        let claude = adapter(&RoleConfig::preset(AdapterKind::ClaudeCode));
        assert_eq!(
            claude.progress_log(log).as_deref(),
            Some(std::path::Path::new("logs/planner.progress.log"))
        );

        for kind in [
            AdapterKind::Codex,
            AdapterKind::OpenCode,
            AdapterKind::Kamui,
        ] {
            let plain = adapter(&RoleConfig::preset(kind));
            assert_eq!(
                plain.progress_log(log),
                None,
                "`{kind}` output is already readable; a twin file would only confuse"
            );
        }
    }

    #[test]
    fn a_harness_that_is_not_claude_is_passed_through_verbatim() {
        for kind in [
            AdapterKind::Codex,
            AdapterKind::OpenCode,
            AdapterKind::Kamui,
        ] {
            let argv = rendered(&RoleConfig::preset(kind), "x");
            assert_eq!(
                output_format(&argv),
                stream::OutputFormat::Passthrough,
                "`{kind}` output must not be decoded as claude events"
            );
        }

        // Another program that happens to accept the same flags must never be mis-decoded.
        let mut config = RoleConfig::preset(AdapterKind::Command);
        config.command = Some(vec![
            "my-agent".into(),
            "--output-format".into(),
            "stream-json".into(),
            PROMPT_PLACEHOLDER.into(),
        ]);
        let argv = rendered(&config, "x");
        assert_eq!(output_format(&argv), stream::OutputFormat::Passthrough);
    }

    #[test]
    fn a_custom_command_spawning_claude_with_streaming_is_decoded() {
        // A `command:` override that asks claude for events must get the claude renderer: both the
        // `--output-format=stream-json` spelling and the `.cmd` stem have to match, or a user who
        // configures streaming by hand is left with a wall of JSON.
        let argv = vec![
            "claude.cmd".to_string(),
            "--print".to_string(),
            "--output-format=stream-json".to_string(),
            "--verbose".to_string(),
            PROMPT_PLACEHOLDER.to_string(),
        ];

        assert_eq!(output_format(&argv), stream::OutputFormat::ClaudeStreamJson);
    }
}
