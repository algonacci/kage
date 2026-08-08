//! Spawning child processes with capture, streaming, and timeouts.
//!
//! Every external program Kage runs — coding agents, validation commands, git — goes through here,
//! so timeout and logging behaviour is identical for all of them.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// What came out of a child process.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `None` when the process was killed by a signal or by our timeout.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration: Duration,
}

impl Outcome {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// A short human explanation of how the process ended.
    pub fn describe(&self) -> String {
        if self.timed_out {
            return format!("timed out after {}s", self.duration.as_secs());
        }
        match self.code {
            Some(0) => format!("ok in {}s", self.duration.as_secs()),
            Some(code) => format!("exit {code} after {}s", self.duration.as_secs()),
            None => "killed".to_string(),
        }
    }

    /// stderr when there is any, else stdout — whichever is likelier to explain a failure.
    pub fn failure_output(&self) -> &str {
        if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        }
    }
}

/// How a child process should be built.
pub struct Spawn {
    pub program: String,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    /// A complete command line appended verbatim, bypassing per-argument escaping.
    ///
    /// Needed for `cmd /C`: Windows escapes each argument independently, so the quotes in
    /// `python -c "assert x == 'y'"` are mangled before cmd.exe ever sees them. Passing the line
    /// raw is the only way to keep a shell command intact.
    pub raw_command: Option<String>,
    pub timeout: Duration,
    /// Prefix for echoing the child's output to the terminal. `None` runs quietly.
    pub stream_prefix: Option<String>,
    /// Combined stdout+stderr transcript, written even if the process is killed.
    pub log_path: Option<PathBuf>,
}

/// Run a process to completion, capturing output and enforcing a timeout.
///
/// Output is read on background tasks rather than with `wait_with_output`, because a child that
/// fills its pipe buffer while we block on `wait` would deadlock — and an agent that prints a whole
/// file diff will fill a 64k pipe long before it exits.
pub async fn run(spawn: Spawn) -> Result<Outcome> {
    let started = Instant::now();

    let resolved = resolve_program(&spawn.program)
        .with_context(|| format!("`{}` not found on PATH", spawn.program))?;

    let mut command = Command::new(&resolved.program);
    command
        .args(&resolved.prefix_args)
        .args(&spawn.args)
        .current_dir(&spawn.workdir)
        .stdin(if spawn.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(raw) = &spawn.raw_command {
        append_raw(&mut command, raw);
    }

    for (key, value) in &spawn.env {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("cannot spawn `{}`", spawn.program))?;

    if let Some(input) = spawn.stdin {
        let mut handle = child
            .stdin
            .take()
            .context("stdin was piped but unavailable")?;
        handle
            .write_all(input.as_bytes())
            .await
            .context("cannot write to child stdin")?;
        // Dropping the handle closes the pipe; without this the child waits forever for EOF.
        drop(handle);
    }

    let stdout = child.stdout.take().context("stdout unavailable")?;
    let stderr = child.stderr.take().context("stderr unavailable")?;

    let out_prefix = spawn.stream_prefix.clone();
    let err_prefix = spawn.stream_prefix.clone();
    let stdout_task = tokio::spawn(async move { drain(stdout, out_prefix).await });
    let stderr_task = tokio::spawn(async move { drain(stderr, err_prefix).await });

    let mut timed_out = false;
    let status = match tokio::time::timeout(spawn.timeout, child.wait()).await {
        Ok(status) => status.context("cannot wait for child process")?,
        Err(_) => {
            timed_out = true;
            // Kill, then wait: the reader tasks only finish once the pipes close, which only
            // happens after the process is actually gone.
            let _ = child.start_kill();
            child.wait().await.context("cannot reap timed-out child")?
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    if let Some(path) = &spawn.log_path {
        write_log(path, &spawn.program, &spawn.args, &stdout, &stderr)?;
    }

    Ok(Outcome {
        code: status.code(),
        stdout,
        stderr,
        timed_out,
        duration: started.elapsed(),
    })
}

/// Append a command line without the usual per-argument quoting.
///
/// `cmd /C "the whole line"` makes cmd.exe strip the outer quotes and run the remainder verbatim,
/// which is what preserves quoting inside the command. On other platforms `sh -c` already takes the
/// command as one ordinary argument, so no special handling is needed.
fn append_raw(command: &mut Command, raw: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().raw_arg(format!("\"{raw}\""));
    }

    #[cfg(not(windows))]
    {
        command.arg(raw);
    }
}

/// Read a pipe to end-of-file, optionally echoing each line as it arrives.
///
/// Streaming matters for a loop that can run for half an hour: without it the user stares at a
/// frozen terminal with no way to tell a working agent from a hung one.
async fn drain<R>(reader: R, prefix: Option<String>) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut collected = String::new();

    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(prefix) = &prefix {
            println!("{prefix} {line}");
        }
        collected.push_str(&line);
        collected.push('\n');
    }

    collected
}

fn write_log(
    path: &Path,
    program: &str,
    args: &[String],
    stdout: &str,
    stderr: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let body = format!(
        "$ {program} {}\n\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        args.join(" ")
    );
    std::fs::write(path, body).with_context(|| format!("cannot write {}", path.display()))
}

/// A program resolved to something the OS can actually execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub program: String,
    /// Arguments that must precede the caller's own — non-empty only for the `cmd /C` shim case.
    pub prefix_args: Vec<String>,
    /// The file that was found, for reporting in `kage doctor`.
    pub path: PathBuf,
}

/// Find `program` on PATH, handling Windows batch shims.
///
/// Rust's `Command` calls `CreateProcess`, which only knows how to execute real binaries. Node-based
/// CLIs install on Windows as `claude.cmd` / `opencode.cmd` batch shims, and handing one of those to
/// `CreateProcess` fails with "%1 is not a valid Win32 application". Those must be run through
/// `cmd /C` instead, which is why resolution returns prefix arguments rather than just a path.
pub fn resolve_program(program: &str) -> Result<Resolved> {
    let direct = Path::new(program);
    let found = if direct.components().count() > 1 || direct.is_absolute() {
        direct
            .is_file()
            .then(|| direct.to_path_buf())
            .with_context(|| format!("{} is not a file", direct.display()))?
    } else {
        search_path(program).with_context(|| format!("`{program}` is not on PATH"))?
    };

    Ok(shim_for(found))
}

fn shim_for(found: PathBuf) -> Resolved {
    let is_batch = found
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));

    if is_batch {
        Resolved {
            program: "cmd".to_string(),
            // `/C` runs the command and exits. The path is passed as one argument so spaces in
            // `C:\Program Files\...` survive.
            prefix_args: vec!["/C".to_string(), found.to_string_lossy().into_owned()],
            path: found,
        }
    } else {
        Resolved {
            program: found.to_string_lossy().into_owned(),
            prefix_args: Vec::new(),
            path: found,
        }
    }
}

/// Look up a bare program name across PATH, trying each PATHEXT extension on Windows.
fn search_path(program: &str) -> Option<PathBuf> {
    search_in(program, &std::env::var_os("PATH")?)
}

/// The lookup itself, over an explicit search path so it can be tested without touching the
/// process environment — mutating `PATH` would corrupt every other test running in parallel.
fn search_in(program: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let extensions = executable_extensions();

    for dir in std::env::split_paths(path) {
        // Extensions come first on Windows. npm installs both `codex` (a POSIX shell script for
        // Git Bash) and `codex.cmd` side by side; the extensionless one is not executable by
        // CreateProcess, so preferring it would break every spawn of an npm-installed harness.
        for extension in &extensions {
            let candidate = dir.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        let direct = dir.join(program);
        if direct.is_file() {
            return Some(direct);
        }
    }

    None
}

/// Extensions that make a file executable on this platform.
fn executable_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }

    // PATHEXT holds `.COM;.EXE;.BAT;.CMD;...`. Falling back to a literal list keeps resolution
    // working on a stripped-down environment where the variable is unset.
    std::env::var("PATHEXT")
        .map(|raw| {
            raw.split(';')
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|_| {
            [".COM", ".EXE", ".BAT", ".CMD"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

/// Whether the external `rtk` binary is available. Detected once per process; RTK is an optional
/// output-compression backend, never a requirement.
pub fn rtk_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(resolved) = resolve_program("rtk") else {
            return false;
        };
        std::process::Command::new(&resolved.program)
            .args(&resolved.prefix_args)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Decide whether to route a command through `rtk`.
///
/// Validation output is not just printed — it lands in TEST_RESULTS.md and from there into the
/// fixer's and reviewer's prompts, where a full `cargo test` log crowds out the plan. Compressing it
/// buys context back for the parts that matter.
///
/// Only simple commands are routed: with a shell operator the prefix would apply to the first
/// segment alone, quietly changing what runs. A command the user already prefixed is left as is.
pub fn route_through_rtk(command: &str, rtk_is_available: bool) -> bool {
    if !rtk_is_available {
        return false;
    }

    let trimmed = command.trim();
    if trimmed == "rtk" || trimmed.starts_with("rtk ") {
        return false;
    }

    const SHELL_OPERATORS: [char; 9] = ['&', '|', ';', '>', '<', '`', '$', '(', '\n'];
    !trimmed.is_empty() && !trimmed.contains(SHELL_OPERATORS)
}

/// Wrap a shell command string so the platform shell parses it.
///
/// Validation commands come from config as one line (`cargo clippy -- -D warnings`) and may use
/// shell features, so they are handed to a shell rather than split by Kage.
pub fn shell_spawn(command: &str, workdir: PathBuf, timeout: Duration) -> Spawn {
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };

    Spawn {
        program: program.to_string(),
        args: vec![flag.to_string()],
        workdir,
        env: Vec::new(),
        stdin: None,
        // The command itself goes through `raw_command` so its quoting survives.
        raw_command: Some(command.to_string()),
        timeout,
        stream_prefix: None,
        log_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(program: &str, args: &[&str]) -> Spawn {
        Spawn {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            workdir: std::env::temp_dir(),
            env: Vec::new(),
            stdin: None,
            raw_command: None,
            timeout: Duration::from_secs(30),
            stream_prefix: None,
            log_path: None,
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let outcome = run(shell_spawn(
            "echo kage-marker",
            std::env::temp_dir(),
            Duration::from_secs(30),
        ))
        .await
        .unwrap();

        assert!(outcome.success(), "{outcome:?}");
        assert!(outcome.stdout.contains("kage-marker"));
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_reported_not_raised() {
        let outcome = run(shell_spawn(
            "exit 3",
            std::env::temp_dir(),
            Duration::from_secs(30),
        ))
        .await
        .unwrap();

        assert!(!outcome.success());
        assert_eq!(outcome.code, Some(3));
        assert!(outcome.describe().contains("exit 3"));
    }

    #[tokio::test]
    async fn a_hung_process_is_killed_at_the_timeout() {
        // `ping` is the portable way to block for a fixed time on Windows without a console.
        let command = if cfg!(windows) {
            "ping -n 60 127.0.0.1"
        } else {
            "sleep 60"
        };
        let mut spawn = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        spawn.timeout = Duration::from_millis(600);

        let outcome = run(spawn).await.unwrap();

        assert!(outcome.timed_out);
        assert!(!outcome.success());
        assert!(outcome.describe().contains("timed out"));
    }

    #[tokio::test]
    async fn stdin_is_delivered_and_closed() {
        // Without closing the pipe the reader below would block forever waiting for EOF.
        let program = if cfg!(windows) { "findstr" } else { "cat" };
        let args: &[&str] = if cfg!(windows) { &["kage"] } else { &[] };
        let mut spawn = spawn(program, args);
        spawn.stdin = Some("kage-from-stdin\n".to_string());

        let outcome = run(spawn).await.unwrap();

        assert!(outcome.stdout.contains("kage-from-stdin"), "{outcome:?}");
    }

    #[tokio::test]
    async fn output_is_written_to_the_log_file() {
        let log = std::env::temp_dir().join(format!("kage-proc-log-{}.txt", std::process::id()));
        let mut spawn = shell_spawn(
            "echo logged-line",
            std::env::temp_dir(),
            Duration::from_secs(30),
        );
        spawn.log_path = Some(log.clone());

        run(spawn).await.unwrap();

        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .contains("logged-line")
        );
        let _ = std::fs::remove_file(&log);
    }

    #[tokio::test]
    async fn quotes_inside_a_shell_command_survive() {
        // Regression guard: per-argument escaping used to split this into `\"assert` and the rest,
        // so every validation command containing quotes failed with a syntax error.
        let outcome = run(shell_spawn(
            "python -c \"print('inner-quotes-intact')\"",
            std::env::temp_dir(),
            Duration::from_secs(60),
        ))
        .await
        .unwrap();

        assert!(outcome.success(), "{outcome:?}");
        assert!(
            outcome.stdout.contains("inner-quotes-intact"),
            "{outcome:?}"
        );
    }

    #[test]
    fn only_simple_commands_are_routed_through_rtk() {
        assert!(route_through_rtk("cargo test", true));
        assert!(route_through_rtk("  cargo clippy --all-targets  ", true));

        // Never without rtk installed.
        assert!(!route_through_rtk("cargo test", false));
        // Never double-prefixed.
        assert!(!route_through_rtk("rtk cargo test", true));
        assert!(!route_through_rtk("rtk", true));
        // A shell operator would leave the prefix applied to the first segment only, silently
        // changing which command the compression covers.
        assert!(!route_through_rtk("cargo build && cargo test", true));
        assert!(!route_through_rtk("cargo test | tail -5", true));
        assert!(!route_through_rtk("pytest > out.txt", true));
        assert!(!route_through_rtk("echo $HOME", true));
        assert!(!route_through_rtk("python -c \"assert (1)\"", true));
    }

    #[test]
    fn a_missing_program_is_an_error_rather_than_a_panic() {
        assert!(resolve_program("kage-definitely-not-installed").is_err());
    }

    #[test]
    #[cfg(windows)]
    fn an_executable_extension_wins_over_a_bare_shell_script() {
        // npm drops both `x` (a POSIX script) and `x.cmd` into the same directory. Only the second
        // one can actually be spawned on Windows.
        let dir = std::env::temp_dir().join(format!("kage-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kagefake"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("kagefake.cmd"), "@echo off\n").unwrap();

        let search = std::env::join_paths([dir.clone()]).unwrap();
        let found = search_in("kagefake", &search);

        // PATHEXT is spelled in uppercase, so the found name is `kagefake.CMD`.
        let extension = found
            .unwrap()
            .extension()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(extension.eq_ignore_ascii_case("cmd"), "got {extension}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_shims_are_routed_through_cmd() {
        let shim = shim_for(PathBuf::from(r"C:\Program Files\nodejs\claude.CMD"));

        assert_eq!(shim.program, "cmd");
        assert_eq!(shim.prefix_args[0], "/C");
        assert!(shim.prefix_args[1].ends_with("claude.CMD"));
    }

    #[test]
    fn real_binaries_are_executed_directly() {
        let resolved = shim_for(PathBuf::from("/usr/bin/git"));

        assert!(resolved.prefix_args.is_empty());
        assert_eq!(resolved.program, "/usr/bin/git");
    }
}
