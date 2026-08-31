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

    /// Where an isolated run's artifacts sit inside its worktree.
    ///
    /// Deliberately shares no path segment with the route to the worktree itself. The worktree
    /// lives at `<project>/.kage/worktrees/<run_id>/`, so putting artifacts under `.kage/runs/
    /// <run_id>/` inside it produced `…/.kage/worktrees/<id>/.kage/runs/<id>/EXECUTION.md` — a path
    /// with `.kage` twice and the run id twice. An executor collapsed the repetition, asked for
    /// `<project>/.kage/runs/<id>/` instead, was refused for reaching outside its sandbox, and
    /// spent twenty minutes failing to write a file it had been given the correct path to.
    ///
    /// Kage is driven by imperfect agents by design, so a path that can be normalised into a wrong
    /// one is a defect here rather than a mistake there. No run id either: a worktree belongs to
    /// exactly one run, and repeating it was the other half of the ambiguity.
    const WORKTREE_ARTIFACTS: &str = ".kage-run";

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
            dir: workdir.join(Self::WORKTREE_ARTIFACTS),
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

    /// Copy the durable artifacts back into a worktree that no longer has them.
    ///
    /// The mirror exists because the worktree is disposable; this is the other half of that deal.
    /// `.kage/` is never committed, so a worktree rebuilt on `kage resume` after `kage clean` is a
    /// checkout of tracked files and nothing else: no `PLAN.md`, no `TEST_RESULTS.md`, and the next
    /// prompt embeds their placeholders — the reviewer is handed "_(not produced …)_" where the plan
    /// it must judge against belongs, while the project's mirror holds the real one all along.
    ///
    /// `EXECUTION.md` is deliberately not restored. It is the one artifact tied to a single attempt
    /// rather than to the run: both phases that write it delete any previous one first, precisely so
    /// a past attempt's account cannot be presented as this one's, and copying a mirrored account
    /// back in would reintroduce exactly what those deletions prevent. Its absence already has a
    /// designed remedy — the executor is asked once more for just the summary, written against the
    /// current diff — and that remedy is the only one that can promise the account describes the
    /// work now on the branch.
    pub fn restore(&self) -> Result<()> {
        let Some(mirror) = &self.mirror else {
            return Ok(());
        };

        copy_tree(mirror, &self.dir)
            .with_context(|| format!("cannot restore artifacts from {}", mirror.display()))?;

        let _ = std::fs::remove_file(self.execution());
        Ok(())
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

    pub fn subagent_dir(&self, id: &str) -> PathBuf {
        self.dir.join("subagents").join(id)
    }

    pub fn shared_discussion(&self) -> PathBuf {
        self.dir.join("shared/discussion.md")
    }

    // ponytail: simple concat — no abstraction, just headers + file content.
    pub fn collect_shards(&self) -> Result<String> {
        let subagents_root = self.dir.join("subagents");
        if !subagents_root.is_dir() { return Ok(String::new()); }
        let mut entries: Vec<String> = std::fs::read_dir(&subagents_root)?.filter_map(|e| e.ok()).filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        entries.sort();
        let mut out = String::new();
        if !entries.is_empty() {
            out.push_str("# Partition Map\n\n");
            out.push_str("| Subagent | Files |\n|---|---|\n");
            for id in &entries {
                let meta_path = subagents_root.join(id).join("meta.json");
                let files = std::fs::read_to_string(&meta_path).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v.get("files").cloned()).map(|v| v.as_array().map(|arr| arr.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()).unwrap_or_default();
                let files = if files.is_empty() { "(none)".to_string() } else { files };
                out.push_str(&format!("| {id} | {files} |\n"));
            }
            out.push('\n');
        }
        for id in &entries {
            let shard = subagents_root.join(id).join("EXECUTION.md");
            let content = std::fs::read_to_string(&shard).unwrap_or_default();
            if content.trim().is_empty() { continue; }
            out.push_str(&format!("## Subagent {id}\n\n"));
            out.push_str(content.trim());
            out.push_str("\n\n");
        }
        Ok(out)
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
    /// a fixer can still work without TEST_RESULTS.md on disk. `EXECUTION.md` is the exception:
    /// its gate fails the run before any prompt would embed its placeholder, so the reviewer never
    /// judges against one.
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
    fn the_worktree_artifact_path_repeats_no_segment_of_the_route_to_it() {
        // The bug this guards: artifacts under `<worktree>/.kage/runs/<id>/` gave a path with
        // `.kage` twice and the run id twice, because the worktree itself lives under
        // `<project>/.kage/worktrees/<id>/`. An executor collapsed the repetition, asked for a path
        // outside its sandbox, was refused, and spent twenty minutes writing nothing.
        let (root, project) = project("no-repeat");
        let worktree = project.worktrees_dir().join("run_20260810_001");

        let artifacts = Artifacts::for_run(&project, "run_20260810_001", &worktree);
        let relative = artifacts
            .execution()
            .strip_prefix(&worktree)
            .expect("artifacts live inside the worktree")
            .to_string_lossy()
            .replace('\\', "/");

        assert!(
            !relative.contains(".kage/"),
            "the path inside the worktree repeats `.kage`: {relative}"
        );
        assert!(
            !relative.contains("run_20260810_001"),
            "the path inside the worktree repeats the run id: {relative}"
        );

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
    fn a_rebuilt_worktree_gets_its_artifacts_back_from_the_mirror() {
        // The bug this guards: `.kage/` is never committed, so a worktree recreated by `kage resume`
        // after `kage clean` held no PLAN.md or TEST_RESULTS.md. The prompts built there embedded
        // "_(not produced — PLAN.md is missing or empty)_" and the reviewer judged the work against
        // a placeholder, while the project's mirror held the real plan the whole time.
        let (root, project) = project("restore");
        let worktree = root.join(".kage/worktrees/run_1");
        let artifacts = Artifacts::for_run(&project, "run_1", &worktree);
        artifacts.ensure_dirs().unwrap();

        std::fs::write(artifacts.plan(), "# Objective\nship it").unwrap();
        std::fs::write(artifacts.test_results(), "# Test Results\nall green").unwrap();
        std::fs::write(artifacts.logs_dir().join("planner.log"), "output").unwrap();
        artifacts.sync().unwrap();

        // `kage clean` removes the checkout, and `kage resume` rebuilds it from the branch.
        std::fs::remove_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        assert!(
            !artifacts.has_content(&artifacts.plan()),
            "a rebuilt checkout starts with no artifacts in it"
        );

        artifacts.restore().unwrap();

        assert_eq!(
            std::fs::read_to_string(artifacts.plan()).unwrap(),
            "# Objective\nship it"
        );
        assert!(artifacts.has_content(&artifacts.test_results()));
        assert!(
            artifacts.logs_dir().join("planner.log").is_file(),
            "nested directories must come back too"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_restored_worktree_still_has_to_ask_for_the_executors_account() {
        // EXECUTION.md is tied to one attempt rather than to the run: both phases that write it
        // delete any previous one first, so that a past attempt's account is never presented as
        // this one's. Restoring a mirrored account would reintroduce exactly that, and the run
        // would review the current diff against an older attempt's claims.
        let (root, project) = project("restore-account");
        let worktree = root.join(".kage/worktrees/run_1");
        let artifacts = Artifacts::for_run(&project, "run_1", &worktree);
        artifacts.ensure_dirs().unwrap();

        std::fs::write(artifacts.plan(), "# Objective\nship it").unwrap();
        std::fs::write(artifacts.execution(), "an earlier attempt's account").unwrap();
        artifacts.sync().unwrap();

        std::fs::remove_dir_all(&worktree).unwrap();
        artifacts.restore().unwrap();

        assert!(artifacts.has_content(&artifacts.plan()));
        assert!(
            !artifacts.has_content(&artifacts.execution()),
            "the account must be re-asked for, not copied back"
        );
        // The mirror still keeps it — `kage status` reads that copy.
        let durable = Artifacts::new(&project, "run_1");
        assert!(
            durable.has_content(&durable.execution()),
            "restoring must not damage the durable copy"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restoring_without_a_mirror_is_a_no_op() {
        let (root, project) = project("restore-plain");

        let artifacts = Artifacts::for_run(&project, "run_1", &project.root);
        artifacts.ensure_dirs().unwrap();
        std::fs::write(artifacts.execution(), "the only copy there is").unwrap();

        artifacts.restore().unwrap();

        assert!(
            artifacts.has_content(&artifacts.execution()),
            "a non-isolated run's artifacts are the durable ones — restore must not delete them"
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

    #[test]
    fn subagent_dir_is_under_artifacts() {
        let (root, project) = project("subagent-dir");
        let artifacts = Artifacts::new(&project, "run_1");
        assert_eq!(artifacts.subagent_dir("auth"), project.run_dir("run_1").join("subagents/auth"));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn shared_discussion_is_under_artifacts() {
        let (root, project) = project("shared-disc");
        let artifacts = Artifacts::new(&project, "run_1");
        assert_eq!(artifacts.shared_discussion(), project.run_dir("run_1").join("shared/discussion.md"));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn collect_shards_concatenates_with_headers_and_partition_map() {
        let (root, project) = project("collect");
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();
        for (id, content, files) in [("auth", "did auth", r#"["src/auth.rs"]"#), ("health", "did health", r#"["src/health.rs"]"#)] {
            let dir = artifacts.subagent_dir(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("EXECUTION.md"), content).unwrap();
            std::fs::write(dir.join("meta.json"), format!(r#"{{"files": {files}}}"#)).unwrap();
        }
        let out = artifacts.collect_shards().unwrap();
        assert!(out.contains("# Partition Map"));
        assert!(out.contains("## Subagent auth"));
        assert!(out.contains("did auth"));
        assert!(out.contains("## Subagent health"));
        assert!(out.contains("did health"));
        assert!(out.find("auth").unwrap() < out.find("health").unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn collect_shards_returns_empty_when_no_subagents() {
        let (root, project) = project("collect-empty");
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();
        assert_eq!(artifacts.collect_shards().unwrap(), "");
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn subagent_shards_survive_crash_via_mirror() {
        let (root, project) = project("shard-crash");
        let worktree = root.join(".kage/worktrees/run_1");
        let artifacts = Artifacts::for_run(&project, "run_1", &worktree);
        artifacts.ensure_dirs().unwrap();
        let dir = artifacts.subagent_dir("auth");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("EXECUTION.md"), "auth shard").unwrap();
        std::fs::write(dir.join("meta.json"), r#"{"files": ["src/auth.rs"]}"#).unwrap();
        std::fs::create_dir_all(artifacts.shared_discussion().parent().unwrap()).unwrap();
        std::fs::write(artifacts.shared_discussion(), "hello").unwrap();
        artifacts.sync().unwrap();
        std::fs::remove_dir_all(&worktree).unwrap();
        let durable = Artifacts::new(&project, "run_1");
        assert!(durable.subagent_dir("auth").join("EXECUTION.md").is_file());
        assert!(durable.shared_discussion().is_file());
        let out = durable.collect_shards().unwrap();
        assert!(out.contains("auth shard"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
