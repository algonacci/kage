//! Putting the run's work onto a branch, so deleting the worktree cannot delete the work.
//!
//! The worktree is a throwaway checkout, and `kage clean` force-removes it when the run is done.
//! The branch is the only thing that survives that removal, so a run that never commits its work
//! has its work vanish the moment anyone cleans up. This module stages everything the agents wrote
//! — everything except Kage's own `.kage/` directory, which is mirrored to the project and does not
//! belong in the run's history — and commits it on whatever HEAD points at.
//!
//! Tracked-but-modified files under `.kage/` stay dirty in the worktree and are lost when it is
//! removed; they are already mirrored to the project's own run directory by `Artifacts::sync`,
//! which is the durable copy.

use std::path::Path;

use anyhow::{Context, Result};

use crate::git;
use crate::git::diff::EXCLUDE_KAGE;
use crate::state::Commitment;

/// Commit everything in `workdir` except Kage's own `.kage/` directory, and report what the
/// branch holds afterwards.
///
/// `Err` means git itself refused; the caller decides what `kage clean` should do about the
/// worktree in that case. A clean tree with nothing to stage is not an error.
pub async fn commit_work(
    workdir: &Path,
    base_commit: &str,
    subject: &str,
    body: &str,
) -> Result<Commitment> {
    git::git(
        workdir,
        &[&["add", "--all", "--", "."][..], EXCLUDE_KAGE].concat(),
    )
    .await
    .context("cannot stage the run's work")?;

    let cached = git::git(
        workdir,
        &[
            &["diff", "--cached", "--name-only", "--", "."][..],
            EXCLUDE_KAGE,
        ]
        .concat(),
    )
    .await
    .context("cannot list the staged work")?;
    let staged = !cached.trim().is_empty();

    if staged {
        // A repository with a passphrase-protected signing key would otherwise block on a prompt
        // nobody can answer until the 120s git timeout kills the commit, and a failing pre-commit
        // hook would leave the work uncommitted for `kage clean` to delete. This is a preservation
        // snapshot, not a contribution to history, so both are disabled for this one invocation;
        // the user can amend, re-sign, or reword before merging.
        let name = git::git(workdir, &["config", "--get", "user.name"])
            .await
            .ok()
            .map(|value| value.trim().to_string());
        let email = git::git(workdir, &["config", "--get", "user.email"])
            .await
            .ok()
            .map(|value| value.trim().to_string());

        let identity = identity_args(name.as_deref(), email.as_deref());
        let mut args: Vec<&str> = identity.iter().map(String::as_str).collect();
        args.extend([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "-m",
            subject,
            "-m",
            body,
        ]);
        git::git(workdir, &args)
            .await
            .context("cannot commit the run's work")?;
    }

    // A detached HEAD is still work worth reporting; the summary then names the actual branch, so
    // it never tells the user to merge a branch that does not contain the commit.
    let branch = git::git(workdir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .map(|out| out.trim().to_string())
        .unwrap_or_else(|_| "(detached HEAD)".to_string());

    let head = git::git(workdir, &["rev-parse", "HEAD"])
        .await
        .context("cannot resolve HEAD")?
        .trim()
        .to_string();

    let changed = git::git(
        workdir,
        &[
            &["diff", "--name-only", base_commit, &head, "--", "."][..],
            EXCLUDE_KAGE,
        ]
        .concat(),
    )
    .await
    .context("cannot count the files on the branch")?;
    let files_changed = changed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    // The count is against the base commit, not the commit Kage just made: a run where the agent
    // had already committed everything must still report that work, just not as Kage's own.
    if staged {
        Ok(Commitment::Committed {
            sha: head,
            branch,
            files_changed,
            created: true,
        })
    } else if files_changed > 0 {
        Ok(Commitment::Committed {
            sha: head,
            branch,
            files_changed,
            created: false,
        })
    } else {
        Ok(Commitment::NothingToCommit { branch })
    }
}

/// Arguments that give git an identity when the environment has not configured one.
///
/// A CI container or a fresh machine has no `user.name`, and there `git commit` fails with
/// "Please tell me who you are" — losing the run's work for a reason that has nothing to do with
/// the work. A configured identity is always preferred and never overridden.
fn identity_args(name: Option<&str>, email: Option<&str>) -> Vec<String> {
    let name = name.map(str::trim).unwrap_or_default();
    let email = email.map(str::trim).unwrap_or_default();

    if name.is_empty() || email.is_empty() {
        vec![
            "-c".to_string(),
            "user.name=Kage".to_string(),
            "-c".to_string(),
            "user.email=kage@localhost".to_string(),
        ]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn repo(label: &str) -> (PathBuf, String) {
        let root = std::env::temp_dir().join(format!("kage-commit-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        git::git(&root, &["init", "-b", "main"]).await.unwrap();
        git::git(&root, &["config", "user.email", "kage@test.local"])
            .await
            .unwrap();
        git::git(&root, &["config", "user.name", "Kage Test"])
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git::git(&root, &["add", "."]).await.unwrap();
        git::git(&root, &["commit", "-m", "initial"]).await.unwrap();

        let base = git::head_commit(&root).await.unwrap();
        (root, base)
    }

    #[tokio::test]
    async fn tracked_edits_and_new_files_are_committed_to_the_branch() {
        let (root, base) = repo("tracked-and-new").await;
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("brand-new.txt"), "hello\n").unwrap();

        let commitment = commit_work(&root, &base, "kage run_1: add caching", "Task: add caching")
            .await
            .unwrap();

        match commitment {
            Commitment::Committed {
                sha,
                branch,
                files_changed,
                created,
            } => {
                assert!(created);
                assert_eq!(files_changed, 2);
                assert_eq!(branch, "main");
                let names = git::git(&root, &["show", "--name-only", "--format=", &sha])
                    .await
                    .unwrap();
                assert!(names.contains("a.txt"), "{names}");
                assert!(names.contains("brand-new.txt"), "{names}");
            }
            other => panic!("unexpected, expected a new commit: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn kage_artifacts_are_never_committed() {
        let (root, base) = repo("exclude-kage").await;
        std::fs::create_dir_all(root.join(".kage/runs/run_1")).unwrap();
        std::fs::write(root.join(".kage/runs/run_1/PLAN.md"), "# Objective\n").unwrap();
        std::fs::write(root.join("real-work.txt"), "the actual change\n").unwrap();

        let commitment = commit_work(&root, &base, "subject", "body").await.unwrap();

        let Commitment::Committed { sha, .. } = commitment else {
            panic!("expected a commit, got {commitment:?}");
        };
        let names = git::git(&root, &["show", "--name-only", "--format=", &sha])
            .await
            .unwrap();
        assert!(names.contains("real-work.txt"), "{names}");
        assert!(
            !names
                .lines()
                .any(|line| line.trim_start().starts_with(".kage")),
            "kage artifacts leaked:\n{names}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn an_unchanged_tree_produces_no_commit() {
        let (root, base) = repo("unchanged").await;

        let commitment = commit_work(&root, &base, "subject", "body").await.unwrap();

        assert!(matches!(commitment, Commitment::NothingToCommit { .. }));
        assert_eq!(git::head_commit(&root).await.unwrap(), base);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn work_the_agent_already_committed_is_reported_not_duplicated() {
        let (root, base) = repo("agent-committed").await;
        std::fs::write(root.join("a.txt"), "agent change\n").unwrap();
        git::git(&root, &["add", "."]).await.unwrap();
        git::git(&root, &["commit", "-m", "the agent's own commit"])
            .await
            .unwrap();
        let count_before = git::git(&root, &["rev-list", "--count", "HEAD"])
            .await
            .unwrap();

        let commitment = commit_work(&root, &base, "subject", "body").await.unwrap();

        match commitment {
            Commitment::Committed {
                files_changed,
                created,
                ..
            } => {
                assert!(!created);
                assert_eq!(files_changed, 1);
            }
            other => panic!("expected a commit, got {other:?}"),
        }
        let count_after = git::git(&root, &["rev-list", "--count", "HEAD"])
            .await
            .unwrap();
        assert_eq!(count_before, count_after, "Kage must not add a commit");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_deleted_file_is_committed_as_a_deletion() {
        let (root, base) = repo("deleted").await;
        std::fs::remove_file(root.join("a.txt")).unwrap();

        let commitment = commit_work(&root, &base, "subject", "body").await.unwrap();

        let Commitment::Committed { sha, .. } = commitment else {
            panic!("expected a commit, got {commitment:?}");
        };
        let names = git::git(&root, &["show", "--name-only", "--format=", &sha])
            .await
            .unwrap();
        assert!(names.contains("a.txt"), "{names}");
        let tree = git::git(&root, &["ls-tree", "-r", "--name-only", "HEAD"])
            .await
            .unwrap();
        assert!(
            !tree.lines().any(|line| line == "a.txt"),
            "deletion missing from the tree:\n{tree}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn identity_args_only_fills_in_what_git_is_missing() {
        let fallback = vec![
            "-c".to_string(),
            "user.name=Kage".to_string(),
            "-c".to_string(),
            "user.email=kage@localhost".to_string(),
        ];

        assert!(identity_args(Some("Jane"), Some("jane@example.com")).is_empty());
        assert_eq!(
            identity_args(Some("Jane"), Some("   ")),
            fallback,
            "a whitespace-only email is a missing email"
        );
        assert_eq!(
            identity_args(Some("   "), Some("jane@example.com")),
            fallback,
            "a whitespace-only name is a missing name"
        );
        assert_eq!(
            identity_args(None, Some("jane@example.com")),
            fallback,
            "a missing name falls back"
        );
        assert_eq!(
            identity_args(Some("Jane"), None),
            fallback,
            "a missing email falls back"
        );
        assert_eq!(identity_args(None, None), fallback);
    }
}
