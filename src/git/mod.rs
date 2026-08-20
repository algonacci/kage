//! Git operations, performed by invoking the installed `git` binary.
//!
//! Shelling out rather than linking a git library keeps the MVP small and, more importantly,
//! debuggable: every operation Kage performs is a command the user can paste into their own
//! terminal to see exactly what happened.

pub mod commit;
pub mod diff;
pub mod worktree;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::adapters::proc::{self, Spawn};

/// Git commands are local and fast; anything slower than this is a hang, not slow work.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Run a git command in `workdir` and return its stdout, failing on a non-zero exit.
pub async fn git(workdir: &Path, args: &[&str]) -> Result<String> {
    let outcome = proc::run(Spawn {
        program: "git".to_string(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        workdir: workdir.to_path_buf(),
        env: Vec::new(),
        stdin: None,
        raw_command: None,
        timeout: GIT_TIMEOUT,
        stream_prefix: None,
        stdout_format: crate::adapters::stream::OutputFormat::Passthrough,
        heartbeat: None,
        // Git is local and fast; the short timeout above already catches a hang.
        stall: None,
        log_path: None,
        progress_path: None,
    })
    .await
    .context("cannot run git")?;

    if !outcome.success() {
        bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            outcome.describe(),
            outcome.failure_output().trim()
        );
    }

    Ok(outcome.stdout)
}

/// Whether `path` is inside a git working tree.
pub async fn is_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

/// The commit `HEAD` currently points at.
pub async fn head_commit(workdir: &Path) -> Result<String> {
    Ok(git(workdir, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string())
}

/// Every local branch name, in the short form a run id can be recovered from.
///
/// Branches are a namespace Kage allocates into (`kage/<run_id>`), and a branch outlives both its
/// checkout and `.kage/runs/` — `kage clean` keeps it deliberately. That makes this the only record
/// of an earlier run that survives its run directory being deleted, which is why id allocation
/// consults it. `--format=%(refname:short)` matters: the full `refs/heads/kage/…` form would match
/// no prefix the caller looks for.
pub async fn branch_names(workdir: &Path) -> Result<Vec<String>> {
    Ok(git(
        workdir,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .await?
    .lines()
    .map(str::trim)
    .filter(|name| !name.is_empty())
    .map(str::to_string)
    .collect())
}

/// Whether the repository has any commit yet.
///
/// A fresh `git init` has no HEAD, and worktrees cannot branch from nothing — worth detecting up
/// front so the failure names the real cause.
pub async fn has_commits(workdir: &Path) -> bool {
    git(workdir, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn branch_names_come_back_in_the_form_a_run_id_can_be_read_from() {
        // Run id allocation matches these against `kage/`, so the full `refs/heads/kage/…` form
        // would silently match nothing and let a fresh run reuse an existing run's branch — the
        // failure this listing exists to prevent, restored by a formatting detail.
        let root = std::env::temp_dir().join(format!("kage-branches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        git(&root, &["init", "-b", "main"]).await.unwrap();
        git(&root, &["config", "user.email", "kage@test.local"])
            .await
            .unwrap();
        git(&root, &["config", "user.name", "Kage Test"])
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "original").unwrap();
        git(&root, &["add", "."]).await.unwrap();
        git(&root, &["commit", "-m", "commit"]).await.unwrap();
        git(&root, &["branch", "kage/run_20260809_007"])
            .await
            .unwrap();

        let names = branch_names(&root).await.unwrap();

        assert!(
            names.iter().any(|name| name == "kage/run_20260809_007"),
            "expected the short form, got {names:?}"
        );
        assert!(names.iter().any(|name| name == "main"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
