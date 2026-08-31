//! Partitioning for subagent execution.
//!
//! Trigger: planner's `# Deferred Tasks` presence plus `codegraph impact --depth 2`
//! disjoint file sets determines partitions; explicit `Partition:` escape hatch
//! in PLAN.md is also honored. Disjoint → parallel via `tokio::join_all`,
//! overlap → fallback sequential (single executor). Single worktree, one
//! `base_commit`, one `git diff` aggregate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One partition of an oversized task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub id: String,
    pub task: String,
    pub files: Vec<PathBuf>,
}

/// Parse partitions from PLAN.md.
///
/// Checks in order:
/// 1. Explicit `Partition:` escape hatch — lines like
///    `Partition: auth | Add auth middleware | src/adapters/api.rs, src/adapters/mod.rs`
///    or `Partition: auth: Add auth middleware -- src/adapters/api.rs`
/// 2. `# Deferred Tasks` section — each bullet becomes a partition with task text,
///    files inferred later via codegraph (empty until enriched).
pub fn parse_partitions(plan: &str) -> Option<Vec<Partition>> {
    if let Some(explicit) = parse_explicit_partitions(plan)
        && !explicit.is_empty()
    {
        return Some(explicit);
    }
    parse_deferred_partitions(plan)
}

fn parse_explicit_partitions(plan: &str) -> Option<Vec<Partition>> {
    let mut partitions = Vec::new();
    for line in plan.lines() {
        let trimmed = line.trim();
        // Match `Partition:` case-insensitive, with optional leading `-` or `*` (list item)
        let rest = if let Some(r) = trimmed.strip_prefix('-') {
            r.trim()
        } else if let Some(r) = trimmed.strip_prefix('*') {
            r.trim()
        } else {
            trimmed
        };
        let Some(after) = rest
            .strip_prefix("Partition:")
            .or_else(|| rest.strip_prefix("partition:"))
            .or_else(|| rest.strip_prefix("PARTITION:"))
        else {
            continue;
        };
        let after = after.trim();
        if after.is_empty() {
            continue;
        }
        // Format: `id | task | file1, file2`  or  `id: task -- file1, file2`  or  `id task`
        // Try `|` separator first
        let parts: Vec<&str> = after.split('|').map(|s| s.trim()).collect();
        let (id, task, files_str) = match parts.len() {
            3 => (parts[0].to_string(), parts[1].to_string(), parts[2]),
            2 => (parts[0].to_string(), parts[1].to_string(), ""),
            1 => {
                // Try `:` or `--` or single token
                let s = parts[0];
                if let Some(idx) = s.find(':') {
                    let id = s[..idx].trim().to_string();
                    let rest = s[idx + 1..].trim();
                    // Check for `--` files separator
                    if let Some(dash_idx) = rest.find("--") {
                        let task = rest[..dash_idx].trim().to_string();
                        let files = rest[dash_idx + 2..].trim();
                        (id, task, files)
                    } else {
                        (id, rest.to_string(), "")
                    }
                } else if let Some(idx) = s.find("--") {
                    let task = s[..idx].trim().to_string();
                    let files = s[idx + 2..].trim();
                    // No id, use slug of task
                    let id = slug(&task);
                    (id, task, files)
                } else {
                    // Single task, id is slug
                    let id = slug(s);
                    (id, s.to_string(), "")
                }
            }
            _ => continue,
        };
        if id.is_empty() || task.is_empty() {
            continue;
        }
        let files = if files_str.is_empty() {
            Vec::new()
        } else {
            files_str
                .split(',')
                .map(|f| f.trim())
                .filter(|f| !f.is_empty())
                .map(PathBuf::from)
                .collect()
        };
        partitions.push(Partition { id, task, files });
    }
    if partitions.is_empty() {
        None
    } else {
        Some(partitions)
    }
}

fn parse_deferred_partitions(plan: &str) -> Option<Vec<Partition>> {
    let deferred = deferred_tasks(plan)?;
    let mut partitions = Vec::new();
    for (idx, line) in deferred.lines().enumerate() {
        let task = line.trim().trim_start_matches(['-', '*']).trim();
        if task.is_empty() {
            continue;
        }
        // Skip empty or heading-like lines
        if task.starts_with('#') {
            continue;
        }
        let id = slug(task);
        let id = if id.is_empty() {
            format!("part-{}", idx + 1)
        } else {
            // Ensure unique ids
            let base = id;
            let mut candidate = base.clone();
            let mut counter = 1;
            while partitions.iter().any(|p: &Partition| p.id == candidate) {
                counter += 1;
                candidate = format!("{base}-{counter}");
            }
            candidate
        };
        partitions.push(Partition {
            id,
            task: task.to_string(),
            files: Vec::new(),
        });
    }
    if partitions.is_empty() {
        None
    } else {
        Some(partitions)
    }
}

/// Extract `# Deferred Tasks` section (same logic as workflow::deferred_tasks).
fn deferred_tasks(plan: &str) -> Option<String> {
    let is_deferred_heading = |line: &str| {
        let trimmed = line.trim();
        trimmed.starts_with('#')
            && trimmed
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case("deferred tasks")
    };
    let mut collected = Vec::new();
    let mut in_section = false;
    for line in plan.lines() {
        if in_section {
            if line.trim_start().starts_with('#') {
                break;
            }
            collected.push(line);
        } else if is_deferred_heading(line) {
            in_section = true;
        }
    }
    if !in_section {
        return None;
    }
    let body = collected.join("\n").trim().to_string();
    (!body.is_empty()).then_some(body)
}

fn slug(task: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in task.chars().take(30) {
        if ch.is_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
        if s.len() >= 20 {
            break;
        }
    }
    s.trim_matches('-').to_string()
}

/// Check if partitions have disjoint file sets.
///
/// Empty file sets are treated as unknown → not disjoint (fallback sequential).
/// This is the safe default: without file info we cannot prove disjointness.
pub fn are_disjoint(partitions: &[Partition]) -> bool {
    if partitions.len() < 2 {
        return false;
    }
    // If any partition has empty files, we cannot validate disjointness
    if partitions.iter().any(|p| p.files.is_empty()) {
        return false;
    }
    let mut seen = HashSet::new();
    for p in partitions {
        for f in &p.files {
            if !seen.insert(f) {
                return false;
            }
        }
    }
    true
}

/// Check disjointness of raw file sets (for testing without Partition).
#[allow(dead_code)]
pub fn file_sets_disjoint(sets: &[Vec<PathBuf>]) -> bool {
    if sets.len() < 2 {
        return false;
    }
    let mut seen = HashSet::new();
    for set in sets {
        for f in set {
            if !seen.insert(f) {
                return false;
            }
        }
    }
    true
}

/// Enrich partitions with codegraph file sets when files are empty.
///
/// For each partition with empty files, try `codegraph impact --depth 2` via
/// the task text as symbol query. If codegraph unavailable, leaves files empty
/// (which will cause `are_disjoint` to return false → sequential fallback).
pub fn enrich_with_codegraph(workdir: &Path, mut partitions: Vec<Partition>) -> Vec<Partition> {
    for p in &mut partitions {
        if !p.files.is_empty() {
            continue;
        }
        if let Some(files) = crate::engine::codegraph::impact_files_for_task(workdir, &p.task) {
            p.files = files.into_iter().map(PathBuf::from).collect();
        }
    }
    partitions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_partition_with_pipe_separator() {
        let plan = "# Objective\nx\n\nPartition: auth | Add auth middleware | src/adapters/api.rs, src/adapters/mod.rs\nPartition: health | Add health check | src/cli/doctor.rs\n";
        let parts = parse_partitions(plan).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].id, "auth");
        assert_eq!(parts[0].task, "Add auth middleware");
        assert_eq!(
            parts[0].files,
            vec![
                PathBuf::from("src/adapters/api.rs"),
                PathBuf::from("src/adapters/mod.rs")
            ]
        );
        assert_eq!(parts[1].id, "health");
        assert_eq!(parts[1].files, vec![PathBuf::from("src/cli/doctor.rs")]);
    }

    #[test]
    fn explicit_partition_case_insensitive() {
        let plan = "partition: auth | task | src/a.rs\n";
        let parts = parse_partitions(plan).unwrap();
        assert_eq!(parts[0].id, "auth");
    }

    #[test]
    fn deferred_tasks_become_partitions() {
        let plan = "# Objective\nShip A.\n\n# Deferred Tasks\n\n- add auth middleware\n- add health check\n\n# Done\n";
        let parts = parse_partitions(plan).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].task.contains("auth"));
        assert!(parts[1].task.contains("health"));
        // Files empty until enriched
        assert!(parts[0].files.is_empty());
    }

    #[test]
    fn no_partition_when_no_trigger() {
        let plan = "# Objective\nShip it all.\n";
        assert_eq!(parse_partitions(plan), None);
    }

    #[test]
    fn explicit_partition_takes_precedence_over_deferred() {
        let plan = "# Objective\nx\n\n# Deferred Tasks\n- task a\n- task b\n\nPartition: auth | explicit | src/a.rs\n";
        let parts = parse_partitions(plan).unwrap();
        // Explicit should win
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].id, "auth");
    }

    #[test]
    fn auth_vs_health_disjoint() {
        let auth = Partition {
            id: "auth".to_string(),
            task: "auth".to_string(),
            files: vec![
                PathBuf::from("src/adapters/api.rs"),
                PathBuf::from("src/adapters/mod.rs"),
            ],
        };
        let health = Partition {
            id: "health".to_string(),
            task: "health".to_string(),
            files: vec![
                PathBuf::from("src/cli/doctor.rs"),
                PathBuf::from("src/engine/runner.rs"),
            ],
        };
        assert!(are_disjoint(&[auth, health]));
    }

    #[test]
    fn overlapping_files_not_disjoint() {
        let a = Partition {
            id: "a".to_string(),
            task: "a".to_string(),
            files: vec![
                PathBuf::from("src/engine/workflow.rs"),
                PathBuf::from("src/adapters/api.rs"),
            ],
        };
        let b = Partition {
            id: "b".to_string(),
            task: "b".to_string(),
            files: vec![
                PathBuf::from("src/engine/workflow.rs"),
                PathBuf::from("src/cli/doctor.rs"),
            ],
        };
        assert!(!are_disjoint(&[a, b]));
    }

    #[test]
    fn empty_files_not_disjoint_fallback_sequential() {
        let a = Partition {
            id: "a".to_string(),
            task: "a".to_string(),
            files: vec![],
        };
        let b = Partition {
            id: "b".to_string(),
            task: "b".to_string(),
            files: vec![PathBuf::from("src/a.rs")],
        };
        assert!(!are_disjoint(&[a, b]));
    }

    #[test]
    fn file_sets_disjoint_helper() {
        let sets = vec![
            vec![PathBuf::from("src/a.rs")],
            vec![PathBuf::from("src/b.rs")],
        ];
        assert!(file_sets_disjoint(&sets));
        let overlapping = vec![
            vec![PathBuf::from("src/a.rs")],
            vec![PathBuf::from("src/a.rs")],
        ];
        assert!(!file_sets_disjoint(&overlapping));
    }

    #[test]
    fn single_partition_not_disjoint() {
        let a = Partition {
            id: "a".to_string(),
            task: "a".to_string(),
            files: vec![PathBuf::from("src/a.rs")],
        };
        assert!(!are_disjoint(&[a]));
    }
}
