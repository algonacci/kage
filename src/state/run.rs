//! The persisted shape of a single Kage run.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::engine::gates::Verdict;
use crate::state::subagent::SubagentState;

/// Where a run is in the workflow.
///
/// The phase is written to disk *before* the work of that phase begins, so a run interrupted by a
/// crash resumes by re-entering the phase it was in rather than skipping it. Re-running a phase is
/// safe: every phase overwrites its artifact rather than appending to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Created,
    Planning,
    Executing,
    Testing,
    Reviewing,
    Fixing,
    Completed,
    Failed,
    Blocked,
}

impl Phase {
    /// Whether the run has stopped for good. Terminal runs are not resumable.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Blocked)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Planning => "planning",
            Self::Executing => "executing",
            Self::Testing => "testing",
            Self::Reviewing => "reviewing",
            Self::Fixing => "fixing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry in a run's audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: DateTime<Utc>,
    pub phase: Phase,
    pub message: String,
}

/// What sent the run into the FIX phase.
///
/// Recorded because the two causes spend different budgets and brief the fixer differently: a
/// validation failure is mechanical and must not re-present the previous review's findings as if
/// they were the reason — those were already addressed, and the exit code is the whole story.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixCause {
    /// The validation commands failed: caught by an exit code, charged to the repair budget.
    Validation,
    /// The reviewer rejected the work: a premium judgment, charged to the iteration budget.
    Review,
}

/// Isolation details, recorded so `kage status` can tell the user where the code actually landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// What became of the agent's work when the run stopped.
///
/// A run's branch is the only thing that outlives its worktree, so what did or did not reach that
/// branch is the single most important fact in the closing summary — and the fact `kage clean` has
/// to consult before it force-removes a checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Commitment {
    /// The work is on the branch. `created` is false when the agent had already committed it
    /// itself and Kage found nothing left to stage.
    Committed {
        sha: String,
        branch: String,
        files_changed: usize,
        created: bool,
    },
    /// The run finished without changing a single file outside `.kage/`.
    NothingToCommit { branch: String },
    /// Git refused. The work exists only in the worktree, and removing that directory destroys it.
    Failed { reason: String },
}

impl Commitment {
    /// One line, for `kage status`.
    pub fn describe(&self) -> String {
        match self {
            Self::Committed {
                sha,
                branch,
                files_changed,
                created: true,
            } => format!(
                "{} on `{branch}` ({} file(s))",
                short_sha(sha),
                files_changed
            ),
            Self::Committed {
                sha,
                branch,
                files_changed,
                created: false,
            } => format!(
                "{} on `{branch}` ({} file(s), committed by the agent)",
                short_sha(sha),
                files_changed
            ),
            Self::NothingToCommit { branch } => {
                format!("nothing to commit — `{branch}` is unchanged")
            }
            Self::Failed { reason } => format!("not committed: {reason}"),
        }
    }
}

/// The first twelve characters of a commit sha, so a short or unusual sha cannot panic on a byte
/// slice.
fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect::<String>()
}

/// Everything needed to describe, resume, or audit a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub task: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub phase: Phase,
    /// Completed review-rejection fix attempts. Starts at 0; the first review failure pushes it
    /// to 1. Validation failures never touch this — they spend `repairs`.
    pub iteration: usize,
    pub max_iterations: usize,
    /// Repair attempts spent making validation pass in the current fix cycle.
    ///
    /// Reset when a review rejection opens a new cycle: each review's findings are a fresh
    /// implementation job with the same right to a compiling result. Kept apart from `iteration`
    /// because a broken build and a review finding are different failures — sharing one budget let
    /// three failed builds end a run before the reviewer had seen the code at all.
    #[serde(default)]
    pub repairs: usize,
    /// Absent from state files written before the split budget; those runs resume with the
    /// documented default rather than a repair budget of zero, which would fail them on their
    /// first broken build.
    #[serde(default = "default_max_repairs")]
    pub max_repairs: usize,
    /// Why the run is in `Fixing`, when it is. Absent on state files that predate the split
    /// budget; the workflow then falls back to inferring the cause from whether a verdict exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_cause: Option<FixCause>,
    /// Whether the run started at EXECUTE with the task itself as the executor's instruction.
    ///
    /// Recorded rather than inferred from a missing `PLAN.md`: a plan that was written and then
    /// lost with its worktree is not the same thing as a run that never asked for one, and neither
    /// `kage status` nor the closing summary may present one as the other.
    #[serde(default)]
    pub skip_plan: bool,
    /// Whether the validation gate was already failing before this run touched anything.
    ///
    /// Recorded so the reviewer is not asked to judge a change against failures that predate it,
    /// and so `kage status` can say why a run's first TEST phase failed on work it never did.
    #[serde(default)]
    pub gate_was_red: bool,
    /// Directory the agents actually run in — the worktree when isolating, else the project root.
    pub workdir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    /// Commit the run started from. Everything diffed against it is the agents' work, which is what
    /// the reviewer judges and what a resumed run must keep diffing against — recomputing it later
    /// from HEAD would silently exclude changes an agent had already committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// What Kage did with the working tree when the run stopped. `None` means no commit was
    /// attempted: the run is still going, it ran without isolation, or its state file predates
    /// commit-on-finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<Commitment>,
    /// Why a run ended in `Failed` or `Blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The phase a failed or blocked run was in when it stopped.
    ///
    /// `Failed` and `Blocked` are terminal, so without remembering the phase underneath them a
    /// resumed run has nowhere to go — the loop sees a terminal state and exits having done
    /// nothing, which is not what `kage status` promises when it suggests resuming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<Phase>,
    #[serde(default)]
    pub history: Vec<Event>,
    // ponytail: additive field — None when no subagents, no new Phase variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<SubagentState>>,
}

/// The repair budget a state file gets when it predates the field, and `new` starts from before
/// `start` copies the config's value in.
fn default_max_repairs() -> usize {
    3
}

impl RunState {
    pub fn new(id: String, task: String, workdir: PathBuf, max_iterations: usize) -> Self {
        let now = Utc::now();
        Self {
            id,
            task,
            created_at: now,
            updated_at: now,
            phase: Phase::Created,
            iteration: 0,
            max_iterations,
            repairs: 0,
            max_repairs: default_max_repairs(),
            fix_cause: None,
            skip_plan: false,
            gate_was_red: false,
            workdir,
            worktree: None,
            base_commit: None,
            verdict: None,
            commit: None,
            error: None,
            resume_from: None,
            history: Vec::new(),
            subagents: None,
        }
    }

    /// Move to `phase` and record why. Callers persist afterwards; this only mutates memory.
    pub fn transition(&mut self, phase: Phase, message: impl Into<String>) {
        self.phase = phase;
        self.updated_at = Utc::now();
        self.history.push(Event {
            at: self.updated_at,
            phase,
            message: message.into(),
        });
    }

    /// Review-rejection fix attempts still available before the loop gives up.
    pub fn remaining_iterations(&self) -> usize {
        self.max_iterations.saturating_sub(self.iteration)
    }

    /// Repair attempts still available in the current fix cycle.
    pub fn remaining_repairs(&self) -> usize {
        self.max_repairs.saturating_sub(self.repairs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transition_is_recorded_in_history() {
        let mut state = RunState::new(
            "run_20260809_001".to_string(),
            "add caching".to_string(),
            PathBuf::from("."),
            3,
        );

        state.transition(Phase::Planning, "planner starting");

        assert_eq!(state.phase, Phase::Planning);
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].message, "planner starting");
    }

    #[test]
    fn remaining_iterations_saturates_instead_of_underflowing() {
        let mut state = RunState::new("r".to_string(), "t".to_string(), PathBuf::from("."), 1);
        state.iteration = 5;

        assert_eq!(state.remaining_iterations(), 0);
    }

    #[test]
    fn only_the_end_states_are_terminal() {
        assert!(Phase::Completed.is_terminal());
        assert!(Phase::Failed.is_terminal());
        assert!(Phase::Blocked.is_terminal());
        assert!(!Phase::Fixing.is_terminal());
        assert!(!Phase::Created.is_terminal());
    }

    #[test]
    fn a_state_file_written_before_commit_tracking_still_loads() {
        let json = r#"{
            "id": "run_1",
            "task": "add caching",
            "created_at": "2026-08-09T00:00:00Z",
            "updated_at": "2026-08-09T00:00:00Z",
            "phase": "completed",
            "iteration": 0,
            "max_iterations": 3,
            "workdir": "."
        }"#;

        let state: RunState = serde_json::from_str(json).unwrap();

        assert_eq!(state.id, "run_1");
        assert_eq!(state.phase, Phase::Completed);
        assert!(
            state.commit.is_none(),
            "older state files carry no commit record"
        );
    }

    #[test]
    fn a_state_file_written_before_skippable_planning_still_loads() {
        // A state file predating the `--skip-plan` flag has no `skip_plan` key and must still
        // plan: `#[serde(default)]` yields `false`, so an old run is never retroactively marked
        // plan-free the way an absent PLAN.md must not be either.
        let json = r#"{ "id": "run_1",
            "task": "add caching",
            "created_at": "2026-08-09T00:00:00Z",
            "updated_at": "2026-08-09T00:00:00Z",
            "phase": "completed",
            "iteration": 0,
            "max_iterations": 3,
            "workdir": "." }"#;

        let state: RunState = serde_json::from_str(json).unwrap();

        assert!(!state.skip_plan, "a predating state file must still plan");
    }

    #[test]
    fn a_state_file_written_before_the_split_budget_still_loads_with_repairs_to_spend() {
        // A mid-flight run saved before `repairs`/`max_repairs` existed must resume with the
        // documented default budget, not zero — zero would fail it on its first broken build,
        // which is a harsher rule than the one it started under.
        let json = r#"{ "id": "run_1",
            "task": "add caching",
            "created_at": "2026-08-09T00:00:00Z",
            "updated_at": "2026-08-09T00:00:00Z",
            "phase": "fixing",
            "iteration": 1,
            "max_iterations": 3,
            "workdir": "." }"#;

        let state: RunState = serde_json::from_str(json).unwrap();

        assert_eq!(state.repairs, 0);
        assert_eq!(state.max_repairs, 3);
        assert!(
            state.fix_cause.is_none(),
            "an old run's cause is unknown and must be inferred, not invented"
        );
    }

    #[test]
    fn each_commitment_describes_itself_for_status() {
        assert_eq!(
            Commitment::Committed {
                sha: "0123456789abcdef".to_string(),
                branch: "kage/run_1".to_string(),
                files_changed: 3,
                created: true,
            }
            .describe(),
            "0123456789ab on `kage/run_1` (3 file(s))"
        );

        assert_eq!(
            Commitment::Committed {
                sha: "0123456789abcdef".to_string(),
                branch: "kage/run_1".to_string(),
                files_changed: 1,
                created: false,
            }
            .describe(),
            "0123456789ab on `kage/run_1` (1 file(s), committed by the agent)"
        );

        assert_eq!(
            Commitment::NothingToCommit {
                branch: "kage/run_1".to_string()
            }
            .describe(),
            "nothing to commit — `kage/run_1` is unchanged"
        );

        assert_eq!(
            Commitment::Failed {
                reason: "git died".to_string()
            }
            .describe(),
            "not committed: git died"
        );
    }

    #[test]
    fn a_state_file_written_before_subagents_still_loads() {
        let json = r#"{
            "id": "run_1",
            "task": "add caching",
            "created_at": "2026-08-09T00:00:00Z",
            "updated_at": "2026-08-09T00:00:00Z",
            "phase": "completed",
            "iteration": 0,
            "max_iterations": 3,
            "workdir": "."
        }"#;
        let state: RunState = serde_json::from_str(json).unwrap();
        assert!(state.subagents.is_none());
    }

    #[test]
    fn run_state_with_subagents_round_trips() {
        let mut state = RunState::new("run_1".to_string(), "t".to_string(), PathBuf::from("."), 3);
        state.subagents = Some(vec![crate::state::SubagentState {
            id: "auth".to_string(),
            task: "add auth".to_string(),
            files: vec![PathBuf::from("src/auth.rs")],
            status: crate::state::SubagentStatus::Completed,
            cost_usd: Some(0.12),
        }]);
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("subagents"));
        let back: RunState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.subagents.as_ref().unwrap().len(), 1);
        assert_eq!(back.subagents.as_ref().unwrap()[0].id, "auth");
    }

    #[test]
    fn run_state_without_subagents_omits_field() {
        let state = RunState::new("run_1".to_string(), "t".to_string(), PathBuf::from("."), 3);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("subagents"));
    }
}
