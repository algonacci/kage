//! Subagent state — one shard of a partitioned execution.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One Kage-spawned child handling a disjoint file partition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentState {
    pub id: String,
    pub task: String,
    pub files: Vec<PathBuf>,
    pub status: SubagentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

// ponytail: simple enum, no abstraction — four variants cover the lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Pending,
    Running,
    Completed,
    Failed(String),
}

impl std::fmt::Display for SubagentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("pending"),
            Self::Running => f.write_str("running"),
            Self::Completed => f.write_str("completed"),
            Self::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_json() {
        let cases = vec![
            SubagentStatus::Pending,
            SubagentStatus::Running,
            SubagentStatus::Completed,
            SubagentStatus::Failed("timeout".to_string()),
        ];
        for status in cases {
            let json = serde_json::to_string(&status).unwrap();
            let back: SubagentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn subagent_state_round_trips_with_cost() {
        let state = SubagentState {
            id: "auth".to_string(),
            task: "add auth".to_string(),
            files: vec![PathBuf::from("src/adapters/api.rs")],
            status: SubagentStatus::Completed,
            cost_usd: Some(0.42),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: SubagentState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn subagent_state_omits_none_cost() {
        let state = SubagentState {
            id: "health".to_string(),
            task: "add health".to_string(),
            files: vec![],
            status: SubagentStatus::Pending,
            cost_usd: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("cost_usd"));
        let back: SubagentState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cost_usd, None);
    }
}
