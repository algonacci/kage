//! `kage doctor` — report what is available before a run depends on it.
//!
//! Kage cannot install or authenticate anything on the user's behalf; it can only tell them what is
//! missing. That is worth a command of its own, because the alternative is discovering a missing
//! harness twenty minutes into a run that already spent a planner's budget.

use std::path::Path;

use anyhow::Result;

use crate::adapters::{preflight, proc};
use crate::config::{AdapterKind, Config, Project, RoleConfig};

const CHECK: &str = "\u{2713}";
const CROSS: &str = "\u{2717}";
const CIRCLE: &str = "\u{25cb}";

/// Harnesses Kage knows how to spawn, with the command that installs each.
const KNOWN_HARNESSES: &[(&str, &str)] = &[
    ("claude", "npm i -g @anthropic-ai/claude-code"),
    ("codex", "npm i -g @openai/codex"),
    ("opencode", "npm i -g opencode-ai"),
    ("kamui", "cargo install kamui"),
];

pub async fn run(cwd: &Path) -> Result<()> {
    println!("Kage v{}\n", env!("CARGO_PKG_VERSION"));

    let git_ok = report_tool("git", "https://git-scm.com/downloads");
    println!();

    // Roles are checked against the project config when there is one, so the report reflects what
    // this project would actually spawn rather than a generic inventory.
    let project = Project::discover(cwd).ok();
    let config = project.as_ref().and_then(|p| match p.load_config() {
        Ok(config) => Some(config),
        Err(error) => {
            println!("{CROSS} .kage/config.yaml\n  {error:#}\n");
            None
        }
    });

    let mut required_missing = Vec::new();

    if let Some(config) = &config {
        println!("Roles");
        for status in preflight::inspect(&config.roles) {
            let model = status
                .config
                .model
                .as_deref()
                .map(|model| format!(" · {model}"))
                .unwrap_or_default();

            // An API-backed role has no program on PATH. What can go wrong instead is a provider
            // that is not declared or a key that is not exported — the same class of problem this
            // command exists to catch before a run pays for a planner.
            if status.config.adapter == AdapterKind::Api {
                let (mark, detail) = api_role_status(config, status.config);
                println!("{mark} {}  api{model}", status.role);
                println!("    {detail}");

                if mark == CROSS {
                    required_missing.push(format!("{} ({detail})", status.role));
                }
                continue;
            }

            let mark = if status.found() { CHECK } else { CROSS };
            println!("{mark} {}  {}{model}", status.role, status.config.adapter);
            println!(
                "    `{}`{}",
                status.program,
                if status.found() { "" } else { "  not found" }
            );

            if !status.found() {
                required_missing.push(status.label());
            }
        }
        println!();
    }

    println!("Harnesses");
    for (program, install) in KNOWN_HARNESSES {
        report_optional(program, install);
    }
    println!();

    match (&project, &config) {
        (Some(project), Some(_)) => {
            println!("Project: {}", project.root.display());
            let runs = crate::state::store::list_run_ids(project)?;
            println!("Runs:    {}", runs.len());
        }
        _ => println!("{CIRCLE} no .kage/ project here — run `kage init` in your project root"),
    }

    println!();
    if !git_ok {
        println!("git is required for isolated runs.");
    }
    if !required_missing.is_empty() {
        println!("{}", preflight::not_ready(&required_missing));
    } else if config.is_some() && git_ok {
        println!("Ready to run.");
    }

    Ok(())
}

/// Whether an API-backed role could actually make its call, and what to say about it.
///
/// The key is only tested for presence — never printed, never partially printed. A prefix is still
/// a secret, and a report is a thing people paste into issues.
fn api_role_status(config: &Config, role: &RoleConfig) -> (&'static str, String) {
    let Some(name) = &role.provider else {
        return (CROSS, "no provider named".to_string());
    };

    let Some(provider) = config.providers.get(name) else {
        return (CROSS, format!("provider `{name}` is not declared"));
    };

    match std::env::var(&provider.api_key_env) {
        Ok(value) if !value.trim().is_empty() => (
            CHECK,
            format!("{} · ${} is set", provider.base_url, provider.api_key_env),
        ),
        Ok(_) => (CROSS, format!("${} is set but empty", provider.api_key_env)),
        Err(_) => (CROSS, format!("${} is not set", provider.api_key_env)),
    }
}

fn report_tool(program: &str, install_hint: &str) -> bool {
    match proc::resolve_program(program) {
        Ok(resolved) => {
            println!("{CHECK} {program}");
            println!("    {}", resolved.path.display());
            true
        }
        Err(_) => {
            println!("{CROSS} {program}  not found");
            println!("    {install_hint}");
            false
        }
    }
}

/// A harness that is simply unused shows as a neutral circle, not a failure.
fn report_optional(program: &str, install_hint: &str) {
    match proc::resolve_program(program) {
        Ok(resolved) => println!("{CHECK} {program}\n    {}", resolved.path.display()),
        Err(_) => println!("{CIRCLE} {program}  not installed\n    {install_hint}"),
    }
}
