//! Subagent orchestration — parallel vs sequential execution.
//!
//! Trigger: `parse_partitions` from PLAN.md (Deferred Tasks or explicit Partition:).
//! Enrich with codegraph, check disjoint, then either:
//! - `tokio::join_all` in parallel (disjoint → safe)
//! - sequential fallback (overlap or no codegraph → safe)
//!
//! Single worktree, one `base_commit`, one `git diff` aggregate.
use std::path::Path;

use anyhow::{Context, Result};

use crate::adapters::{AgentAdapter, AgentRequest};
use crate::engine::partition::{Partition, are_disjoint, enrich_with_codegraph, parse_partitions};
use crate::engine::prompts;
use crate::state::Artifacts;

/// Aggregate shards into EXECUTION.md with headers + partition map.
pub fn aggregate_shards(artifacts: &Artifacts, partitions: &[Partition]) -> Result<()> {
    let mut out = String::new();
    out.push_str("# Partition Map\n\n");
    out.push_str("| Subagent | Files |\n|---|---|\n");
    for p in partitions {
        let files = if p.files.is_empty() {
            "(none)".to_string()
        } else {
            p.files
                .iter()
                .map(|f| f.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!("| {} | {files} |\n", p.id));
    }
    out.push('\n');
    for p in partitions {
        let shard = artifacts.subagent_dir(&p.id).join("EXECUTION.md");
        let content = std::fs::read_to_string(&shard).unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("## Subagent {}\n\n", p.id));
        out.push_str(content.trim());
        out.push_str("\n\n");
    }
    std::fs::write(artifacts.execution(), out).context("cannot write aggregated EXECUTION.md")?;
    Ok(())
}

/// Try to parse partitions from PLAN.md. Returns None if no trigger.
pub fn try_parse_partitions(artifacts: &Artifacts, workdir: &Path) -> Option<Vec<Partition>> {
    let plan_text = artifacts.read_or_placeholder(&artifacts.plan());
    let mut partitions = parse_partitions(&plan_text)?;
    if partitions.len() < 2 {
        return None;
    }
    partitions = enrich_with_codegraph(workdir, partitions);
    Some(partitions)
}

/// Check if partitions are disjoint and should run in parallel.
pub fn should_parallelize(partitions: &[Partition]) -> bool {
    are_disjoint(partitions)
}

/// Run subagents in parallel via `tokio::join_all` + executor per subagent.
///
/// Each subagent is an `AgentRequest`/`AgentResult` via the executor adapter.
/// No new `Phase` — inside `EXECUTE`. Parent wall-clock `timeout_secs` bounds `join_all`.
pub async fn run_parallel(
    workdir: &Path,
    artifacts: &Artifacts,
    partitions: &[Partition],
    executor: &dyn AgentAdapter,
    brief: prompts::Brief<'_>,
) -> Result<Vec<crate::adapters::AgentResult>> {
    let delivery = prompts::Delivery::from_adapter(executor.writes_own_artifacts());
    let futures: Vec<_> = partitions
        .iter()
        .map(|p| {
            let prompt = prompts::subagent(workdir, artifacts, p, brief, delivery);
            let label = format!("subagent-{}", p.id);
            let log_path = artifacts
                .subagent_dir(&p.id)
                .join("logs")
                .join(format!("{label}.log"));
            let prompt_file = artifacts.prompts_dir().join(format!("{label}.md"));
            let workdir = workdir.to_path_buf();
            let shard_path = artifacts.subagent_dir(&p.id).join("EXECUTION.md");
            async move {
                let _ = std::fs::create_dir_all(log_path.parent().unwrap());
                let _ = std::fs::File::create(&log_path);
                let result = executor
                    .run(AgentRequest {
                        prompt,
                        prompt_file,
                        workdir,
                        log_path: log_path.clone(),
                        label: label.clone(),
                    })
                    .await;
                if let Ok(ref r) = result
                    && delivery == prompts::Delivery::KageWrites
                    && !r.stdout.trim().is_empty()
                {
                    let _ = std::fs::create_dir_all(shard_path.parent().unwrap());
                    let _ = std::fs::write(&shard_path, &r.stdout);
                }
                result
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;
    let mut out = Vec::new();
    for r in results {
        out.push(r?);
    }
    Ok(out)
}

/// Run subagents sequentially (fallback when not disjoint).
#[allow(dead_code)]
pub async fn run_sequential(
    workdir: &Path,
    artifacts: &Artifacts,
    partitions: &[Partition],
    executor: &dyn AgentAdapter,
    brief: prompts::Brief<'_>,
) -> Result<Vec<crate::adapters::AgentResult>> {
    let delivery = prompts::Delivery::from_adapter(executor.writes_own_artifacts());
    let mut out = Vec::new();
    for p in partitions {
        let prompt = prompts::subagent(workdir, artifacts, p, brief, delivery);
        let label = format!("subagent-{}", p.id);
        let log_path = artifacts
            .subagent_dir(&p.id)
            .join("logs")
            .join(format!("{label}.log"));
        let prompt_file = artifacts.prompts_dir().join(format!("{label}.md"));
        let shard_path = artifacts.subagent_dir(&p.id).join("EXECUTION.md");
        let _ = std::fs::create_dir_all(log_path.parent().unwrap());
        let _ = std::fs::File::create(&log_path);
        let result = executor
            .run(AgentRequest {
                prompt,
                prompt_file,
                workdir: workdir.to_path_buf(),
                log_path: log_path.clone(),
                label: label.clone(),
            })
            .await?;
        if delivery == prompts::Delivery::KageWrites && !result.stdout.trim().is_empty() {
            let _ = std::fs::create_dir_all(shard_path.parent().unwrap());
            let _ = std::fs::write(&shard_path, &result.stdout);
        }
        out.push(result);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn disjoint_partitions_trigger_parallel() {
        let auth = Partition {
            id: "auth".to_string(),
            task: "auth".to_string(),
            files: vec![PathBuf::from("src/a.rs")],
        };
        let health = Partition {
            id: "health".to_string(),
            task: "health".to_string(),
            files: vec![PathBuf::from("src/b.rs")],
        };
        assert!(should_parallelize(&[auth, health]));
    }

    #[test]
    fn overlapping_partitions_fallback_sequential() {
        let a = Partition {
            id: "a".to_string(),
            task: "a".to_string(),
            files: vec![PathBuf::from("src/shared.rs")],
        };
        let b = Partition {
            id: "b".to_string(),
            task: "b".to_string(),
            files: vec![PathBuf::from("src/shared.rs")],
        };
        assert!(!should_parallelize(&[a, b]));
    }

    #[test]
    fn aggregate_shards_writes_headers_and_map() {
        let root = std::env::temp_dir().join(format!("kage-orch-agg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();
        let project = crate::config::Project::discover(&root).unwrap();
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();
        let partitions = vec![
            Partition {
                id: "auth".to_string(),
                task: "auth".to_string(),
                files: vec![PathBuf::from("src/a.rs")],
            },
            Partition {
                id: "health".to_string(),
                task: "health".to_string(),
                files: vec![PathBuf::from("src/b.rs")],
            },
        ];
        for (id, content) in [("auth", "did auth"), ("health", "did health")] {
            let dir = artifacts.subagent_dir(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("EXECUTION.md"), content).unwrap();
        }
        aggregate_shards(&artifacts, &partitions).unwrap();
        let agg = std::fs::read_to_string(artifacts.execution()).unwrap();
        assert!(agg.contains("# Partition Map"));
        assert!(agg.contains("## Subagent auth"));
        assert!(agg.contains("did auth"));
        assert!(agg.contains("## Subagent health"));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[tokio::test]
    async fn join_all_with_mocked_adapters() {
        // Mock adapter that writes a shard and returns success
        #[allow(dead_code)]
        struct MockAdapter {
            id: String,
        }
        #[async_trait::async_trait]
        impl AgentAdapter for MockAdapter {
            async fn run(&self, req: AgentRequest) -> anyhow::Result<crate::adapters::AgentResult> {
                // Simulate work
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let _ = req.prompt.len();
                Ok(crate::adapters::AgentResult {
                    code: Some(0),
                    stdout: format!("done by {}", self.id),
                    stderr: String::new(),
                    timed_out: false,
                    stalled: false,
                    duration_secs: 0,
                })
            }
            fn describe(&self) -> String {
                format!("mock-{}", self.id)
            }
            fn writes_own_artifacts(&self) -> bool {
                false
            }
        }

        let root = std::env::temp_dir().join(format!("kage-orch-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();
        let project = crate::config::Project::discover(&root).unwrap();
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();
        std::fs::write(artifacts.plan(), "# Objective\nx\n").unwrap();

        let partitions = vec![
            Partition {
                id: "a".to_string(),
                task: "task a".to_string(),
                files: vec![PathBuf::from("src/a.rs")],
            },
            Partition {
                id: "b".to_string(),
                task: "task b".to_string(),
                files: vec![PathBuf::from("src/b.rs")],
            },
        ];

        // Use a single mock for parallel — but run_parallel takes &dyn AgentAdapter (one adapter for all)
        // So we test the sequential path with a mock that handles both
        struct SharedMock;
        #[async_trait::async_trait]
        impl AgentAdapter for SharedMock {
            async fn run(&self, req: AgentRequest) -> anyhow::Result<crate::adapters::AgentResult> {
                Ok(crate::adapters::AgentResult {
                    code: Some(0),
                    stdout: format!("done: {}", req.label),
                    stderr: String::new(),
                    timed_out: false,
                    stalled: false,
                    duration_secs: 0,
                })
            }
            fn describe(&self) -> String {
                "shared-mock".to_string()
            }
            fn writes_own_artifacts(&self) -> bool {
                false
            }
        }
        let mock = SharedMock;
        let brief = prompts::Brief::Plan;
        let results = run_parallel(&root, &artifacts, &partitions, &mock, brief)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.code == Some(0)));

        // Check shards were written
        for p in &partitions {
            let shard = artifacts.subagent_dir(&p.id).join("EXECUTION.md");
            assert!(shard.exists(), "shard for {} missing", p.id);
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
