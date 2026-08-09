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

/// Whether the repository has any commit yet.
///
/// A fresh `git init` has no HEAD, and worktrees cannot branch from nothing — worth detecting up
/// front so the failure names the real cause.
pub async fn has_commits(workdir: &Path) -> bool {
    git(workdir, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok()
}
