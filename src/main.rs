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
        /// What you want built, in plain language
        task: String,

        /// Fix attempts allowed after a failing review (overrides the config)
        #[arg(long)]
        max_iterations: Option<usize>,

        /// Let agents edit your working tree directly instead of an isolated worktree
        #[arg(long)]
        no_isolate: bool,

        /// Skip planning: start at EXECUTE with the task as the executor's instruction
        #[arg(long)]
        skip_plan: bool,
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
            max_iterations,
            no_isolate,
            skip_plan,
        } => {
            cli::run(
                &cwd,
                &task,
                Options {
                    max_iterations,
                    no_isolate,
                    skip_plan,
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
            Command::Run { task, .. } => assert_eq!(task, "Implement rate limiting for the API"),
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
