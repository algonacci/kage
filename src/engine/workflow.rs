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

use crate::adapters::{self, AgentAdapter, AgentRequest, Role, preflight};
use crate::config::{Config, Project};
use crate::engine::{gates, prompts, runner};
use crate::git;
use crate::state::{Artifacts, Commitment, Phase, RunState, store};

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
    // Before anything is allocated on disk: a run that cannot reach one of its harnesses must cost
    // nothing, and must not leave a half-run behind for the user to clean up.
    preflight::check(config)?;

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
    // A resume spends no writes on a run that cannot reach its harnesses: the state file must stay
    // exactly as it was, so the run stays resumable once the harness is installed.
    preflight::check(config)?;

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
    // A previous attempt's commit record must not be reported as this attempt's result.
    state.commit = None;
    state.transition(state.phase, format!("resumed at `{}`", state.phase));
    store::save(project, &state)?;

    drive(project, config, state).await
}

/// Drive the run, then put its work somewhere it can survive the worktree being deleted.
///
/// The state machine below has ten ways out; the commit must happen under all of them, so it lives
/// on this single exit point rather than at each `fail`, `block`, or `break`. A run that ends in an
/// error instead of a phase (`?` out of `run_phases`) is not terminal, stays resumable, and never
/// commits.
async fn drive(project: &Project, config: &Config, state: RunState) -> Result<RunState> {
    let mut state = run_phases(project, config, state).await?;

    if state.phase.is_terminal() {
        finalize(&mut state).await;
        store::save(project, &state)?;
    }

    Ok(state)
}

/// Commit the agent's work to the run's branch, and record what happened.
///
/// Never returns an error: a bookkeeping failure must not change a run's outcome. Every failure
/// path becomes `Commitment::Failed`, which the closing summary prints and `kage clean` consults.
async fn finalize(state: &mut RunState) {
    let Some(worktree) = state.worktree.clone() else {
        return; // Not isolated: committing would write to the branch the user has checked out.
    };

    let commitment = if let Some(base) = &state.base_commit {
        if !worktree.path.exists() {
            Commitment::Failed {
                reason: "the worktree directory is gone".to_string(),
            }
        } else {
            let (subject, body) = commit_message(state);
            match crate::git::commit::commit_work(&worktree.path, base, &subject, &body).await {
                Ok(committed) => committed,
                Err(error) => Commitment::Failed {
                    reason: format!("{error:#}"),
                },
            }
        }
    } else {
        Commitment::Failed {
            reason: "the run recorded no base commit".to_string(),
        }
    };

    println!("  {}", commitment.describe());
    state.commit = Some(commitment);
}

/// The commit message for a run's work: a subject git can display, and a body that says where the
/// work came from.
///
/// The task reaches git as an argv element and Windows caps a command line at roughly 32k
/// characters, so the task is truncated — a bound on the line, not a sanitisation.
fn commit_message(state: &RunState) -> (String, String) {
    let summary = state.task.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary = if summary.chars().count() > 50 {
        let mut truncated = summary.chars().take(47).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        summary
    };
    let subject = format!("kage {}: {summary}", state.id);

    let task = state.task.chars().take(2000).collect::<String>();
    let verdict = match &state.verdict {
        Some(verdict) => match verdict.verdict {
            gates::VerdictKind::Pass => "PASS".to_string(),
            gates::VerdictKind::Fail => "FAIL".to_string(),
            gates::VerdictKind::Blocked => "BLOCKED".to_string(),
        },
        None => "none — the run did not reach review".to_string(),
    };
    let body = format!(
        "Task: {task}\nPhase: {}\nVerdict: {verdict}\n\n\
         Committed by Kage. Kage's own .kage/ directory is excluded from this commit.",
        state.phase.as_str()
    );

    (subject, body)
}

/// The state machine.
///
/// Each phase persists its own state *before* doing any work, so a crash mid-phase resumes by
/// re-entering that phase rather than skipping past it. Re-entry is safe because every phase
/// overwrites its artifact instead of appending.
async fn run_phases(project: &Project, config: &Config, mut state: RunState) -> Result<RunState> {
    let artifacts = Artifacts::for_run(project, &state.id, &state.workdir);
    artifacts.ensure_dirs()?;

    let planner = adapters::build(Role::Planner, config)?;
    let executor = adapters::build(Role::Executor, config)?;
    let reviewer = adapters::build(Role::Reviewer, config)?;

    while !state.phase.is_terminal() {
        store::save(project, &state)?;
        artifacts.sync()?;

        match state.phase {
            Phase::Created => {
                state.transition(Phase::Planning, "starting");
            }

            Phase::Planning => {
                banner(&state, "PLAN", &planner.describe());
                let delivery = prompts::Delivery::from_adapter(planner.writes_own_artifacts());
                let prompt = prompts::planner(&state.task, &state.workdir, &artifacts, delivery);

                let result = run_agent(&*planner, &state, &artifacts, "planner", prompt).await?;
                if let Some(reason) = agent_failure(&result, "planner") {
                    return fail(project, state, reason);
                }
                if let Err(error) = save_reply(delivery, &result, &artifacts.plan()) {
                    return fail(project, state, format!("{error:#}"));
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
                let delivery = prompts::Delivery::from_adapter(executor.writes_own_artifacts());
                let prompt = prompts::executor(&state.workdir, &artifacts, delivery);

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

                let delivery = prompts::Delivery::from_adapter(reviewer.writes_own_artifacts());
                let prompt = prompts::reviewer(&state.workdir, &artifacts, &diff, delivery);
                let result = run_agent(&*reviewer, &state, &artifacts, "reviewer", prompt).await?;
                if let Some(reason) = agent_failure(&result, "reviewer") {
                    return fail(project, state, reason);
                }
                if let Err(error) = save_reply(delivery, &result, &artifacts.review()) {
                    return fail(project, state, format!("{error:#}"));
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

/// Put an API-backed role's reply on disk where the next phase expects to find it.
///
/// A spawned harness has already written the file itself, so this does nothing for it. For a role
/// that only returns text, this is the step that makes the two kinds of backend interchangeable to
/// everything downstream — the phase checks, the next prompt, and `kage status` all keep reading
/// files and never learn which kind produced them.
///
/// The reviewer's `VERDICT.json` is deliberately not written here: `gates::read` already falls back
/// to finding the verdict in the returned text, which is the same recovery a chatty CLI reviewer
/// gets.
fn save_reply(
    delivery: prompts::Delivery,
    result: &crate::adapters::AgentResult,
    path: &std::path::Path,
) -> Result<()> {
    if delivery == prompts::Delivery::AgentWrites {
        return Ok(());
    }

    if result.stdout.trim().is_empty() {
        anyhow::bail!(
            "the model returned nothing to save as {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(path, &result.stdout).with_context(|| format!("cannot write {}", path.display()))
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
    let log_path = artifacts.logs_dir().join(format!("{label}.log"));
    // Create the file before announcing it (REV-002): the path is printed as an invitation to
    // tail it, and a `tail -f` issued the moment it appears must not race the spawn that would
    // otherwise create the file a beat later.
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !log_path.exists() {
        let _ = std::fs::File::create(&log_path);
    }
    // Point the user at the live copy inside the worktree — the one worth tailing — not the
    // mirror, which is only written when the phase ends.
    println!("  log: {}", log_path.display());

    adapter
        .run(AgentRequest {
            prompt,
            prompt_file: artifacts.prompts_dir().join(format!("{label}.md")),
            workdir: state.workdir.clone(),
            log_path,
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
    let base = state.base_commit.as_deref().unwrap_or("HEAD");
    match &state.worktree {
        None => format!("changes were made in place at {}", state.workdir.display()),
        Some(worktree) => match &state.commit {
            Some(Commitment::Committed {
                sha,
                branch,
                files_changed,
                ..
            }) => format!(
                "{} file(s) committed as {} on branch `{branch}`\n  \
                 review them with:  git diff {base} {branch}\n  \
                 take them with:    git merge {branch}",
                files_changed,
                sha.chars().take(12).collect::<String>(),
            ),
            Some(Commitment::NothingToCommit { branch }) => {
                format!("no changes were made — branch `{branch}` is unchanged")
            }
            Some(Commitment::Failed { reason }) => format!(
                "the work could NOT be committed to `{}`: {reason}\n  \
                 it exists only in {} — copy or commit it before `kage clean` removes that directory",
                worktree.branch,
                worktree.path.display(),
            ),
            None => format!(
                "changes are in {} on branch `{}`, uncommitted\n  \
                 review them with:  git diff {base}",
                worktree.path.display(),
                worktree.branch,
            ),
        },
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
    fn the_outcome_hint_names_the_commit_and_how_to_take_it() {
        let mut state = state(3);
        state.base_commit = Some("abc123".to_string());
        state.worktree = Some(crate::state::Worktree {
            path: PathBuf::from("/tmp/wt"),
            branch: "kage/run_1".to_string(),
        });
        state.commit = Some(crate::state::Commitment::Committed {
            sha: "0123456789abcdef".to_string(),
            branch: "kage/run_1".to_string(),
            files_changed: 2,
            created: true,
        });

        let hint = outcome_hint(&state);

        assert!(hint.contains("0123456789ab"), "{hint}");
        assert!(hint.contains("git diff abc123"), "{hint}");
        assert!(hint.contains("git merge kage/run_1"), "{hint}");
    }

    #[test]
    fn a_run_that_changed_nothing_does_not_offer_a_merge() {
        let mut state = state(3);
        state.base_commit = Some("abc123".to_string());
        state.worktree = Some(crate::state::Worktree {
            path: PathBuf::from("/tmp/wt"),
            branch: "kage/run_1".to_string(),
        });
        state.commit = Some(crate::state::Commitment::NothingToCommit {
            branch: "kage/run_1".to_string(),
        });

        let hint = outcome_hint(&state);

        assert!(hint.contains("no changes were made"), "{hint}");
        assert!(!hint.contains("git merge"), "{hint}");
    }

    #[test]
    fn a_failed_commit_tells_the_user_where_the_work_still_is() {
        let mut state = state(3);
        state.worktree = Some(crate::state::Worktree {
            path: PathBuf::from("/tmp/wt"),
            branch: "kage/run_1".to_string(),
        });
        state.commit = Some(crate::state::Commitment::Failed {
            reason: "git refused".to_string(),
        });

        let hint = outcome_hint(&state);

        assert!(hint.contains("/tmp/wt"), "{hint}");
        assert!(hint.contains("git refused"), "{hint}");
        assert!(hint.contains("kage clean"), "{hint}");
    }

    #[test]
    fn a_run_without_a_worktree_still_reports_changes_in_place() {
        let state = state(3);

        let hint = outcome_hint(&state);

        assert!(hint.contains("changes were made in place"), "{hint}");
    }

    #[test]
    fn the_commit_subject_is_one_short_line() {
        let mut state = state(3);
        state.task = format!("line one\nline two\n{}\n", "filler ".repeat(90));
        state.verdict = Some(gates::Verdict {
            verdict: gates::VerdictKind::Pass,
            severity: None,
            summary: None,
            issues: Vec::new(),
        });

        let (subject, body) = commit_message(&state);

        assert!(!subject.contains('\n'), "{subject}");
        let summary = subject
            .strip_prefix(&format!("kage {}: ", state.id))
            .unwrap();
        assert!(
            summary.chars().count() <= 50,
            "summary too long ({summary})"
        );
        assert!(
            body.contains(&format!("Phase: {}", state.phase.as_str())),
            "{body}"
        );
        assert!(body.contains("Verdict: PASS"), "{body}");
    }

    #[test]
    fn the_commit_subject_survives_a_multibyte_task() {
        let mut state = state(3);
        state.task = "é".repeat(300);

        let (subject, _body) = commit_message(&state);

        assert!(!subject.contains('\n'), "{subject}");
        assert!(subject.contains("..."), "{subject}");
    }

    /// Point a role at an arbitrary program (never spawned in these tests).
    fn role_named(program: &str) -> crate::config::RoleConfig {
        let mut role = crate::config::RoleConfig::preset(crate::config::AdapterKind::Command);
        role.command = Some(vec![program.to_string()]);
        role
    }

    /// A fresh project whose planner and executor point at this test binary (guaranteed to exist)
    /// and whose reviewer names a program that cannot be found.
    fn project_with_a_missing_reviewer(
        label: &str,
    ) -> (std::path::PathBuf, Project, crate::config::Config) {
        let root =
            std::env::temp_dir().join(format!("kage-workflow-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();

        let project = Project::discover(&root).unwrap();
        let mut config = project.load_config().unwrap();
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        config.roles.planner = role_named(&exe);
        config.roles.executor = role_named(&exe);
        config.roles.reviewer = role_named("kage-definitely-not-installed");

        (root, project, config)
    }

    #[tokio::test]
    async fn a_missing_harness_aborts_before_the_run_directory_exists() {
        let (root, project, config) = project_with_a_missing_reviewer("aborts");

        let error = start(&project, &config, "task", &Options::default())
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("Not ready"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_dir(project.runs_dir()).unwrap().count(),
            0,
            "a blocked run must leave no run directory behind"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_missing_harness_leaves_a_resumable_run_untouched() {
        let (root, project, config) = project_with_a_missing_reviewer("resume");

        let mut state = crate::state::RunState::new(
            "run_20260809_001".to_string(),
            "task".to_string(),
            root.clone(),
            3,
        );
        state.transition(crate::state::Phase::Executing, "started");
        store::save(&project, &state).unwrap();
        let history_len = state.history.len();
        assert_eq!(history_len, 1);

        let error = resume(&project, &config, state).await.unwrap_err();

        assert!(
            error.to_string().contains("Not ready"),
            "unexpected error: {error}"
        );

        let reloaded = store::load(&project, "run_20260809_001").unwrap();
        assert_eq!(reloaded.phase, crate::state::Phase::Executing);
        assert!(reloaded.error.is_none(), "no error may be recorded");
        assert_eq!(
            reloaded.history.len(),
            history_len,
            "a blocked resume must record nothing"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
