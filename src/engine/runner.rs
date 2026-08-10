//! Running the validation commands that decide whether execution actually worked.
//!
//! These run in Kage's process, not inside an agent. An agent reporting "all tests pass" is a
//! claim; an exit code is evidence, and the whole design rests on being strict about results even
//! while staying flexible about approach.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::adapters::proc;
use crate::config::{Setup, Validation};

/// The result of one validation command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub command: String,
    pub passed: bool,
    pub summary: String,
    pub output: String,
}

/// The result of a full validation pass.
#[derive(Debug, Clone)]
pub struct TestReport {
    pub results: Vec<CommandResult>,
    /// True when nothing was configured — an absence of evidence, not evidence of success.
    pub skipped: bool,
}

impl TestReport {
    /// Whether every configured command succeeded.
    ///
    /// An empty configuration passes: Kage cannot invent a test suite, and blocking every run in a
    /// repository without one would make the tool unusable there. The report says loudly that
    /// nothing was verified so the reviewer weighs the code itself more heavily.
    pub fn passed(&self) -> bool {
        self.results.iter().all(|result| result.passed)
    }

    pub fn failed(&self) -> impl Iterator<Item = &CommandResult> {
        self.results.iter().filter(|result| !result.passed)
    }

    /// The TEST_RESULTS.md artifact.
    pub fn to_markdown(&self) -> String {
        if self.skipped {
            return "# Test Results\n\n\
                    **No validation commands are configured**, so nothing was verified \
                    mechanically. Treat any claim that the code works as unproven and judge the \
                    implementation on its own merits.\n\n\
                    Add commands under `validation.commands` in `.kage/config.yaml` to enable \
                    automated checking.\n"
                .to_string();
        }

        let mut out = String::from("# Test Results\n\n");
        out.push_str(if self.passed() {
            "**All validation commands passed.**\n\n"
        } else {
            "**Validation failed.**\n\n"
        });

        for result in &self.results {
            let mark = if result.passed { "PASS" } else { "FAIL" };
            out.push_str(&format!(
                "## `{}` — {mark} ({})\n\n",
                result.command, result.summary
            ));

            // Passing commands contribute nothing but noise to the next prompt; failing ones are
            // the entire reason the fixer exists, so they keep their output.
            if !result.passed {
                out.push_str("```\n");
                out.push_str(tail(&result.output, 200).trim_end());
                out.push_str("\n```\n\n");
            }
        }

        out
    }
}

/// Prepare a freshly created worktree so its validation gate can run at all.
///
/// A worktree holds only tracked files, so a project that keeps its dependencies outside the
/// repository arrives with none of them. `npm test` there fails for reasons that have nothing to do
/// with the change under test, and it would fail that way on every phase, spending the repair
/// budget on an empty `node_modules`.
///
/// Failure aborts the run before any agent is spawned: preparation that did not work leaves a gate
/// that cannot judge anything, and every phase after it would be measuring the environment.
pub async fn prepare(workdir: &Path, setup: &Setup) -> Result<()> {
    if setup.commands.is_empty() {
        return Ok(());
    }

    let timeout = Duration::from_secs(setup.timeout_secs);

    for command in &setup.commands {
        println!("  setup $ {command}");

        let mut spawn = proc::shell_spawn(command, workdir.to_path_buf(), timeout);
        spawn.heartbeat = Some(proc::HEARTBEAT_INTERVAL);
        let outcome = proc::run(spawn).await?;

        println!("    {}", outcome.describe());

        if !outcome.success() {
            bail!(
                "the setup command `{command}` failed ({})\n{}\n\
                 the worktree is not usable, so nothing was spawned — fix the command in \
                 `setup.commands` or run it by hand in the project first",
                outcome.describe(),
                tail(outcome.failure_output().trim(), 20)
            );
        }
    }

    Ok(())
}

/// Run every configured validation command in order, stopping at the first failure.
///
/// Stopping early is deliberate: if the build is broken, the test and lint runs after it will fail
/// for the same reason, and three copies of one root cause in the fix prompt buries the signal.
pub async fn validate(workdir: &Path, validation: &Validation) -> Result<TestReport> {
    if validation.commands.is_empty() {
        return Ok(TestReport {
            results: Vec::new(),
            skipped: true,
        });
    }

    let timeout = Duration::from_secs(validation.timeout_secs);
    let rtk = proc::rtk_available();
    let mut results = Vec::new();

    for command in &validation.commands {
        // Record what actually ran, not what was configured — a compressed log is confusing if the
        // header claims a command that was never executed.
        let executed = if proc::route_through_rtk(command, rtk) {
            format!("rtk {}", command.trim())
        } else {
            command.clone()
        };

        println!("  $ {executed}");

        // A 15-minute `cargo test` is the same blank terminal as a thinking planner; the heartbeat
        // is what tells a working suite from a hung one. Output still goes to TEST_RESULTS.md.
        let mut spawn = proc::shell_spawn(&executed, workdir.to_path_buf(), timeout);
        spawn.heartbeat = Some(proc::HEARTBEAT_INTERVAL);
        let outcome = proc::run(spawn).await?;
        let passed = outcome.success();

        println!("    {}", outcome.describe());

        results.push(CommandResult {
            command: executed,
            passed,
            summary: outcome.describe(),
            // stdout and stderr both matter: test harnesses split failure detail between them
            // inconsistently, and losing either can hide the actual error.
            output: format!("{}\n{}", outcome.stdout, outcome.stderr),
        });

        if !passed {
            break;
        }
    }

    Ok(TestReport {
        results,
        skipped: false,
    })
}

/// The last `lines` lines of `text`.
///
/// Compiler and test output puts the summary at the end, and a full `cargo test` log would crowd
/// out the plan in the fix prompt.
fn tail(text: &str, lines: usize) -> String {
    let collected: Vec<&str> = text.lines().collect();
    if collected.len() <= lines {
        return text.to_string();
    }

    format!(
        "[... {} earlier lines omitted ...]\n{}",
        collected.len() - lines,
        collected[collected.len() - lines..].join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validation(commands: &[&str]) -> Validation {
        Validation {
            commands: commands.iter().map(|c| c.to_string()).collect(),
            timeout_secs: 60,
        }
    }

    fn setup(commands: &[&str]) -> Setup {
        Setup {
            commands: commands.iter().map(|c| c.to_string()).collect(),
            timeout_secs: 60,
        }
    }

    #[tokio::test]
    async fn a_project_with_no_setup_needs_none() {
        assert!(prepare(&std::env::temp_dir(), &setup(&[])).await.is_ok());
    }

    #[tokio::test]
    async fn setup_commands_run_in_order_and_a_failure_stops_the_run() {
        // The bug this guards: a worktree is a clean checkout, so a JS project arrives with no
        // node_modules and `npm test` fails on every phase for reasons the executor did not cause.
        assert!(
            prepare(
                &std::env::temp_dir(),
                &setup(&["echo first", "echo second"])
            )
            .await
            .is_ok()
        );

        let error = prepare(&std::env::temp_dir(), &setup(&["exit 3", "echo never"]))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("exit 3"), "{error}");
        assert!(
            error.contains("nothing was spawned"),
            "the message must say no agent was paid for: {error}"
        );
        assert!(error.contains("setup.commands"), "and name the way out");
    }

    #[tokio::test]
    async fn all_commands_run_when_they_pass() {
        let report = validate(
            &std::env::temp_dir(),
            &validation(&["echo first", "echo second"]),
        )
        .await
        .unwrap();

        assert!(report.passed());
        assert_eq!(report.results.len(), 2);
        assert!(!report.skipped);
    }

    #[tokio::test]
    async fn a_failure_stops_the_remaining_commands() {
        let report = validate(
            &std::env::temp_dir(),
            &validation(&["exit 1", "echo never-runs"]),
        )
        .await
        .unwrap();

        assert!(!report.passed());
        assert_eq!(report.results.len(), 1, "later commands must not run");
        assert_eq!(report.failed().count(), 1);
    }

    #[tokio::test]
    async fn no_configured_commands_is_reported_as_unverified() {
        let report = validate(&std::env::temp_dir(), &validation(&[]))
            .await
            .unwrap();

        assert!(report.skipped);
        assert!(report.passed(), "an empty suite must not block the loop");

        let markdown = report.to_markdown();
        assert!(markdown.contains("No validation commands are configured"));
        assert!(markdown.contains("unproven"));
    }

    #[tokio::test]
    async fn failing_output_reaches_the_artifact_and_passing_output_does_not() {
        let report = validate(
            &std::env::temp_dir(),
            &validation(&["echo quiet-success", "echo loud-failure && exit 2"]),
        )
        .await
        .unwrap();

        let markdown = report.to_markdown();

        assert!(markdown.contains("Validation failed"));
        // Not anchored on a leading backtick: the recorded command carries an `rtk ` prefix on
        // machines that have RTK installed, and this test must pass either way.
        assert!(markdown.contains("echo quiet-success"));
        assert!(markdown.contains("loud-failure"));
        // Exactly one fenced block: the failing command's. The passing one contributes no output.
        assert_eq!(markdown.matches("```").count(), 2, "{markdown}");
    }

    #[test]
    fn tail_keeps_the_end_where_the_error_summary_lives() {
        let text: String = (1..=500).map(|n| format!("line {n}\n")).collect();

        let out = tail(&text, 10);

        assert!(out.contains("line 500"));
        assert!(!out.contains("line 100\n"));
        assert!(out.contains("490 earlier lines omitted"));
    }

    #[test]
    fn tail_leaves_short_output_alone() {
        assert_eq!(tail("a\nb\n", 10), "a\nb\n");
    }
}
