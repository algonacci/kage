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
    env: Vec<(String, String)>,
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
        template.extend(config.extra_args.iter().cloned());

        Ok(Self {
            role,
            kind: config.adapter,
            template,
            delivery: config.prompt_delivery,
            timeout: Duration::from_secs(config.timeout_secs),
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
        AdapterKind::ClaudeCode => vec!["claude".into(), "--print".into()],
        AdapterKind::Codex => vec!["codex".into(), "exec".into()],
        AdapterKind::OpenCode => vec!["opencode".into(), "run".into()],
        // Verified against Kamui's own argument parser: `kamui -p <prompt> [--auto-approve]`.
        AdapterKind::Kamui => vec!["kamui".into(), "-p".into()],
        AdapterKind::Command => Vec::new(),
    };

    if let Some(model) = model {
        let flag = match kind {
            AdapterKind::ClaudeCode => Some("--model"),
            AdapterKind::Codex | AdapterKind::OpenCode => Some("-m"),
            // Kamui picks its model from its own config; there is no per-invocation flag.
            AdapterKind::Kamui | AdapterKind::Command => None,
        };
        if let Some(flag) = flag {
            argv.push(flag.to_string());
            argv.push(model.to_string());
        }
    }

    argv.push(PROMPT_PLACEHOLDER.to_string());
    argv
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

        let outcome = proc::run(Spawn {
            program: program.clone(),
            args: args.to_vec(),
            workdir: request.workdir,
            env: self.env.clone(),
            stdin,
            raw_command: None,
            timeout: self.timeout,
            stream_prefix: Some(format!("  [{}]", request.label)),
            log_path: Some(request.log_path),
        })
        .await?;

        Ok(AgentResult {
            code: outcome.code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            timed_out: outcome.timed_out,
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
        assert!(argv.last().unwrap().contains("/tmp/p.md"));
        assert!(!argv.last().unwrap().contains("a very long plan"));
    }

    #[test]
    fn arg_delivery_inlines_the_prompt() {
        let mut config = RoleConfig::preset(AdapterKind::Codex);
        config.prompt_delivery = PromptDelivery::Arg;

        let argv = rendered(&config, "do the thing");

        assert_eq!(argv, vec!["codex", "exec", "do the thing"]);
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
    fn adapter_command_without_a_command_is_rejected_at_load_time() {
        // Better to fail before spawning anything than to discover the gap mid-run.
        let config = RoleConfig::preset(AdapterKind::Command);

        let error = CliAdapter::from_config(Role::Executor, &config).unwrap_err();

        assert!(error.to_string().contains("no `command`"));
    }

    #[test]
    fn extra_args_land_after_the_generated_arguments() {
        let mut config = RoleConfig::preset(AdapterKind::ClaudeCode);
        config.extra_args = vec!["--permission-mode".into(), "acceptEdits".into()];

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
}
