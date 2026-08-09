//! The v0.1 loop: plan, execute, test, review, fix, verify.
//!
//! ```text
//! TASK -> PLAN -> EXECUTE -> TEST -> REVIEW -> DECISION -> DONE
//!                             ^                    |
//!                             +------- FIX <-------+
//! ```
//!
//! One planner, one executor, one reviewer. No DAG, no parallel agents, no dynamic delegation:
//! those belong to later versions, and none of them are worth building before this line is
//! reliable end to end.

use anyhow::{Context, Result};

use crate::adapters::{self, AgentAdapter, AgentRequest, Role};
use crate::config::{Config, Project};
use crate::engine::{gates, prompts, runner};
use crate::git;
use crate::state::{Artifacts, Phase, RunState, store};

/// Per-invocation overrides from the command line.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub max_iterations: Option<usize>,
    /// Force agents to run in the user's working tree instead of a worktree.
    pub no_isolate: bool,
}

/// Begin a new run and drive it to a terminal state.
pub async fn start(
    project: &Project,
    config: &Config,
    task: &str,
    options: &Options,
) -> Result<RunState> {
    let run_id = project.next_run_id(chrono::Local::now())?;
    let max_iterations = options
        .max_iterations
        .unwrap_or(config.loop_config.max_iterations);

    let isolate = config.git.isolate && !options.no_isolate;
    let in_repo = git::is_repo(&project.root).await;

    let mut state = RunState::new(
        run_id.clone(),
        task.to_string(),
        project.root.clone(),
        max_iterations,
    );

    if isolate && in_repo {
        let worktree = git::worktree::create(
            &project.root,
            &project.worktrees_dir(),
            &run_id,
            config.git.base.as_deref(),
        )
        .await?;

        println!(
            "  isolated in {} ({})",
            worktree.path.display(),
            worktree.branch
        );
        state.workdir = worktree.path.clone();
        state.worktree = Some(worktree);
    } else if isolate && !in_repo {
        // Isolation is the default, so silently running in the user's tree would be a surprise
        // with real consequences.
        println!("  ! not a git repository — agents will edit files in place, without isolation");
    }

    if in_repo {
        state.base_commit = Some(git::head_commit(&state.workdir).await?);
    }

    // Only now is the working directory known, and with it where artifacts have to live.
    let artifacts = Artifacts::for_run(project, &run_id, &state.workdir);
    artifacts.ensure_dirs()?;

    // The request is an artifact like any other, so a run directory explains itself later.
    std::fs::write(
        artifacts.request(),
        format!("# Request\n\n{task}\n\nRun: `{run_id}`\n"),
    )
    .context("cannot write REQUEST.md")?;
    artifacts.sync()?;

    store::save(project, &state)?;
    drive(project, config, state).await
}

/// Continue an interrupted run from wherever its state file says it stopped.
pub async fn resume(project: &Project, config: &Config, mut state: RunState) -> Result<RunState> {
    // A worktree can be removed between runs; recreate it so a resumed run still has its checkout.
    if let Some(worktree) = &state.worktree
        && !worktree.path.exists()
    {
        let restored = git::worktree::create(
            &project.root,
            &project.worktrees_dir(),
            &state.id,
            config.git.base.as_deref(),
        )
        .await?;
        state.workdir = restored.path.clone();
        state.worktree = Some(restored);
    }

    // A failed or blocked run is terminal, so re-entering the phase it died in is the only way
    // resuming can do anything at all.
    if state.phase.is_terminal()
        && let Some(phase) = state.resume_from.take()
    {
        state.phase = phase;
    }

    state.error = None;
    state.transition(state.phase, format!("resumed at `{}`", state.phase));
    store::save(project, &state)?;

    drive(project, config, state).await
}

/// The state machine.
///
/// Each phase persists its own state *before* doing any work, so a crash mid-phase resumes by
/// re-entering that phase rather than skipping past it. Re-entry is safe because every phase
/// overwrites its artifact instead of appending.
async fn drive(project: &Project, config: &Config, mut state: RunState) -> Result<RunState> {
    let artifacts = Artifacts::for_run(project, &state.id, &state.workdir);
    artifacts.ensure_dirs()?;

    let planner = adapters::build(Role::Planner, &config.roles.planner)?;
    let executor = adapters::build(Role::Executor, &config.roles.executor)?;
    let reviewer = adapters::build(Role::Reviewer, &config.roles.reviewer)?;

    while !state.phase.is_terminal() {
        store::save(project, &state)?;
        artifacts.sync()?;

        match state.phase {
            Phase::Created => {
                state.transition(Phase::Planning, "starting");
            }

            Phase::Planning => {
                banner(&state, "PLAN", &planner.describe());
                let prompt = prompts::planner(&state.task, &state.workdir, &artifacts);

                let result = run_agent(&*planner, &state, &artifacts, "planner", prompt).await?;
                if let Some(reason) = agent_failure(&result, "planner") {
                    return fail(project, state, reason);
                }
                if !artifacts.plan().is_file() {
                    return fail(
                        project,
                        state,
                        "the planner exited successfully but wrote no PLAN.md — \
                         check the planner log in the run directory"
                            .to_string(),
                    );
                }

                state.transition(Phase::Executing, "plan written");
            }

            Phase::Executing => {
                banner(&state, "EXECUTE", &executor.describe());
                let prompt = prompts::executor(&state.workdir, &artifacts);

                let result = run_agent(&*executor, &state, &artifacts, "executor", prompt).await?;
                if let Some(reason) = agent_failure(&result, "executor") {
                    return fail(project, state, reason);
                }

                state.transition(Phase::Testing, "implementation finished");
            }

            Phase::Testing => {
                banner(&state, "TEST", "validation commands");
                let report = runner::validate(&state.workdir, &config.validation).await?;
                std::fs::write(artifacts.test_results(), report.to_markdown())
                    .context("cannot write TEST_RESULTS.md")?;

                if report.passed() {
                    state.transition(Phase::Reviewing, "validation passed");
                    continue;
                }

                let failures: Vec<&str> = report
                    .failed()
                    .map(|result| result.command.as_str())
                    .collect();
                let summary = format!("validation failed: {}", failures.join(", "));
                println!("  {summary}");

                // Failing tests skip review entirely. Asking a reviewer to judge code that does not
                // build spends a premium model to learn what an exit code already proved.
                match next_fix_phase(&mut state, &summary) {
                    Some(phase) => state.transition(phase, summary),
                    None => {
                        return fail(project, state, format!("{summary} — no fix attempts left"));
                    }
                }
            }

            Phase::Reviewing => {
                banner(&state, "REVIEW", &reviewer.describe());

                let diff = match &state.base_commit {
                    Some(base) => git::diff::since(&state.workdir, base)
                        .await
                        .unwrap_or_else(|error| format!("_(could not compute diff: {error})_")),
                    None => "_(not a git repository — no diff available)_".to_string(),
                };

                // A stale verdict from the previous iteration must not be mistaken for this one's.
                let _ = std::fs::remove_file(artifacts.verdict());

                let prompt = prompts::reviewer(&state.workdir, &artifacts, &diff);
                let result = run_agent(&*reviewer, &state, &artifacts, "reviewer", prompt).await?;
                if let Some(reason) = agent_failure(&result, "reviewer") {
                    return fail(project, state, reason);
                }

                let Some(verdict) = gates::read(&artifacts.verdict(), &result.stdout) else {
                    // Without a verdict there is no decision to act on, and guessing PASS would let
                    // unreviewed code through. Stop for a human instead.
                    return block(
                        project,
                        state,
                        "the reviewer produced no machine-readable verdict — \
                         expected a VERDICT.json with a PASS, FAIL, or BLOCKED value"
                            .to_string(),
                    );
                };

                println!(
                    "  verdict: {:?}{}",
                    verdict.verdict,
                    verdict
                        .summary
                        .as_ref()
                        .map(|s| format!(" — {s}"))
                        .unwrap_or_default()
                );

                let passed = verdict.passed();
                let blocked = verdict.verdict == gates::VerdictKind::Blocked;
                state.verdict = Some(verdict);

                if passed {
                    if let Some(base) = &state.base_commit
                        && let Ok(stat) = git::diff::stat(&state.workdir, base).await
                        && !stat.is_empty()
                    {
                        println!(
                            "
{stat}"
                        );
                    }
                    state.transition(Phase::Completed, "review passed");
                } else if blocked {
                    return block(
                        project,
                        state,
                        "the reviewer reported the work as blocked — a human needs to look at it"
                            .to_string(),
                    );
                } else {
                    let summary = "review failed".to_string();
                    match next_fix_phase(&mut state, &summary) {
                        Some(phase) => state.transition(phase, summary),
                        None => {
                            return fail(
                                project,
                                state,
                                "review failed and no fix attempts are left".to_string(),
                            );
                        }
                    }
                }
            }

            Phase::Fixing => {
                banner(
                    &state,
                    &format!("FIX {}/{}", state.iteration, state.max_iterations),
                    &executor.describe(),
                );

                // Test failures reach this phase with no verdict, so an empty finding list is
                // normal; the test results in the prompt carry the detail in that case.
                let verdict = state.verdict.clone().unwrap_or(gates::Verdict {
                    verdict: gates::VerdictKind::Fail,
                    severity: None,
                    summary: Some(
                        "The validation commands failed. See the test results below.".to_string(),
                    ),
                    issues: Vec::new(),
                });

                let prompt = prompts::fixer(
                    &state.workdir,
                    &artifacts,
                    &verdict,
                    state.iteration,
                    state.max_iterations,
                );

                let label = format!("fix-{}", state.iteration);
                let result = run_agent(&*executor, &state, &artifacts, &label, prompt).await?;
                if let Some(reason) = agent_failure(&result, "executor") {
                    return fail(project, state, reason);
                }

                state.transition(Phase::Testing, "fix applied");
            }

            Phase::Completed | Phase::Failed | Phase::Blocked => break,
        }
    }

    artifacts.sync()?;
    store::save(project, &state)?;
    Ok(state)
}

/// Spend an iteration on a fix, or report that none remain.
fn next_fix_phase(state: &mut RunState, _reason: &str) -> Option<Phase> {
    if state.remaining_iterations() == 0 {
        return None;
    }
    state.iteration += 1;
    Some(Phase::Fixing)
}

/// Turn a non-zero exit or a timeout into an explanation worth printing.
///
/// A harness that is not installed, not authenticated, or out of quota fails here, and those are by
/// far the most common ways a run dies — so the message names the role and shows what it printed.
fn agent_failure(result: &crate::adapters::AgentResult, role: &str) -> Option<String> {
    if result.success() {
        return None;
    }

    if result.timed_out {
        return Some(format!(
            "the {role} timed out after {}s — raise `timeout_secs` for that role if the task is \
             genuinely long",
            result.duration_secs
        ));
    }

    let detail = if result.stderr.trim().is_empty() {
        result.stdout.trim()
    } else {
        result.stderr.trim()
    };
    let mut lines: Vec<&str> = detail.lines().rev().take(10).collect();
    // `rev` picked the *last* ten lines; reversing back puts them in reading order. Without this a
    // stack trace is printed upside down, which reads as gibberish at exactly the worst moment.
    lines.reverse();
    let tail = lines.join("\n");

    Some(format!(
        "the {role} exited with code {:?}\n{}",
        result.code,
        tail.trim()
    ))
}

async fn run_agent(
    adapter: &dyn AgentAdapter,
    state: &RunState,
    artifacts: &Artifacts,
    label: &str,
    prompt: String,
) -> Result<crate::adapters::AgentResult> {
    adapter
        .run(AgentRequest {
            prompt,
            prompt_file: artifacts.prompts_dir().join(format!("{label}.md")),
            workdir: state.workdir.clone(),
            log_path: artifacts.logs_dir().join(format!("{label}.log")),
            label: label.to_string(),
        })
        .await
}

fn banner(state: &RunState, phase: &str, who: &str) {
    println!("\n[{}] {phase}", state.id);
    println!("  {who}");
}

fn fail(project: &Project, mut state: RunState, reason: String) -> Result<RunState> {
    println!("  failed: {reason}");
    let _ = Artifacts::for_run(project, &state.id, &state.workdir).sync();
    state.error = Some(reason.clone());
    state.resume_from = Some(state.phase);
    state.transition(Phase::Failed, reason);
    store::save(project, &state)?;
    Ok(state)
}

fn block(project: &Project, mut state: RunState, reason: String) -> Result<RunState> {
    println!("  blocked: {reason}");
    let _ = Artifacts::for_run(project, &state.id, &state.workdir).sync();
    state.error = Some(reason.clone());
    state.resume_from = Some(state.phase);
    state.transition(Phase::Blocked, reason);
    store::save(project, &state)?;
    Ok(state)
}

/// Where the finished work ended up, for the closing summary.
pub fn outcome_hint(state: &RunState) -> String {
    match &state.worktree {
        Some(worktree) => format!(
            "changes are on branch `{}` in {}\n  review them with:  git diff {}\n  \
             take them with:    git merge {}",
            worktree.branch,
            worktree.path.display(),
            state.base_commit.as_deref().unwrap_or("HEAD"),
            worktree.branch,
        ),
        None => format!("changes were made in place at {}", state.workdir.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn state(max_iterations: usize) -> RunState {
        RunState::new(
            "run_1".to_string(),
            "task".to_string(),
            PathBuf::from("."),
            max_iterations,
        )
    }

    #[test]
    fn fix_attempts_are_spent_until_the_budget_runs_out() {
        let mut state = state(2);

        assert_eq!(next_fix_phase(&mut state, "x"), Some(Phase::Fixing));
        assert_eq!(state.iteration, 1);
        assert_eq!(next_fix_phase(&mut state, "x"), Some(Phase::Fixing));
        assert_eq!(state.iteration, 2);
        assert_eq!(next_fix_phase(&mut state, "x"), None, "budget exhausted");
        assert_eq!(state.iteration, 2, "a refused attempt must not be counted");
    }

    #[test]
    fn zero_iterations_means_one_shot_with_no_fixes() {
        let mut state = state(0);

        assert_eq!(next_fix_phase(&mut state, "x"), None);
    }

    #[test]
    fn a_timed_out_agent_is_explained_with_a_remedy() {
        let result = crate::adapters::AgentResult {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            duration_secs: 1800,
        };

        let reason = agent_failure(&result, "executor").unwrap();

        assert!(reason.contains("timed out"));
        assert!(reason.contains("timeout_secs"));
    }

    #[test]
    fn a_failing_agent_surfaces_its_own_error_output() {
        let result = crate::adapters::AgentResult {
            code: Some(1),
            stdout: String::new(),
            stderr: "Error: not authenticated. Run `codex login`.".to_string(),
            timed_out: false,
            duration_secs: 2,
        };

        let reason = agent_failure(&result, "reviewer").unwrap();

        assert!(reason.contains("reviewer"));
        assert!(reason.contains("codex login"));
    }

    #[test]
    fn failure_output_keeps_its_original_line_order() {
        let result = crate::adapters::AgentResult {
            code: Some(1),
            stdout: String::new(),
            stderr: "Traceback:\n  line one\n  line two\nFinalError: boom".to_string(),
            timed_out: false,
            duration_secs: 1,
        };

        let reason = agent_failure(&result, "executor").unwrap();
        let traceback = reason.find("Traceback").unwrap();
        let final_error = reason.find("FinalError").unwrap();

        assert!(traceback < final_error, "printed upside down:\n{reason}");
    }

    #[test]
    fn a_successful_agent_produces_no_failure() {
        let result = crate::adapters::AgentResult {
            code: Some(0),
            stdout: "done".to_string(),
            stderr: String::new(),
            timed_out: false,
            duration_secs: 5,
        };

        assert!(agent_failure(&result, "planner").is_none());
    }

    #[test]
    fn the_outcome_hint_names_the_branch_when_isolated() {
        let mut state = state(3);
        state.base_commit = Some("abc123".to_string());
        state.worktree = Some(crate::state::Worktree {
            path: PathBuf::from("/tmp/wt"),
            branch: "kage/run_1".to_string(),
        });

        let hint = outcome_hint(&state);

        assert!(hint.contains("kage/run_1"));
        assert!(hint.contains("git merge kage/run_1"));
        assert!(hint.contains("git diff abc123"));
    }
}
