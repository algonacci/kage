//! Command implementations.

pub mod doctor;
pub mod queue;
pub mod render;
pub mod shape;
pub mod status;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, Project};
use crate::engine::workflow::{self, Options};
use crate::state::{Artifacts, Commitment, Phase, RunState, store};
use crate::{finish, notify};

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

/// The task text, from wherever the user chose to put it.
///
/// A brief worth writing down is a brief the shell will mangle: a newline ends the argument and
/// Windows caps a command line near 32k characters, so a long task arrives silently truncated and
/// the run plans whatever survived. `--task-file` is the way out, and one of the two must be given.
pub fn resolve_task(task: Option<String>, task_file: Option<PathBuf>) -> Result<String> {
    match (task, task_file) {
        (Some(task), _) => Ok(task),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read the task file at {}", path.display()))?;

            if text.trim().is_empty() {
                bail!("{} is empty", path.display());
            }
            Ok(text)
        }
        (None, None) => bail!(
            "give a task, either on the command line or with --task-file\n\
             a brief longer than a sentence belongs in a file: the shell truncates at the first \
             newline and does it silently"
        ),
    }
}

/// `kage run "<task>"` — plan, execute, test, review, fix, verify.
pub async fn run(cwd: &Path, task: &str, options: Options) -> Result<()> {
    if task.trim().is_empty() {
        bail!("the task cannot be empty");
    }

    let project = Project::discover(cwd)?;
    let config = project.load_config()?;

    // A long brief is quoted rather than echoed: the terminal is not where it is read, and the
    // run's REQUEST.md holds it verbatim either way.
    match task.lines().count() {
        1 => println!("Task: {task}"),
        lines => println!(
            "Task: {} … ({lines} lines)",
            task.lines().next().unwrap_or_default().trim()
        ),
    }

    let state = start(&project, &config, task, &options).await?;
    let hand_over = finish::for_run(&project, &state);
    conclude(&project, &config, &state, hand_over.as_ref()).await;
    exit_for(&state)
}

/// Start a run and see it through to its closing summary; the caller then concludes it.
///
/// Shared by `kage run` and the queue so a queued ticket is reported exactly as a typed one would
/// be — the queue adds order, never a second way of finishing a run. Concluding is the caller's
/// because only the caller knows whether this run's branch is the one to hand over.
pub(crate) async fn start(
    project: &Project,
    config: &Config,
    task: &str,
    options: &Options,
) -> Result<RunState> {
    let state = match workflow::start(project, config, task, options).await {
        Ok(state) => state,
        Err(error) => {
            notify::announce_error(config.notify.as_ref(), &project_name(project), None, &error)
                .await;
            return Err(error);
        }
    };
    report(&state);
    Ok(state)
}

/// Hand the work over, when there is a branch to hand over, then tell the configured webhook.
///
/// Hand-over first, so the notification can carry the pull request it opened — the one link the
/// person reading it on a phone wants next.
pub(crate) async fn conclude(
    project: &Project,
    config: &Config,
    state: &RunState,
    hand_over: Option<&finish::HandOver>,
) {
    let handed_over = match hand_over {
        Some(hand_over) => {
            finish::hand_over(&config.finish, project, &project_name(project), hand_over).await
        }
        None => None,
    };
    announce(project, config, state, handed_over.as_deref()).await;
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

    let run_id = state.id.clone();
    let state = match workflow::resume(&project, &config, state).await {
        Ok(state) => state,
        Err(error) => {
            notify::announce_error(
                config.notify.as_ref(),
                &project_name(&project),
                Some(&run_id),
                &error,
            )
            .await;
            return Err(error);
        }
    };
    report(&state);
    // A queued ticket with a later sibling is not handed over: the next ticket stacks on its
    // branch, and the session's pull request comes from the last one.
    let hand_over = match queue::hand_over_for(&project, &state) {
        queue::Place::NotQueued => finish::for_run(&project, &state),
        queue::Place::Stacked => None,
        queue::Place::Tip(hand_over) => hand_over,
    };
    conclude(&project, &config, &state, hand_over.as_ref()).await;

    exit_for(&state)
}

/// Tell the configured webhook how a run ended.
///
/// Runs after the closing summary so a slow receiver never delays what the terminal shows, and
/// before the exit code so a script chaining runs knows the message has gone.
async fn announce(project: &Project, config: &Config, state: &RunState, handed_over: Option<&str>) {
    let deferred = std::fs::read_to_string(Artifacts::new(project, &state.id).plan())
        .ok()
        .and_then(|plan| workflow::deferred_tasks(&plan));

    notify::announce(
        config.notify.as_ref(),
        &project_name(project),
        state,
        deferred.as_deref(),
        handed_over,
    )
    .await;
}

/// The repository's directory name — what a person juggling several repos recognises.
pub(crate) fn project_name(project: &Project) -> String {
    project
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.root.display().to_string())
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
