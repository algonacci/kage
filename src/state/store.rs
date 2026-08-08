//! Crash-durable persistence for run state.
//!
//! Filesystem JSON rather than SQLite: a run is a handful of writes per minute, and a plain file
//! the user can `cat` when a run misbehaves is worth more here than query power.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Project;
use crate::state::run::RunState;

pub const STATE_FILE: &str = "state.json";

/// Write run state so that a crash cannot leave a half-written file behind.
///
/// Writes to a sibling temp file and renames over the target: rename is atomic on both NTFS and
/// POSIX, so `state.json` is always either the previous state or the new one, never a truncated
/// mix. Without this a crash mid-write would strand a run that `kage resume` could not read.
pub fn save(project: &Project, state: &RunState) -> Result<()> {
    let dir = project.run_dir(&state.id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;

    let target = dir.join(STATE_FILE);
    let temp = dir.join(format!("{STATE_FILE}.tmp"));
    let encoded = serde_json::to_vec_pretty(state).context("cannot encode run state")?;

    std::fs::write(&temp, &encoded).with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, &target)
        .with_context(|| format!("cannot replace {}", target.display()))?;

    Ok(())
}

pub fn load(project: &Project, run_id: &str) -> Result<RunState> {
    let path = project.run_dir(run_id).join(STATE_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no run state at {}", path.display()))?;

    serde_json::from_str(&raw).with_context(|| format!("corrupt run state at {}", path.display()))
}

/// Every run id on disk, oldest first. Ids sort chronologically by construction.
pub fn list_run_ids(project: &Project) -> Result<Vec<String>> {
    let runs = project.runs_dir();
    if !runs.is_dir() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::new();
    for entry in
        std::fs::read_dir(&runs).with_context(|| format!("cannot read {}", runs.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join(STATE_FILE).is_file() {
            ids.push(name);
        }
    }

    ids.sort();
    Ok(ids)
}

pub fn latest_run_id(project: &Project) -> Result<String> {
    list_run_ids(project)?
        .pop()
        .context("no runs found — start one with `kage run \"<task>\"`")
}

/// Resolve an explicit run id, falling back to the most recent run.
pub fn resolve(project: &Project, run_id: Option<&str>) -> Result<RunState> {
    match run_id {
        Some(id) => {
            if !project.run_dir(id).is_dir() {
                bail!("unknown run `{id}`");
            }
            load(project, id)
        }
        None => {
            let id = latest_run_id(project)?;
            load(project, &id)
        }
    }
}

/// Per-run artifact paths. Agents read and write these files; Kage only decides where they live.
pub struct Artifacts {
    pub dir: PathBuf,
}

impl Artifacts {
    pub fn new(project: &Project, run_id: &str) -> Self {
        Self {
            dir: project.run_dir(run_id),
        }
    }

    pub fn request(&self) -> PathBuf {
        self.dir.join("REQUEST.md")
    }

    pub fn plan(&self) -> PathBuf {
        self.dir.join("PLAN.md")
    }

    pub fn execution(&self) -> PathBuf {
        self.dir.join("EXECUTION.md")
    }

    pub fn test_results(&self) -> PathBuf {
        self.dir.join("TEST_RESULTS.md")
    }

    pub fn review(&self) -> PathBuf {
        self.dir.join("REVIEW.md")
    }

    pub fn verdict(&self) -> PathBuf {
        self.dir.join("VERDICT.json")
    }

    pub fn prompts_dir(&self) -> PathBuf {
        self.dir.join("prompts")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [self.dir.clone(), self.prompts_dir(), self.logs_dir()] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }

    /// Read an artifact for embedding into a downstream prompt, or a placeholder when the agent
    /// never wrote it. A missing artifact is a fact the next agent should see, not a hard error —
    /// the reviewer can still judge code that shipped without an EXECUTION.md.
    pub fn read_or_placeholder(&self, path: &Path) -> String {
        match std::fs::read_to_string(path) {
            Ok(content) if !content.trim().is_empty() => content,
            _ => format!(
                "_(not produced — {} is missing or empty)_",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::run::Phase;

    fn project(label: &str) -> (PathBuf, Project) {
        let root = std::env::temp_dir().join(format!("kage-store-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();
        let project = Project::discover(&root).unwrap();
        (root, project)
    }

    #[test]
    fn state_survives_a_save_load_round_trip() {
        let (root, project) = project("roundtrip");
        let mut state = RunState::new(
            "run_20260809_001".to_string(),
            "add rate limiting".to_string(),
            root.clone(),
            3,
        );
        state.transition(Phase::Reviewing, "reviewer starting");

        save(&project, &state).unwrap();
        let loaded = load(&project, &state.id).unwrap();

        assert_eq!(loaded.phase, Phase::Reviewing);
        assert_eq!(loaded.task, "add rate limiting");
        assert_eq!(loaded.history.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_twice_leaves_no_temp_file_behind() {
        let (root, project) = project("atomic");
        let state = RunState::new("run_1".to_string(), "t".to_string(), root.clone(), 3);

        save(&project, &state).unwrap();
        save(&project, &state).unwrap();

        assert!(!project.run_dir("run_1").join("state.json.tmp").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_without_an_id_picks_the_newest_run() {
        let (root, project) = project("resolve");
        for id in ["run_20260809_001", "run_20260809_002"] {
            let state = RunState::new(id.to_string(), "t".to_string(), root.clone(), 3);
            save(&project, &state).unwrap();
        }

        assert_eq!(resolve(&project, None).unwrap().id, "run_20260809_002");
        assert!(resolve(&project, Some("run_nope")).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_artifact_becomes_a_placeholder_not_an_error() {
        let (root, project) = project("placeholder");
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();

        let text = artifacts.read_or_placeholder(&artifacts.plan());

        assert!(text.contains("PLAN.md"));
        assert!(text.contains("missing"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
