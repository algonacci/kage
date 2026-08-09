//! Command implementations.

pub mod doctor;
pub mod status;

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{self, Project};
use crate::engine::workflow::{self, Options};
use crate::state::{Commitment, Phase, RunState, store};

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
/// survives, so work that was never merged is still recoverable afterwards. A finished run whose
/// work was never committed is kept — `--force` removal would destroy the only copy — unless
/// `--all` is passed, which overrides every guard.
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

        // The branch only holds the work if it was committed there. Removing a checkout whose work
        // never reached the branch destroys the work, so that checkout stays unless --all is given.
        if !all && let Some(reason) = uncommitted_reason(&state) {
            println!("kept {id}: {reason} — use --all to remove it anyway");
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

    if let Some(line) = planning_line(state) {
        println!("{line}");
    }

    if let Some(error) = &state.error {
        println!("\n{error}");
    }

    // A failed or blocked run may still have produced work worth recovering, so every terminal
    // phase says where it went; `outcome_hint` handles all of them.
    if state.phase.is_terminal() {
        println!("\n{}", workflow::outcome_hint(state));
    }

    println!("\nDetails:  kage status {}", state.id);
}

/// The extra line the closing summary prints for a run that had no planning phase.
///
/// Without it the summary reads exactly like a run whose planner produced nothing.
fn planning_line(state: &RunState) -> Option<String> {
    state
        .skip_plan
        .then(|| format!("Planning: {}", workflow::PLANNING_SKIPPED))
}

/// Why a finished run's checkout must not be removed.
///
/// `kage clean` tells the user the branch still holds the work. When the commit never happened
/// that sentence is a lie and `--force` removal destroys the only copy, so the checkout stays.
fn uncommitted_reason(state: &RunState) -> Option<String> {
    match &state.commit {
        Some(Commitment::Committed { .. }) | Some(Commitment::NothingToCommit { .. }) => None,
        Some(Commitment::Failed { reason }) => {
            Some(format!("its work was never committed ({reason})"))
        }
        None => Some("its work was never committed".to_string()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_worktree_whose_work_was_never_committed_is_kept() {
        let mut state = RunState::new(
            "run_1".to_string(),
            "task".to_string(),
            PathBuf::from("."),
            3,
        );
        assert_eq!(
            uncommitted_reason(&state).unwrap(),
            "its work was never committed"
        );

        state.commit = Some(Commitment::Failed {
            reason: "git died".to_string(),
        });
        assert_eq!(
            uncommitted_reason(&state).unwrap(),
            "its work was never committed (git died)"
        );

        state.commit = Some(Commitment::Committed {
            sha: "abc".to_string(),
            branch: "kage/run_1".to_string(),
            files_changed: 1,
            created: true,
        });
        assert!(uncommitted_reason(&state).is_none());

        state.commit = Some(Commitment::NothingToCommit {
            branch: "kage/run_1".to_string(),
        });
        assert!(uncommitted_reason(&state).is_none());
    }

    #[test]
    fn the_closing_summary_says_when_planning_was_skipped() {
        let mut state = RunState::new(
            "run_1".to_string(),
            "task".to_string(),
            PathBuf::from("."),
            3,
        );

        assert_eq!(
            planning_line(&state),
            None,
            "a normal run's summary must not claim planning was skipped"
        );

        state.skip_plan = true;
        let line =
            planning_line(&state).unwrap_or_else(|| panic!("a plan-free run must be announced"));
        assert!(line.starts_with("Planning:"), "{line}");
        assert!(line.contains("skipped"), "{line}");
    }
}
