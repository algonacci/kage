//! Command implementations.

pub mod doctor;
pub mod status;

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{self, Project};
use crate::engine::workflow::{self, Options};
use crate::state::{Phase, RunState, store};

/// `kage init` — scaffold `.kage/` in the current directory.
pub fn init(cwd: &Path, force: bool) -> Result<()> {
    let path = config::init(cwd, force)?;

    println!("Initialized Kage in {}", cwd.join(".kage").display());
    println!(
        "\nEdit {} to bind roles to the tools you have.",
        path.display()
    );
    println!("Then check your setup:  kage doctor");

    Ok(())
}

/// `kage run "<task>"` — plan, execute, test, review, fix, verify.
pub async fn run(cwd: &Path, task: &str, options: Options) -> Result<()> {
    if task.trim().is_empty() {
        bail!("the task cannot be empty");
    }

    let project = Project::discover(cwd)?;
    let config = project.load_config()?;

    println!("Task: {task}");

    let state = workflow::start(&project, &config, task, &options).await?;
    report(&state);

    exit_for(&state)
}

/// `kage resume [run_id]` — continue an interrupted run.
pub async fn resume(cwd: &Path, run_id: Option<&str>) -> Result<()> {
    let project = Project::discover(cwd)?;
    let config = project.load_config()?;
    let state = store::resolve(&project, run_id)?;

    if state.phase == Phase::Completed {
        println!("Run {} already completed — nothing to resume.", state.id);
        return Ok(());
    }

    println!("Resuming {} from `{}`", state.id, state.phase);

    let state = workflow::resume(&project, &config, state).await?;
    report(&state);

    exit_for(&state)
}

/// `kage clean` — reclaim the disk that finished runs are holding.
///
/// Every isolated run leaves a full checkout behind, so a repository worked on for a week quietly
/// accumulates a dozen copies of itself. Only the checkout is removed: the `kage/<run_id>` branch
/// survives, so work that was never merged is still recoverable afterwards.
pub async fn clean(cwd: &Path, all: bool) -> Result<()> {
    let project = Project::discover(cwd)?;
    let mut removed = 0usize;

    for id in store::list_run_ids(&project)? {
        let Ok(state) = store::load(&project, &id) else {
            continue;
        };
        let Some(worktree) = &state.worktree else {
            continue;
        };

        // An unfinished run's worktree holds work in progress that `kage resume` still needs.
        if !state.phase.is_terminal() && !all {
            continue;
        }
        if !worktree.path.exists() {
            continue;
        }

        match crate::git::worktree::remove(&project.root, &worktree.path).await {
            Ok(()) => {
                println!("removed {}  (branch {} kept)", id, worktree.branch);
                removed += 1;
            }
            Err(error) => println!("could not remove {id}: {error:#}"),
        }
    }

    if removed == 0 {
        println!("Nothing to clean.");
        if !all {
            println!(
                "Unfinished runs are kept so `kage resume` still works — use --all to \
                      remove those too."
            );
        }
    } else {
        println!(
            "\nRemoved {removed} worktree(s). Their branches are still there:  git branch --list 'kage/*'"
        );
    }

    Ok(())
}

fn report(state: &RunState) {
    println!("\n{}", "-".repeat(60));
    println!("Run {} finished: {}", state.id, state.phase);

    if let Some(error) = &state.error {
        println!("\n{error}");
    }

    if state.phase == Phase::Completed {
        println!("\n{}", workflow::outcome_hint(state));
    }

    println!("\nDetails:  kage status {}", state.id);
}

/// Map a terminal phase to a process exit code.
///
/// A run that failed must not exit 0. Kage is meant to be chained into scripts and CI, and there
/// the exit code is the only signal that gets read.
fn exit_for(state: &RunState) -> Result<()> {
    match state.phase {
        Phase::Completed => Ok(()),
        Phase::Blocked => bail!("run {} is blocked and needs a human", state.id),
        _ => bail!("run {} did not complete", state.id),
    }
}

/// Resolve the directory commands operate on.
pub fn current_dir() -> Result<std::path::PathBuf> {
    std::env::current_dir().context("cannot determine the current directory")
}
