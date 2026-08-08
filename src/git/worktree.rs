//! Isolating agent work in a throwaway git worktree.
//!
//! Kage hands file-write access to an autonomous executor. Pointing that at the user's working tree
//! means an agent can destroy uncommitted work that exists nowhere else. A worktree gives the agent
//! a real checkout to build and test in while the user's tree stays untouched, and makes the result
//! reviewable as a branch that can be merged, cherry-picked, or thrown away.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;
use crate::state::Worktree;

/// Create a worktree for `run_id`, branching from `base` (or current HEAD).
pub async fn create(
    root: &Path,
    worktrees_dir: &Path,
    run_id: &str,
    base: Option<&str>,
) -> Result<Worktree> {
    if !git::has_commits(root).await {
        bail!(
            "this repository has no commits yet, so Kage cannot create an isolated worktree\n\
             make an initial commit, or set `git.isolate: false` in .kage/config.yaml"
        );
    }

    std::fs::create_dir_all(worktrees_dir)
        .with_context(|| format!("cannot create {}", worktrees_dir.display()))?;

    let path = path_for(worktrees_dir, run_id);
    let branch = format!("kage/{run_id}");

    if path.exists() {
        // A resumed run reuses its worktree; recreating it would throw away the work in progress.
        return Ok(Worktree {
            path: crate::paths::canonical(&path),
            branch,
        });
    }

    let path_arg = path.to_string_lossy().into_owned();

    // The branch can outlive its checkout: `kage clean` keeps branches by design, and a run that
    // died between creating the branch and creating the directory leaves one behind too. Passing
    // `-b` in either case fails with "a branch named ... already exists", so check first and attach
    // to the existing branch instead of demanding a new one.
    let branch_exists = git::git(root, &["rev-parse", "--verify", &branch])
        .await
        .is_ok();

    let mut args = vec!["worktree", "add"];
    if branch_exists {
        args.extend(["--force", &path_arg, &branch]);
    } else {
        args.extend(["-b", &branch, &path_arg]);
        if let Some(base) = base {
            args.push(base);
        }
    }

    git::git(root, &args)
        .await
        .context("cannot create the isolated worktree")?;

    Ok(Worktree {
        path: crate::paths::canonical(&path),
        branch,
    })
}

/// Remove a worktree checkout, keeping its branch.
///
/// The branch is what holds the agent's work, so it outlives the checkout — a user who wants the
/// changes merges `kage/<run_id>` long after the directory is gone.
pub async fn remove(root: &Path, path: &Path) -> Result<()> {
    let path_arg = path.to_string_lossy().into_owned();
    git::git(root, &["worktree", "remove", "--force", &path_arg])
        .await
        .with_context(|| format!("cannot remove worktree at {}", path.display()))?;

    Ok(())
}

/// Where a run's worktree would live.
pub fn path_for(worktrees_dir: &Path, run_id: &str) -> PathBuf {
    worktrees_dir.join(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("kage-wt-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        git::git(&root, &["init", "-b", "main"]).await.unwrap();
        git::git(&root, &["config", "user.email", "kage@test.local"])
            .await
            .unwrap();
        git::git(&root, &["config", "user.name", "Kage Test"])
            .await
            .unwrap();
        root
    }

    async fn commit(root: &Path, name: &str, content: &str) {
        std::fs::write(root.join(name), content).unwrap();
        git::git(root, &["add", "."]).await.unwrap();
        git::git(root, &["commit", "-m", "commit"]).await.unwrap();
    }

    #[tokio::test]
    async fn a_worktree_is_a_real_checkout_on_its_own_branch() {
        let root = repo("create").await;
        commit(&root, "a.txt", "original").await;
        let worktrees = root.join(".kage/worktrees");

        let worktree = create(&root, &worktrees, "run_20260809_001", None)
            .await
            .unwrap();

        assert_eq!(worktree.branch, "kage/run_20260809_001");
        assert_eq!(
            std::fs::read_to_string(worktree.path.join("a.txt")).unwrap(),
            "original"
        );

        // Edits inside the worktree must not touch the user's working tree.
        std::fs::write(worktree.path.join("a.txt"), "agent edit").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "original"
        );

        remove(&root, &worktree.path).await.unwrap();
        assert!(!worktree.path.exists());
        // The branch survives removal, so the work is still recoverable.
        assert!(
            git::git(&root, &["rev-parse", "--verify", "kage/run_20260809_001"])
                .await
                .is_ok()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn recreating_an_existing_worktree_preserves_work_in_progress() {
        // This is the resume path: the run crashed mid-execution and the partial work must survive.
        let root = repo("resume").await;
        commit(&root, "a.txt", "original").await;
        let worktrees = root.join(".kage/worktrees");

        let first = create(&root, &worktrees, "run_1", None).await.unwrap();
        std::fs::write(first.path.join("progress.txt"), "half done").unwrap();

        let second = create(&root, &worktrees, "run_1", None).await.unwrap();

        assert_eq!(first.path, second.path);
        assert_eq!(
            std::fs::read_to_string(second.path.join("progress.txt")).unwrap(),
            "half done"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_worktree_can_be_recreated_from_a_branch_that_outlived_it() {
        // This is the `kage clean` then `kage resume` path, and also what happens when a run dies
        // between creating the branch and creating the directory.
        let root = repo("orphan-branch").await;
        commit(&root, "a.txt", "original").await;
        let worktrees = root.join(".kage/worktrees");

        let first = create(&root, &worktrees, "run_1", None).await.unwrap();
        remove(&root, &first.path).await.unwrap();
        assert!(
            !first.path.exists(),
            "the checkout is gone but the branch remains"
        );

        let second = create(&root, &worktrees, "run_1", None)
            .await
            .expect("an existing branch must be reused, not rejected");

        assert_eq!(second.branch, "kage/run_1");
        assert!(second.path.join("a.txt").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_repository_with_no_commits_gets_an_actionable_error() {
        let root = repo("empty").await;

        let error = create(&root, &root.join(".kage/worktrees"), "run_1", None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no commits"));
        assert!(error.to_string().contains("isolate: false"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
