//! Kage — an engineering workflow orchestrator for AI coding agents.
//!
//! Kage does not implement a coding agent. It drives the ones that already exist: it plans with an
//! expensive model, implements with a cheap one, verifies mechanically, reviews with a third, and
//! loops on the findings until the work passes or the iteration budget runs out.
//!
//! The organising idea is to spend intelligence where it has leverage. Architecture and review are
//! worth a premium model; reading files, editing code, and re-running tests are not.

mod adapters;
mod cli;
mod config;
mod engine;
mod git;
mod paths;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::engine::workflow::Options;

#[derive(Parser)]
#[command(
    name = "kage",
    version,
    about = "Engineering workflow orchestrator for AI coding agents",
    long_about = "Kage runs a plan -> execute -> test -> review -> fix loop across the coding \
                  agents you already have installed, keeping the expensive models on the \
                  decisions that matter."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create .kage/ with a starter configuration
    Init {
        /// Overwrite an existing configuration
        #[arg(long)]
        force: bool,
    },

    /// Run the full workflow on a task
    Run {
        /// What you want built, in plain language. Omit when passing --task-file.
        task: Option<String>,

        /// Read the task from a file instead of the command line
        ///
        /// A brief long enough to be worth writing down is a brief the shell will mangle: a
        /// newline ends the argument, and Windows caps a command line near 32k characters. Both
        /// failures are silent — the run starts, and plans whatever survived.
        #[arg(long, value_name = "PATH", conflicts_with = "task")]
        task_file: Option<std::path::PathBuf>,

        /// Fix attempts allowed after a failing review (overrides the config)
        #[arg(long)]
        max_iterations: Option<usize>,

        /// Let agents edit your working tree directly instead of an isolated worktree
        #[arg(long)]
        no_isolate: bool,

        /// Skip planning: start at EXECUTE with the task as the executor's instruction
        #[arg(long)]
        skip_plan: bool,

        /// Do not run the validation commands once before the first phase
        #[arg(long)]
        skip_gate_check: bool,
    },

    /// Show a run's state, artifacts, and history
    Status {
        /// Which run to show. Defaults to the most recent.
        run_id: Option<String>,

        /// List every run instead
        #[arg(long)]
        all: bool,
    },

    /// Continue an interrupted run
    Resume {
        /// Which run to continue. Defaults to the most recent.
        run_id: Option<String>,
    },

    /// Remove worktree checkouts left by finished runs (their branches are kept)
    Clean {
        /// Also remove worktrees of runs that have not finished
        #[arg(long)]
        all: bool,
    },

    /// Check which tools and integrations are available
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let cwd = cli::current_dir()?;

    match args.command {
        Command::Init { force } => cli::init(&cwd, force),

        Command::Run {
            task,
            task_file,
            max_iterations,
            no_isolate,
            skip_plan,
            skip_gate_check,
        } => {
            let task = cli::resolve_task(task, task_file)?;
            cli::run(
                &cwd,
                &task,
                Options {
                    max_iterations,
                    no_isolate,
                    skip_plan,
                    skip_gate_check,
                },
            )
            .await
        }

        Command::Status { run_id, all } => cli::status::run(&cwd, run_id.as_deref(), all),

        Command::Resume { run_id } => cli::resume(&cwd, run_id.as_deref()).await,

        Command::Clean { all } => cli::clean(&cwd, all).await,

        Command::Doctor => cli::doctor::run(&cwd).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_task_with_spaces_stays_one_argument() {
        let args = Cli::try_parse_from(["kage", "run", "Implement rate limiting for the API"])
            .expect("a quoted task is a single positional argument");

        match args.command {
            Command::Run { task, .. } => {
                assert_eq!(task.as_deref(), Some("Implement rate limiting for the API"));
            }
            _ => panic!("expected the run command"),
        }
    }

    #[test]
    fn run_flags_are_parsed() {
        let args = Cli::try_parse_from([
            "kage",
            "run",
            "task",
            "--max-iterations",
            "5",
            "--no-isolate",
        ])
        .unwrap();

        match args.command {
            Command::Run {
                max_iterations,
                no_isolate,
                skip_plan,
                ..
            } => {
                assert_eq!(max_iterations, Some(5));
                assert!(no_isolate);
                assert!(!skip_plan, "absent by default");
            }
            _ => panic!("expected the run command"),
        }
    }

    #[test]
    fn skip_plan_is_parsed() {
        let args = Cli::try_parse_from(["kage", "run", "task", "--skip-plan"]).unwrap();

        match args.command {
            Command::Run { skip_plan, .. } => assert!(skip_plan),
            _ => panic!("expected the run command"),
        }
    }

    #[test]
    fn a_task_can_come_from_a_file_instead_of_argv() {
        let args = Cli::try_parse_from(["kage", "run", "--task-file", "PROMPT.md"])
            .expect("no positional");

        match args.command {
            Command::Run {
                task, task_file, ..
            } => {
                assert!(task.is_none());
                assert_eq!(task_file.unwrap().to_string_lossy(), "PROMPT.md");
            }
            _ => panic!("expected the run command"),
        }
    }

    #[test]
    fn a_task_and_a_task_file_cannot_both_be_given() {
        // Silently preferring one would hide which brief the run actually used.
        assert!(
            Cli::try_parse_from(["kage", "run", "inline", "--task-file", "PROMPT.md"]).is_err()
        );
    }

    #[test]
    fn a_run_with_no_task_at_all_is_refused_with_the_way_out() {
        let error = crate::cli::resolve_task(None, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("--task-file"), "{error}");
        assert!(
            error.contains("truncates"),
            "the silent failure must be named"
        );
    }

    #[test]
    fn a_task_file_is_read_whole() {
        // The bug this guards: a 1838-line brief passed through argv arrived as its first line,
        // and the run planned a title.
        let path = std::env::temp_dir().join(format!("kage-task-{}.md", std::process::id()));
        std::fs::write(
            &path,
            "# Brief

line two
line three
",
        )
        .unwrap();

        let task = crate::cli::resolve_task(None, Some(path.clone())).unwrap();

        assert_eq!(task.lines().count(), 4);
        assert!(task.contains("line three"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn status_and_resume_take_an_optional_run_id() {
        assert!(matches!(
            Cli::try_parse_from(["kage", "status"]).unwrap().command,
            Command::Status { run_id: None, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["kage", "resume", "run_20260809_001"])
                .unwrap()
                .command,
            Command::Resume { run_id: Some(_) }
        ));
    }
}
