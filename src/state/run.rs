//! The persisted shape of a single Kage run.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::engine::gates::Verdict;

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

/// Isolation details, recorded so `kage status` can tell the user where the code actually landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// Everything needed to describe, resume, or audit a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub task: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub phase: Phase,
    /// Completed fix attempts. Starts at 0; the first review failure pushes it to 1.
    pub iteration: usize,
    pub max_iterations: usize,
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
            workdir,
            worktree: None,
            base_commit: None,
            verdict: None,
            error: None,
            resume_from: None,
            history: Vec::new(),
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

    /// Fix attempts still available before the loop gives up.
    pub fn remaining_iterations(&self) -> usize {
        self.max_iterations.saturating_sub(self.iteration)
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
}
