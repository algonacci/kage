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
///
/// Where they live is not a free choice. Coding agents sandbox themselves to their working
/// directory, so when a run is isolated the artifacts have to sit *inside* the worktree or the
/// agent cannot read its own prompt or write its deliverable — the harness refuses before the model
/// gets a say. But the worktree is disposable, and `kage status` must still work after `kage clean`
/// removes it. So agents work against a copy in the worktree that is mirrored back to the project's
/// own run directory after every phase.
pub struct Artifacts {
    /// Where agents read and write. Inside the worktree for an isolated run.
    pub dir: PathBuf,
    /// The durable copy under the project's `.kage/runs/`, when that is a different place.
    mirror: Option<PathBuf>,
}

impl Artifacts {
    /// The project's own run directory, with no worktree involved. This is what `kage status` reads.
    pub fn new(project: &Project, run_id: &str) -> Self {
        Self {
            dir: project.run_dir(run_id),
            mirror: None,
        }
    }

    /// The pair of locations a running phase uses, given where its agents will actually run.
    pub fn for_run(project: &Project, run_id: &str, workdir: &Path) -> Self {
        let canonical = project.run_dir(run_id);

        // Not isolated: the agent's working directory already contains the run directory.
        if workdir == project.root {
            return Self {
                dir: canonical,
                mirror: None,
            };
        }

        Self {
            dir: workdir
                .join(crate::config::KAGE_DIR)
                .join("runs")
                .join(run_id),
            mirror: Some(canonical),
        }
    }

    /// Copy every artifact back to the durable location.
    ///
    /// Called after each phase rather than at the end of the run, so a crash mid-loop still leaves
    /// a readable plan and review behind — which is exactly when someone needs them.
    pub fn sync(&self) -> Result<()> {
        let Some(mirror) = &self.mirror else {
            return Ok(());
        };

        copy_tree(&self.dir, mirror)
            .with_context(|| format!("cannot mirror artifacts to {}", mirror.display()))
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
        let mut dirs = vec![self.dir.clone(), self.prompts_dir(), self.logs_dir()];
        if let Some(mirror) = &self.mirror {
            dirs.push(mirror.clone());
        }

        for dir in dirs {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }

    /// Whether an artifact is really there: present, and not blank.
    ///
    /// The gate that enforces an artifact and the placeholder that stands in for a missing one have
    /// to ask the same question. When they did not, a whitespace-only file passed the gate and still
    /// reached the next agent as "_(not produced — … is missing or empty)_", which is the failure the
    /// gate exists to prevent.
    pub fn has_content(&self, path: &Path) -> bool {
        matches!(std::fs::read_to_string(path), Ok(content) if !content.trim().is_empty())
    }

    /// Read an artifact for embedding into a downstream prompt, or a placeholder when the agent
    /// never wrote it. A missing artifact is a fact the next agent should see, not a hard error —
    /// the reviewer can still judge code that shipped without an EXECUTION.md.
    pub fn read_or_placeholder(&self, path: &Path) -> String {
        let missing = || {
            format!(
                "_(not produced — {} is missing or empty)_",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        };
        if !self.has_content(path) {
            return missing();
        }
        match std::fs::read_to_string(path) {
            Ok(content) => content,
            // The gate already read this file a moment ago; losing it to a race still yields a
            // placeholder rather than an empty string that reads as a produced artifact.
            Err(_) => missing(),
        }
    }
}

/// Recursively copy `from` over `to`, creating directories as needed.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;

    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());

        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }

    Ok(())
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
    fn an_isolated_run_keeps_its_artifacts_inside_the_worktree() {
        // The bug this guards: agents run with the worktree as their working directory and sandbox
        // themselves to it, so artifacts in the project's own .kage/ are unreachable — the harness
        // refuses to read the prompt or write the plan, and every isolated run dies at PLAN.
        let (root, project) = project("isolated");
        let worktree = root.join(".kage/worktrees/run_1");

        let artifacts = Artifacts::for_run(&project, "run_1", &worktree);

        assert!(
            artifacts.plan().starts_with(&worktree),
            "agents cannot reach {}",
            artifacts.plan().display()
        );
        assert!(artifacts.prompts_dir().starts_with(&worktree));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_isolated_run_writes_straight_to_the_project() {
        let (root, project) = project("plain");

        let artifacts = Artifacts::for_run(&project, "run_1", &project.root);

        assert_eq!(artifacts.plan(), project.run_dir("run_1").join("PLAN.md"));
        assert!(artifacts.sync().is_ok(), "nothing to mirror");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn artifacts_survive_the_worktree_being_deleted() {
        // `kage clean` removes the checkout; `kage status` must still show the plan and review.
        let (root, project) = project("mirror");
        let worktree = root.join(".kage/worktrees/run_1");
        let artifacts = Artifacts::for_run(&project, "run_1", &worktree);
        artifacts.ensure_dirs().unwrap();

        std::fs::write(artifacts.plan(), "# Objective\nship it").unwrap();
        std::fs::write(artifacts.logs_dir().join("planner.log"), "output").unwrap();
        artifacts.sync().unwrap();

        std::fs::remove_dir_all(&worktree).unwrap();

        let durable = Artifacts::new(&project, "run_1");
        assert_eq!(
            std::fs::read_to_string(durable.plan()).unwrap(),
            "# Objective\nship it"
        );
        assert!(
            durable.logs_dir().join("planner.log").is_file(),
            "nested directories must be mirrored too"
        );

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

    #[test]
    fn a_blank_artifact_does_not_count_as_content() {
        let (root, project) = project("has-content");
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();

        assert!(
            !artifacts.has_content(&artifacts.execution()),
            "an absent file has no content"
        );

        std::fs::write(artifacts.execution(), "   \n\t\n").unwrap();
        assert!(
            !artifacts.has_content(&artifacts.execution()),
            "a whitespace-only file must count as missing"
        );

        std::fs::write(artifacts.execution(), "implemented the plan").unwrap();
        assert!(artifacts.has_content(&artifacts.execution()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_gate_and_the_placeholder_agree_on_what_missing_means() {
        // Regression guard: when the gate asked `is_file()` while the placeholder triggered on
        // blank content, a whitespace-only file passed one and still reached the next agent as
        // "missing or empty" — the exact failure the gate exists to prevent. The two must never
        // disagree, so this asserts the equivalence that keeps them honest.
        let (root, project) = project("agree");
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();

        let cases = [(false, "absent"), (true, "blank"), (true, "real")];
        let execution = artifacts.execution();

        for (write, label) in cases {
            match write {
                false => {
                    let _ = std::fs::remove_file(&execution);
                }
                true if label == "blank" => std::fs::write(&execution, " \n\t ").unwrap(),
                _ => std::fs::write(&execution, "account of the work").unwrap(),
            }

            assert_eq!(
                artifacts.has_content(&execution),
                !artifacts
                    .read_or_placeholder(&execution)
                    .contains("missing or empty"),
                "the gate and the placeholder disagree for {label} content"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
