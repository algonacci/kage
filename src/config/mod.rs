//! Locating, loading, and scaffolding the Kage project directory.

pub mod schema;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub use schema::{AdapterKind, Config, PromptDelivery, RoleConfig, Roles, Validation};

/// Directory name that marks a Kage project, mirroring `.git`.
pub const KAGE_DIR: &str = ".kage";
pub const CONFIG_FILE: &str = "config.yaml";

/// A resolved Kage project: the repository root and its `.kage` directory.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub kage_dir: PathBuf,
}

impl Project {
    /// Walk up from `start` looking for `.kage`, the way git finds its repository root.
    ///
    /// This is what lets `kage status` work from a subdirectory of the project.
    pub fn discover(start: &Path) -> Result<Self> {
        if !start.exists() {
            bail!("{} does not exist", start.display());
        }
        let start = crate::paths::canonical(start);

        for dir in start.ancestors() {
            let candidate = dir.join(KAGE_DIR);
            if candidate.is_dir() {
                return Ok(Self {
                    root: dir.to_path_buf(),
                    kage_dir: candidate,
                });
            }
        }

        bail!(
            "no {KAGE_DIR}/ directory found in {} or any parent directory\n\
             run `kage init` in your project root first",
            start.display()
        )
    }

    pub fn config_path(&self) -> PathBuf {
        self.kage_dir.join(CONFIG_FILE)
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.kage_dir.join("runs")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.kage_dir.join("worktrees")
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }

    pub fn load_config(&self) -> Result<Config> {
        let path = self.config_path();
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;

        serde_yaml_ng::from_str(&raw)
            .with_context(|| format!("invalid config at {}", path.display()))
    }

    /// Run ids are date-ordered and human-readable (`run_20260809_001`) so that sorting a directory
    /// listing sorts chronologically, and a user can say "run 3 from Sunday" out loud.
    pub fn next_run_id(&self, now: chrono::DateTime<chrono::Local>) -> Result<String> {
        let date = now.format("%Y%m%d").to_string();
        let prefix = format!("run_{date}_");

        let mut highest = 0usize;
        let runs = self.runs_dir();
        if runs.is_dir() {
            for entry in std::fs::read_dir(&runs)
                .with_context(|| format!("cannot read {}", runs.display()))?
            {
                let name = entry?.file_name().to_string_lossy().into_owned();
                let Some(sequence) = name.strip_prefix(&prefix) else {
                    continue;
                };
                if let Ok(value) = sequence.parse::<usize>() {
                    highest = highest.max(value);
                }
            }
        }

        Ok(format!("{prefix}{:03}", highest + 1))
    }
}

/// Create `.kage/` with a starter config. Returns the path to the config that was written.
pub fn init(root: &Path, force: bool) -> Result<PathBuf> {
    let kage_dir = root.join(KAGE_DIR);
    let config_path = kage_dir.join(CONFIG_FILE);

    if config_path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            config_path.display()
        );
    }

    for sub in ["runs", "artifacts", "state", "worktrees"] {
        let dir = kage_dir.join(sub);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }

    // Worktrees are disposable checkouts; committing them would be nonsense. Runs and artifacts are
    // deliberately left tracked — a plan and a review are engineering records worth keeping.
    std::fs::write(kage_dir.join(".gitignore"), "worktrees/\nruns/*/logs/\n")
        .context("cannot write .kage/.gitignore")?;

    std::fs::write(&config_path, starter_config(root))
        .with_context(|| format!("cannot write {}", config_path.display()))?;

    Ok(config_path)
}

/// The config `kage init` writes.
///
/// Hand-written rather than serialized from `Config::default()` so it can carry comments — the
/// file is the primary documentation of what is configurable.
fn starter_config(root: &Path) -> String {
    let commands = detect_validation_commands(root);
    let rendered = if commands.is_empty() {
        "  # Kage could not detect a toolchain. Add the commands that prove the work is correct,\n\
         \x20 # for example:\n\
         \x20 #   - cargo test\n\
         \x20 #   - cargo clippy -- -D warnings\n\
         \x20 commands: []\n"
            .to_string()
    } else {
        let lines = commands
            .iter()
            .map(|command| format!("    - {command}\n"))
            .collect::<String>();
        format!("  commands:\n{lines}")
    };

    format!(
        "# Kage configuration\n\
         #\n\
         # Roles are bound to harnesses here; the workflow itself never names a model.\n\
         # Adapters: claude-code | codex | opencode | kamui | command\n\
         version: 1\n\
         \n\
         # Leave `model` unset to use whatever each harness is already configured with.\n\
         # A name the harness does not recognise fails the run, so only set one you have\n\
         # verified with that tool (`claude --model`, `codex -m`, `opencode -m`).\n\
         roles:\n\
         \x20 planner:\n\
         \x20   adapter: claude-code\n\
         \x20   # model: opus\n\
         \n\
         \x20 executor:\n\
         \x20   adapter: opencode\n\
         \n\
         \x20 reviewer:\n\
         \x20   adapter: codex\n\
         \n\
         # Any binary that takes a prompt can fill a role:\n\
         #\n\
         #   executor:\n\
         #     adapter: command\n\
         #     command: [\"my-agent\", \"--yolo\", \"{{prompt}}\"]\n\
         \n\
         loop:\n\
         \x20 # Fix attempts allowed after the first failing review.\n\
         \x20 max_iterations: 3\n\
         \n\
         git:\n\
         \x20 # Run agents in a throwaway worktree instead of your working tree.\n\
         \x20 isolate: true\n\
         \x20 # base: main\n\
         \n\
         # Exit codes, not agent self-reports, decide whether execution succeeded.\n\
         validation:\n\
         {rendered}\
         \x20 timeout_secs: 900\n"
    )
}

/// Guess validation commands from the files in the repository root.
///
/// A wrong guess is cheap (the user edits one line) but an empty `validation:` block is a trap:
/// the loop would accept any change that compiles in the agent's imagination.
fn detect_validation_commands(root: &Path) -> Vec<String> {
    if root.join("Cargo.toml").is_file() {
        return vec![
            "cargo test".to_string(),
            "cargo clippy --all-targets -- -D warnings".to_string(),
            "cargo fmt --check".to_string(),
        ];
    }
    if root.join("package.json").is_file() {
        let manager = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        return vec![format!("{manager} test"), format!("{manager} run lint")];
    }
    if root.join("go.mod").is_file() {
        return vec!["go test ./...".to_string(), "go vet ./...".to_string()];
    }
    if root.join("pyproject.toml").is_file() {
        return vec!["pytest".to_string()];
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kage-config-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_writes_a_config_that_parses_back() {
        let root = temp_dir("init");
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();

        let path = init(&root, false).unwrap();
        let config: Config = serde_yaml_ng::from_str(&std::fs::read_to_string(&path).unwrap())
            .expect("the generated starter config must be valid");

        assert_eq!(config.roles.planner.adapter, AdapterKind::ClaudeCode);
        assert!(config.validation.commands.iter().any(|c| c == "cargo test"));
        assert!(root.join(".kage/runs").is_dir());
        assert!(root.join(".kage/worktrees").is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn init_refuses_to_clobber_an_existing_config() {
        let root = temp_dir("clobber");
        init(&root, false).unwrap();
        std::fs::write(root.join(".kage/config.yaml"), "version: 1\n").unwrap();

        assert!(init(&root, false).is_err());
        init(&root, true).expect("--force overwrites");
        assert!(
            std::fs::read_to_string(root.join(".kage/config.yaml"))
                .unwrap()
                .contains("roles:")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_ids_increment_within_a_day() {
        let root = temp_dir("runids");
        init(&root, false).unwrap();
        let project = Project::discover(&root).unwrap();
        let now = chrono::Local::now();

        let first = project.next_run_id(now).unwrap();
        std::fs::create_dir_all(project.run_dir(&first)).unwrap();
        let second = project.next_run_id(now).unwrap();

        assert!(first.ends_with("_001"), "got {first}");
        assert!(second.ends_with("_002"), "got {second}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_finds_the_project_from_a_subdirectory() {
        let root = temp_dir("discover");
        init(&root, false).unwrap();
        let nested = root.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();

        let project = Project::discover(&nested).unwrap();

        assert_eq!(
            project.root.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
