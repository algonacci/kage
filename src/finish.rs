//! Handing finished work over: the owner's own commands, run once a branch is ready for them.
//!
//! A run ends at a branch on purpose — pushing and opening a pull request need credentials Kage
//! never holds. `finish.commands` closes that last step with the owner's tools. Like a
//! notification, a hand-over that fails is a printed warning and never touches the exit code: the
//! work is committed, and a push that did not happen is one command away.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::adapters::proc;
use crate::config::{Finish, Project};
use crate::state::{Artifacts, Commitment, Phase, RunState};

/// The file the summary is written to, in the run directory of the run being handed over.
pub const SUMMARY_FILE: &str = "HANDOVER.md";

/// Everything the commands are told about the work they hand over.
#[derive(Debug, Clone)]
pub struct HandOver {
    pub title: String,
    pub branch: String,
    /// The run whose branch is handed over — for a queued session, its last ticket's.
    pub run_id: String,
    pub run_dir: PathBuf,
    /// Markdown for a pull request body: what was asked and what each run says it did.
    pub summary: String,
}

/// The branch a completed run committed its work to, or `None` when there is nothing to hand
/// over — a run that changed nothing would open an empty pull request, and one whose commit failed
/// has no branch holding the work.
pub fn ready_branch(state: &RunState) -> Option<&str> {
    if state.phase != Phase::Completed {
        return None;
    }
    match &state.commit {
        Some(Commitment::Committed { branch, .. }) => Some(branch),
        _ => None,
    }
}

/// A hand-over for one run on its own.
pub fn for_run(project: &Project, state: &RunState) -> Option<HandOver> {
    let branch = ready_branch(state)?.to_string();
    let title = title_of(&state.task);
    let summary = summary(&title, &[section(project, &title, state)]);
    Some(HandOver {
        title,
        branch,
        run_id: state.id.clone(),
        run_dir: project.run_dir(&state.id),
        summary,
    })
}

/// A hand-over for a queued session: the last ticket's branch holds every ticket stacked under it,
/// so one pull request carries the whole session and its body accounts for each ticket.
pub fn for_session(
    project: &Project,
    title: String,
    tickets: &[(String, RunState)],
) -> Option<HandOver> {
    let (_, tip) = tickets.last()?;
    // The last ticket may have changed nothing — its plan was already satisfied by the tickets
    // under it — yet its branch still holds every earlier ticket's commits. The session is handed
    // over when any ticket committed work, from the branch the last one ran on.
    if tip.phase != Phase::Completed
        || !tickets
            .iter()
            .any(|(_, state)| ready_branch(state).is_some())
    {
        return None;
    }
    let branch = match &tip.commit {
        Some(Commitment::Committed { branch, .. } | Commitment::NothingToCommit { branch }) => {
            branch.clone()
        }
        _ => return None,
    };
    let sections: Vec<String> = tickets
        .iter()
        .map(|(name, state)| section(project, name, state))
        .collect();
    Some(HandOver {
        summary: summary(&title, &sections),
        title,
        branch,
        run_id: tip.id.clone(),
        run_dir: project.run_dir(&tip.id),
    })
}

/// The first line of a task, without its heading marks — what a pull request is titled.
pub fn title_of(task: &str) -> String {
    let line = task
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("Kage run");
    let mut title: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        title.push('…');
    }
    title
}

fn section(project: &Project, heading: &str, state: &RunState) -> String {
    let account = std::fs::read_to_string(Artifacts::new(project, &state.id).execution())
        .ok()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| "_The executor left no account._".to_string());
    format!(
        "## {heading}\n\nRun `{}`.\n\n{}\n",
        state.id,
        account.trim()
    )
}

fn summary(title: &str, sections: &[String]) -> String {
    format!(
        "# {title}\n\n{}\n---\n\nPlanned, implemented, validated, and reviewed by Kage.\n",
        sections.join("\n")
    )
}

/// The environment the commands see.
pub fn environment(
    project_name: &str,
    hand_over: &HandOver,
    summary_file: &Path,
) -> Vec<(String, String)> {
    vec![
        ("KAGE_BRANCH".to_string(), hand_over.branch.clone()),
        ("KAGE_TITLE".to_string(), hand_over.title.clone()),
        (
            "KAGE_SUMMARY_FILE".to_string(),
            summary_file.to_string_lossy().into_owned(),
        ),
        ("KAGE_RUN_ID".to_string(), hand_over.run_id.clone()),
        (
            "KAGE_RUN_DIR".to_string(),
            hand_over.run_dir.to_string_lossy().into_owned(),
        ),
        ("KAGE_PROJECT".to_string(), project_name.to_string()),
    ]
}

/// Run the commands in the project root, in order, stopping at the first failure.
///
/// Returns the line worth relaying to whoever is told the run finished: the last line the final
/// command printed (`gh pr create` prints the pull request's URL), or what failed. `None` when
/// nothing was configured.
pub async fn hand_over(
    finish: &Finish,
    project: &Project,
    project_name: &str,
    hand_over: &HandOver,
) -> Option<String> {
    if finish.commands.is_empty() {
        return None;
    }

    let summary_file = hand_over.run_dir.join(SUMMARY_FILE);
    if let Err(error) = std::fs::write(&summary_file, &hand_over.summary) {
        println!("finish: cannot write {} — {error}", summary_file.display());
        return Some("hand-over skipped: its summary could not be written".to_string());
    }
    let summary_file = crate::paths::canonical(&summary_file);
    let env = environment(project_name, hand_over, &summary_file);
    let timeout = Duration::from_secs(finish.timeout_secs);

    println!("\nHanding over {}", hand_over.branch);
    let mut last_line = None;
    for command in &finish.commands {
        println!("  finish $ {command}");
        let mut spawn = proc::shell_spawn(command, project.root.clone(), timeout);
        spawn.env = env.clone();
        spawn.stream_prefix = Some("  [finish]".to_string());
        spawn.heartbeat = Some(proc::HEARTBEAT_INTERVAL);

        let outcome = match proc::run(spawn).await {
            Ok(outcome) => outcome,
            Err(error) => {
                println!("finish: `{command}` could not start — {error:#}");
                return Some(format!("hand-over failed: `{command}` could not start"));
            }
        };
        println!("    {}", outcome.describe());
        if !outcome.success() {
            println!(
                "finish: stopped — the work is committed on {}, so the rest is yours to run",
                hand_over.branch
            );
            return Some(format!(
                "hand-over failed at `{command}` ({})",
                outcome.describe()
            ));
        }
        last_line = last_nonempty_line(&outcome.stdout);
    }

    Some(last_line.unwrap_or_else(|| format!("handed over {}", hand_over.branch)))
}

fn last_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(tag: &str) -> (PathBuf, Project) {
        let root = std::env::temp_dir().join(format!("kage-finish-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();
        let project = Project::discover(&root).unwrap();
        (root, project)
    }

    fn completed(id: &str, task: &str) -> RunState {
        let mut state = RunState::new(id.to_string(), task.to_string(), PathBuf::from("."), 3);
        state.phase = Phase::Completed;
        state.commit = Some(Commitment::Committed {
            sha: "abc".to_string(),
            branch: format!("kage/{id}"),
            files_changed: 1,
            created: true,
        });
        state
    }

    #[test]
    fn only_committed_work_on_a_completed_run_is_handed_over() {
        let mut state = completed("run_1", "task");
        assert_eq!(ready_branch(&state), Some("kage/run_1"));

        state.commit = Some(Commitment::NothingToCommit {
            branch: "kage/run_1".to_string(),
        });
        assert_eq!(ready_branch(&state), None, "an empty pull request is noise");

        let mut failed = completed("run_2", "task");
        failed.phase = Phase::Failed;
        assert_eq!(ready_branch(&failed), None);
    }

    #[test]
    fn a_title_is_the_first_line_without_heading_marks() {
        assert_eq!(
            title_of("\n# T2: Approval flow\n\nbody"),
            "T2: Approval flow"
        );
        assert_eq!(title_of(""), "Kage run");
        assert_eq!(title_of(&"x".repeat(150)).chars().count(), 101);
    }

    #[test]
    fn a_session_hand_over_takes_the_last_branch_and_accounts_for_every_ticket() {
        let (root, project) = project("session");
        let first = completed("run_1", "# T1: one");
        let last = completed("run_2", "# T2: two");
        let execution = Artifacts::new(&project, "run_1").execution();
        std::fs::create_dir_all(execution.parent().unwrap()).unwrap();
        std::fs::write(&execution, "added the table").unwrap();

        let hand_over = for_session(
            &project,
            "Leave requests".to_string(),
            &[
                ("feat-t1.md".to_string(), first),
                ("feat-t2.md".to_string(), last),
            ],
        )
        .unwrap();

        assert_eq!(hand_over.branch, "kage/run_2");
        assert_eq!(hand_over.run_id, "run_2");
        assert!(hand_over.summary.starts_with("# Leave requests"));
        assert!(hand_over.summary.contains("## feat-t1.md"));
        assert!(hand_over.summary.contains("added the table"));
        assert!(hand_over.summary.contains("## feat-t2.md"));
        assert!(hand_over.summary.contains("left no account"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_whose_last_ticket_changed_nothing_is_still_handed_over() {
        // Found driving the queue end to end: the last ticket committed nothing, so no pull
        // request was opened for a session whose branch held the first ticket's work.
        let (root, project) = project("empty-tip");
        let mut last = completed("run_2", "# T2");
        last.commit = Some(Commitment::NothingToCommit {
            branch: "kage/run_2".to_string(),
        });
        let tickets = [
            ("t1".to_string(), completed("run_1", "# T1")),
            ("t2".to_string(), last.clone()),
        ];

        let hand_over = for_session(&project, "S".to_string(), &tickets).unwrap();
        assert_eq!(hand_over.branch, "kage/run_2");

        assert!(
            for_session(&project, "S".to_string(), &[("t2".to_string(), last)]).is_none(),
            "a session that committed nothing has nothing to hand over"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn commands_see_the_facts_as_environment_and_the_last_line_is_relayed() {
        // The title is free text from a task; it reaches the command as a variable, never as part
        // of the command line, so a title holding shell syntax is inert.
        let (root, project) = project("env");
        let mut state = completed("run_1", "# Fix `rm -rf` $(whoami) quoting");
        state.task.push_str("\nbody");
        let hand_over = for_run(&project, &state).unwrap();
        std::fs::create_dir_all(&hand_over.run_dir).unwrap();

        let command = if cfg!(windows) {
            "echo %KAGE_BRANCH% & echo https://example.test/pull/7"
        } else {
            "test -f \"$KAGE_SUMMARY_FILE\" && printf '%s\\n' \"$KAGE_TITLE\" && echo https://example.test/pull/7"
        };
        let finish = Finish {
            commands: vec![command.to_string()],
            timeout_secs: 30,
        };

        let relayed = super::hand_over(&finish, &project, "proj", &hand_over).await;

        assert_eq!(relayed.as_deref(), Some("https://example.test/pull/7"));
        assert!(hand_over.run_dir.join(SUMMARY_FILE).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_failing_command_stops_the_rest_and_says_which() {
        let (root, project) = project("fail");
        let hand_over = for_run(&project, &completed("run_1", "task")).unwrap();
        std::fs::create_dir_all(&hand_over.run_dir).unwrap();
        let marker = root.join("second-ran");
        let finish = Finish {
            commands: vec![
                "exit 3".to_string(),
                format!("echo x > \"{}\"", marker.display()),
            ],
            timeout_secs: 30,
        };

        let relayed = super::hand_over(&finish, &project, "proj", &hand_over)
            .await
            .unwrap();

        assert!(
            relayed.contains("hand-over failed at `exit 3`"),
            "{relayed}"
        );
        assert!(
            !marker.exists(),
            "a push that failed must not be followed by a PR"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn nothing_configured_runs_nothing() {
        let (root, project) = project("none");
        let hand_over = for_run(&project, &completed("run_1", "task")).unwrap();
        let relayed = super::hand_over(&Finish::default(), &project, "proj", &hand_over).await;
        assert_eq!(relayed, None);
        assert!(!hand_over.run_dir.join(SUMMARY_FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
