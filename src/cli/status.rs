//! `kage status` — what happened, and what to do about it.

use std::path::Path;

use anyhow::Result;

use crate::config::Project;
use crate::engine::workflow;
use crate::state::{Artifacts, Phase, RunState, store};

pub fn run(cwd: &Path, run_id: Option<&str>, all: bool) -> Result<()> {
    let project = Project::discover(cwd)?;

    if all {
        return list(&project);
    }

    let state = store::resolve(&project, run_id)?;
    detail(&project, &state)
}

fn list(project: &Project) -> Result<()> {
    let ids = store::list_run_ids(project)?;
    if ids.is_empty() {
        println!("No runs yet. Start one with:  kage run \"<task>\"");
        return Ok(());
    }

    // Newest first: the run someone is asking about is almost always the last one.
    for id in ids.iter().rev() {
        let Ok(state) = store::load(project, id) else {
            continue;
        };
        println!(
            "{:<20} {:<10} {}",
            state.id,
            state.phase.as_str(),
            truncate(&state.task, 60)
        );
    }

    Ok(())
}

fn detail(project: &Project, state: &RunState) -> Result<()> {
    let artifacts = Artifacts::new(project, &state.id);

    println!("Run:     {}", state.id);
    println!("Task:    {}", state.task);
    println!("Phase:   {}", state.phase);
    println!(
        "Fixes:   {} of {} used",
        state.iteration, state.max_iterations
    );
    println!(
        "Started: {}",
        state.created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!(
        "Updated: {}",
        state.updated_at.format("%Y-%m-%d %H:%M:%S UTC")
    );

    if let Some(worktree) = &state.worktree {
        println!("Branch:  {}", worktree.branch);
        println!("Workdir: {}", worktree.path.display());
    } else {
        println!("Workdir: {}", state.workdir.display());
    }

    if let Some(commit) = &state.commit {
        println!("Commit:  {}", commit.describe());
    }

    if let Some(verdict) = &state.verdict {
        println!("\nVerdict: {:?}", verdict.verdict);
        if let Some(summary) = &verdict.summary {
            println!("  {summary}");
        }
        for issue in &verdict.issues {
            println!("  - {} {}", issue.id, issue.description);
        }
    }

    if let Some(error) = &state.error {
        println!("\nError:\n  {error}");
    }

    println!("\nArtifacts ({})", artifacts.dir.display());
    for (label, path) in [
        ("REQUEST.md", artifacts.request()),
        ("PLAN.md", artifacts.plan()),
        ("EXECUTION.md", artifacts.execution()),
        ("TEST_RESULTS.md", artifacts.test_results()),
        ("REVIEW.md", artifacts.review()),
        ("VERDICT.json", artifacts.verdict()),
    ] {
        let mark = if path.is_file() {
            "\u{2713}"
        } else {
            "\u{25cb}"
        };
        println!("  {mark} {label}");
    }

    if !state.history.is_empty() {
        println!("\nHistory");
        for event in &state.history {
            println!(
                "  {}  {:<10} {}",
                event.at.format("%H:%M:%S"),
                event.phase.as_str(),
                event.message
            );
        }
    }

    println!("\n{}", next_step(state));
    Ok(())
}

/// What the user should do now.
///
/// A phase name alone puts the burden of knowing the state machine on the reader; a run that
/// stopped is only useful if the next move is obvious.
fn next_step(state: &RunState) -> String {
    match state.phase {
        Phase::Completed => format!("Completed. {}", workflow::outcome_hint(state)),
        Phase::Failed => format!(
            "Failed. Inspect the logs in the run directory, then retry with:  kage resume {}\n{}",
            state.id,
            workflow::outcome_hint(state)
        ),
        Phase::Blocked => format!(
            "Blocked — this needs a human decision. Read REVIEW.md, then either fix the plan and \
             run `kage resume {}`, or start a new run with a clearer task.\n{}",
            state.id,
            workflow::outcome_hint(state)
        ),
        _ => format!(
            "Interrupted while `{}`. Continue with:  kage resume {}",
            state.phase, state.id
        ),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    let single_line = text.replace('\n', " ");
    if single_line.chars().count() <= limit {
        return single_line;
    }
    single_line.chars().take(limit - 1).collect::<String>() + "\u{2026}"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state(phase: Phase) -> RunState {
        let mut state = RunState::new(
            "run_20260809_001".to_string(),
            "task".to_string(),
            PathBuf::from("."),
            3,
        );
        state.phase = phase;
        state
    }

    #[test]
    fn every_stopped_phase_suggests_a_concrete_next_command() {
        assert!(next_step(&state(Phase::Planning)).contains("kage resume run_20260809_001"));
        assert!(next_step(&state(Phase::Failed)).contains("kage resume"));
        assert!(next_step(&state(Phase::Blocked)).contains("human"));
        assert!(next_step(&state(Phase::Completed)).contains("Completed"));
    }

    #[test]
    fn every_terminal_phase_reports_where_the_work_went() {
        // `state()` builds a run with no worktree, so the outcome hint resolves to the
        // "made in place" case; the point is that Failed and Blocked must carry it too, not that
        // this fixture exercises every commitment variant.
        for phase in [Phase::Failed, Phase::Blocked, Phase::Completed] {
            let next = next_step(&state(phase));
            assert!(
                next.contains("changes were made in place"),
                "phase {phase:?} omits the outcome hint:\n{next}"
            );
        }
    }

    #[test]
    fn long_multiline_tasks_collapse_to_one_line() {
        let out = truncate("a task\nwith newlines and quite a lot of words in it", 20);

        assert!(!out.contains('\n'));
        assert_eq!(out.chars().count(), 20);
    }

    #[test]
    fn short_tasks_are_left_alone() {
        assert_eq!(truncate("short", 20), "short");
    }
}
