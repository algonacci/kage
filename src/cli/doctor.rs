//! `kage doctor` — report what is available before a run depends on it.
//!
//! Kage cannot install or authenticate anything on the user's behalf; it can only tell them what is
//! missing. That is worth a command of its own, because the alternative is discovering a missing
//! harness twenty minutes into a run that already spent a planner's budget.

use std::path::Path;

use anyhow::Result;

use crate::adapters::{preflight, proc};
use crate::config::Project;

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
            let mark = if status.found() { CHECK } else { CROSS };
            let model = status
                .config
                .model
                .as_deref()
                .map(|model| format!(" · {model}"))
                .unwrap_or_default();

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
