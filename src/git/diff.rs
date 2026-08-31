//! Collecting what the executor actually changed, for the reviewer to judge.

use std::path::Path;

use anyhow::Result;

use crate::git;

/// Cap on the diff embedded in a review prompt.
///
/// A generated migration or a lockfile can produce a diff larger than any context window. Sending
/// it whole would push the plan out of the reviewer's context — losing the very thing it reviews
/// against — so an oversized diff is truncated and labelled as such.
const MAX_DIFF_CHARS: usize = 120_000;

/// Keep Kage's own files out of the diff.
///
/// An isolated run writes its plan, review, and prompts inside the worktree so the agents can reach
/// them. Without this the reviewer would be shown its own artifacts as if the executor had authored
/// them, and every run would look like it touched Kage's directories.
///
/// Two entries because they are different things: `.kage` is the project's own configuration, which
/// a repository may commit and an agent must not quietly edit, while `.kage-run` is this run's
/// scratch copy of its artifacts, which no diff should ever contain.
pub(crate) const EXCLUDE_KAGE: &[&str] = &[":(exclude).kage", ":(exclude).kage-run"];

/// Everything that changed since `base_commit`, committed or not.
///
/// Agents are inconsistent about committing: some commit each step, some leave everything in the
/// working tree. Diffing against the run's base commit covers both, and marking untracked files
/// with `--intent-to-add` first means a brand-new file shows up as content rather than as a name in
/// an untracked list.
pub async fn since(workdir: &Path, base_commit: &str) -> Result<String> {
    // Recording intent-to-add stages nothing and changes no file contents; it only makes untracked
    // paths visible to `git diff`.
    let _ = git::git(
        workdir,
        &[
            &["add", "--all", "--intent-to-add", "--", "."][..],
            EXCLUDE_KAGE,
        ]
        .concat(),
    )
    .await;

    let diff = git::git(
        workdir,
        &[&["diff", base_commit, "--", "."][..], EXCLUDE_KAGE].concat(),
    )
    .await?;

    if diff.trim().is_empty() {
        return Ok("_(no changes were made)_".to_string());
    }

    Ok(truncate(&diff))
}

/// A one-line-per-file summary, for terminal output where a full diff would drown the user.
pub async fn stat(workdir: &Path, base_commit: &str) -> Result<String> {
    let _ = git::git(
        workdir,
        &[
            &["add", "--all", "--intent-to-add", "--", "."][..],
            EXCLUDE_KAGE,
        ]
        .concat(),
    )
    .await;
    Ok(git::git(
        workdir,
        &[
            &["diff", "--stat", base_commit, "--", "."][..],
            EXCLUDE_KAGE,
        ]
        .concat(),
    )
    .await?
    .trim()
    .to_string())
}

/// File names only, for post-join overlap detection.
pub async fn name_only(workdir: &Path, base_commit: &str) -> Result<String> {
    let _ = git::git(workdir, &["add", "--intent-to-add", "--", "."]).await;
    let out = git::git(
        workdir,
        &[
            &["diff", "--name-only", base_commit, "--", "."][..],
            EXCLUDE_KAGE,
        ]
        .concat(),
    )
    .await?;
    Ok(out)
}

fn truncate(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_CHARS {
        return diff.to_string();
    }

    // Cut on a char boundary; a diff can contain multi-byte content.
    let mut cut = MAX_DIFF_CHARS;
    while cut > 0 && !diff.is_char_boundary(cut) {
        cut -= 1;
    }

    format!(
        "{}\n\n[... diff truncated at {MAX_DIFF_CHARS} characters of {} total. \
         Inspect the full diff in the run's worktree if you need it. ...]",
        &diff[..cut],
        diff.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn repo(label: &str) -> (PathBuf, String) {
        let root = std::env::temp_dir().join(format!("kage-diff-{label}-{}", std::process::id()));
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
    async fn uncommitted_edits_and_new_files_both_appear() {
        let (root, base) = repo("uncommitted").await;
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("brand-new.txt"), "hello\n").unwrap();

        let diff = since(&root, &base).await.unwrap();

        assert!(diff.contains("a.txt"), "{diff}");
        assert!(
            diff.contains("brand-new.txt"),
            "untracked file missing:\n{diff}"
        );
        assert!(diff.contains("hello"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn committed_work_still_shows_against_the_base() {
        let (root, base) = repo("committed").await;
        std::fs::write(root.join("a.txt"), "committed change\n").unwrap();
        git::git(&root, &["add", "."]).await.unwrap();
        git::git(&root, &["commit", "-m", "agent commit"])
            .await
            .unwrap();

        let diff = since(&root, &base).await.unwrap();

        assert!(diff.contains("committed change"), "{diff}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn kage_own_artifacts_stay_out_of_the_diff() {
        // An isolated run writes its plan inside the worktree. Showing that to the reviewer would
        // present Kage's own scaffolding as work the executor did.
        let (root, base) = repo("exclude-kage").await;
        std::fs::create_dir_all(root.join(".kage/runs/run_1")).unwrap();
        std::fs::write(root.join(".kage/runs/run_1/PLAN.md"), "# Objective\n").unwrap();
        // The in-worktree copy an isolated run actually writes.
        std::fs::create_dir_all(root.join(".kage-run/logs")).unwrap();
        std::fs::write(root.join(".kage-run/REVIEW.md"), "# Review\n").unwrap();
        std::fs::write(root.join("real-work.txt"), "the actual change\n").unwrap();

        let diff = since(&root, &base).await.unwrap();

        assert!(diff.contains("real-work.txt"), "{diff}");
        assert!(!diff.contains("PLAN.md"), "kage artifacts leaked:\n{diff}");
        assert!(!diff.contains("REVIEW.md"), "kage-run leaked:\n{diff}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn an_untouched_tree_says_so_explicitly() {
        let (root, base) = repo("clean").await;

        let diff = since(&root, &base).await.unwrap();

        assert!(diff.contains("no changes"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversized_diff_is_truncated_with_a_marker() {
        let huge = "x".repeat(MAX_DIFF_CHARS + 500);

        let out = truncate(&huge);

        assert!(out.len() < huge.len() + 200);
        assert!(out.contains("truncated"));
    }

    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        // A naive byte slice here panics; diffs of non-ASCII source must not crash the run.
        let huge = "é".repeat(MAX_DIFF_CHARS);

        let out = truncate(&huge);

        assert!(out.contains("truncated"));
    }
}
