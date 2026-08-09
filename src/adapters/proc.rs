//! Spawning child processes with capture, streaming, and timeouts.
//!
//! Every external program Kage runs — coding agents, validation commands, git — goes through here,
//! so timeout and logging behaviour is identical for all of them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::MissedTickBehavior;

use crate::adapters::stream;

/// What came out of a child process.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `None` when the process was killed by a signal or by our timeout.
    pub code: Option<i32>,
    /// What the child said, rendered — for a streaming harness this is the assistant's prose and
    /// final message, not the JSON envelope. `gates::read` searches it for a verdict.
    pub stdout: String,
    /// Always raw: harness errors are plain text and are never JSON-decoded.
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
    /// How to interpret stdout. A streaming harness emits JSON events; rendering them is what keeps
    /// the terminal readable and keeps `Outcome.stdout` searchable for a verdict.
    pub stdout_format: stream::OutputFormat,
    /// How often to print an elapsed-time line while the child is silent. `None` prints none.
    pub heartbeat: Option<Duration>,
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

    // The transcript must exist before the child has produced a single line, or it is empty for
    // exactly as long as somebody wants to read it. Creation itself is checked — a bad path is a
    // configuration error worth failing on up front; everything after it is best effort.
    let log = match &spawn.log_path {
        Some(path) => Some(Arc::new(Mutex::new(LogWriter::create(
            path,
            &spawn.program,
            &spawn.args,
        )?))),
        None => None,
    };

    // Milliseconds since the run started when output last arrived. The heartbeat reads it; the
    // drains publish it. It is shared rather than pushed through a channel because tokio's `sync`
    // feature is not enabled and must not be added for the one producer value it saves.
    let last_output = Arc::new(AtomicU64::new(0));

    let out_prefix = spawn.stream_prefix.clone();
    let err_prefix = spawn.stream_prefix.clone();
    let stdout_log = log.clone();
    let stderr_log = log.clone();
    let stdout_last = last_output.clone();
    let stderr_last = last_output.clone();

    let stdout_task = tokio::spawn(async move {
        drain(
            stdout,
            Drain {
                prefix: out_prefix,
                format: spawn.stdout_format,
                log_tag: "",
                log: stdout_log,
                started,
                last_output: stdout_last,
            },
        )
        .await
    });
    let stderr_task = tokio::spawn(async move {
        drain(
            stderr,
            Drain {
                prefix: err_prefix,
                // Stderr of every harness is human error text, never JSON events.
                format: stream::OutputFormat::Passthrough,
                log_tag: "[stderr] ",
                log: stderr_log,
                started,
                last_output: stderr_last,
            },
        )
        .await
    });

    let heartbeat = spawn.heartbeat.map(|interval| {
        let prefix = spawn
            .stream_prefix
            .clone()
            .unwrap_or_else(|| "  ".to_string());
        let heartbeat_last = last_output.clone();
        let heartbeat_started = started;
        let timeout = spawn.timeout;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // A starved loop must not burst a backlog of ticks into the terminal at once.
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The very first tick of a new interval completes immediately; without this skip an
            // instant heartbeat would print at t=0.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let elapsed = heartbeat_started.elapsed();
                let last_output = Duration::from_millis(heartbeat_last.load(Ordering::Relaxed));
                if heartbeat_due(elapsed, last_output, interval) {
                    println!("{}", heartbeat_line(&prefix, elapsed, timeout));
                }
            }
        })
    });

    let mut timed_out = false;
    let status = match tokio::time::timeout(spawn.timeout, child.wait()).await {
        Ok(status) => Some(status.context("cannot wait for child process")?),
        Err(_) => {
            timed_out = true;

            // Everything past this point is bounded, because none of it is worth waiting on. A
            // timeout is a decision already made: the child has spent its budget and its output
            // will be discarded either way. Leaving the kill and the reap unbounded turned a
            // one-hour budget into a three-hour-twenty-two-minute run — the deadline fired on
            // time and then Kage waited for a process that had already been told to die.
            //
            // Killing the tree rather than the child is separate and still necessary: coding-agent
            // CLIs spawn helpers that inherit the stdout pipe, and while those live the drains
            // never see EOF.
            let _ = tokio::time::timeout(KILL_GRACE, kill_tree(&mut child)).await;

            match tokio::time::timeout(REAP_GRACE, child.wait()).await {
                Ok(status) => Some(status.context("cannot reap timed-out child")?),
                // Unreapable. The exit status is unknown and stays that way; `timed_out` is what
                // the caller acts on, and pretending to a code would be worse than admitting none.
                Err(_) => None,
            }
        }
    };

    // No heartbeat may land after the phase summary; whichever way we got here, the child's fate
    // is decided and the clock no longer means anything to the user.
    if let Some(handle) = heartbeat {
        handle.abort();
    }

    // After a kill, a straggler that survived the tree-kill can still hold the pipe open; give
    // the drains a short grace and then abandon them rather than pinning the run indefinitely.
    // The live log has already recorded every line as it arrived, so nothing durable is lost —
    // only the in-memory capture is cut short, on a run whose outcome is already "timed out".
    let (stdout, stderr) = if timed_out {
        let stdout = tokio::time::timeout(DRAIN_GRACE, stdout_task)
            .await
            .ok()
            .and_then(|joined| joined.ok())
            .unwrap_or_default();
        let stderr = tokio::time::timeout(DRAIN_GRACE, stderr_task)
            .await
            .ok()
            .and_then(|joined| joined.ok())
            .unwrap_or_default();
        (stdout, stderr)
    } else {
        (
            stdout_task.await.unwrap_or_default(),
            stderr_task.await.unwrap_or_default(),
        )
    };

    if let Some(log) = &log {
        let finished = Outcome {
            code: status.and_then(|status| status.code()),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            timed_out,
            duration: started.elapsed(),
        }
        .describe();
        lock_log(log, |writer| writer.finish(&finished));
    }

    Ok(Outcome {
        code: status.and_then(|status| status.code()),
        stdout,
        stderr,
        timed_out,
        duration: started.elapsed(),
    })
}

/// The lock on the shared transcript, released on every body exit.
///
/// Holds are never kept across an `.await` (clippy's `await_holding_lock` is denied by CI). A
/// poisoned mutex is still worth writing to — a panicked drain must not lose the rest of the log.
fn lock_log<T>(log: &Arc<Mutex<LogWriter>>, body: impl FnOnce(&mut LogWriter) -> T) -> T {
    match log.lock() {
        Ok(mut guard) => body(&mut guard),
        Err(poisoned) => body(&mut poisoned.into_inner()),
    }
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

/// Everything one drain task needs to turn pipe lines into terminal lines, log lines, and capture.
struct Drain {
    prefix: Option<String>,
    format: stream::OutputFormat,
    log_tag: &'static str,
    log: Option<Arc<Mutex<LogWriter>>>,
    started: Instant,
    last_output: Arc<AtomicU64>,
}

/// Read a pipe to end-of-file, logging, rendering, and accumulating each line as it arrives.
///
/// Streaming matters for a loop that can run for half an hour: without it the user stares at a
/// frozen terminal with no way to tell a working agent from a hung one. Every line is flushed to
/// the log immediately, so `tail -f` shows progress in real time rather than at exit.
async fn drain<R>(reader: R, ctx: Drain) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut collected = String::new();

    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(log) = &ctx.log {
            // The transcript keeps the *raw* line, never the rendered one — it is the record of
            // exactly what the harness emitted, which is the first thing wanted when it misbehaves.
            lock_log(log, |writer| writer.line(ctx.log_tag, &line));
        }

        let rendered = stream::render(ctx.format, &line);
        if let Some(prefix) = &ctx.prefix {
            for display in &rendered.display {
                println!("{prefix} {display}");
            }
        }
        if let Some(captured) = rendered.captured {
            collected.push_str(&captured);
            collected.push('\n');
        }

        ctx.last_output
            .store(ctx.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    collected
}

/// The transcript, written as it arrives rather than at the end.
///
/// A log that only appears after the child exits is empty for exactly as long as somebody wants to
/// read it — the whole planning phase. Every line is flushed so `tail -f` shows it immediately.
struct LogWriter {
    file: std::fs::File,
}

impl LogWriter {
    /// Create (or truncate) the transcript and write its header.
    ///
    /// Truncating is deliberate: a resumed phase re-enters and its log must describe *this*
    /// attempt, matching "every phase overwrites its artifact".
    fn create(path: &Path, program: &str, args: &[String]) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }

        let mut file = std::fs::File::create(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let header = format!("$ {program} {}\n\n", args.join(" "));
        file.write_all(header.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("cannot flush {}", path.display()))?;

        Ok(Self { file })
    }

    /// Append one line. Best effort: a failing write must not kill a working agent, and there is
    /// no recovery mid-stream for one that does.
    fn line(&mut self, tag: &str, text: &str) {
        let _ = writeln!(self.file, "{tag}{text}");
        let _ = self.file.flush();
    }

    /// Record how the process ended, so a truncated log is distinguishable from a killed child.
    fn finish(&mut self, description: &str) {
        let _ = writeln!(self.file, "\n--- {description} ---\n");
        let _ = self.file.flush();
    }
}

/// How often a silent child is reported as still alive.
///
/// Long enough not to clutter a chatty run, short enough that a user does not conclude the terminal
/// is frozen while the agent thinks.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How long the drains may keep reading after a timed-out child was killed.
///
/// Long enough for the tree-kill to close the pipes and the drains to flush; short enough that the
/// one straggler taskkill missed cannot turn a 30-minute timeout into an 80-minute wait.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// How long the kill itself may take.
///
/// `taskkill` walks a process tree and normally returns in milliseconds; if it has not returned by
/// now it is not going to, and waiting on it is what a timeout exists to prevent.
const KILL_GRACE: Duration = Duration::from_secs(15);

/// How long to wait for a killed child to actually die before giving up on its exit status.
///
/// A process can survive `/F` — a driver-blocked write, a debugger attached, a stuck NTFS handle.
/// The run is over either way, so the wait is bounded and the status simply becomes unknown.
const REAP_GRACE: Duration = Duration::from_secs(20);

/// Kill a timed-out child and, on Windows, its whole process tree.
///
/// `taskkill /T` takes down the grandchildren that inherited the stdout pipe, which is what lets
/// the drain tasks reach EOF promptly. `start_kill` stays as the fallback for a taskkill that is
/// missing or fails — and as the whole story on Unix, where the drain grace bounds the wait
/// instead.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    let _ = child.start_kill();
}

/// Whether a tick should print, given how long the child has been quiet.
///
/// The child is quiet for `elapsed - last_output`; enough unanswered ticks mean a pulse is owed,
/// whatever the child is doing — including printing nothing at all.
fn heartbeat_due(elapsed: Duration, last_output: Duration, interval: Duration) -> bool {
    elapsed >= last_output + interval
}

/// `  [planner] still working — 4m12s elapsed of 30m00s`
fn heartbeat_line(prefix: &str, elapsed: Duration, timeout: Duration) -> String {
    format!(
        "{prefix} still working — {} elapsed of {}",
        stream::brief_duration(elapsed),
        stream::brief_duration(timeout)
    )
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
        stdout_format: stream::OutputFormat::Passthrough,
        heartbeat: None,
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
            stdout_format: stream::OutputFormat::Passthrough,
            heartbeat: None,
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
    async fn a_timeout_bounds_the_whole_run_and_not_just_the_wait() {
        // The bug this guards: the deadline fired on time, then the kill and the reap after it were
        // both unbounded, so a one-hour executor budget produced a three-hour-twenty-two-minute run.
        // Everything after the deadline must be bounded, because a timeout is a decision already
        // made and nothing past it is worth waiting for.
        let command = if cfg!(windows) {
            "start /B ping -n 120 127.0.0.1 & ping -n 120 127.0.0.1"
        } else {
            "sleep 120 & sleep 120"
        };
        let mut spawn = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        spawn.timeout = Duration::from_millis(500);

        let started = std::time::Instant::now();
        let outcome = run(spawn).await.unwrap();
        let elapsed = started.elapsed();

        assert!(outcome.timed_out);

        // The deadline plus every grace that may legitimately follow it, and nothing more.
        let ceiling = Duration::from_millis(500) + KILL_GRACE + REAP_GRACE + DRAIN_GRACE * 2;
        assert!(
            elapsed < ceiling,
            "a timed-out run ran for {elapsed:?}, past its own ceiling of {ceiling:?}"
        );
    }

    #[tokio::test]
    async fn a_grandchild_holding_the_pipe_cannot_pin_a_timed_out_run() {
        // The bug this guards: coding-agent CLIs spawn helpers that inherit the stdout pipe.
        // Killing only the direct child left those helpers alive, the drains waited on their pipe
        // for EOF, and a run that timed out at 30 minutes sat open for another 49. The tree-kill
        // plus the drain grace must bound the wait regardless of what survives.
        let command = if cfg!(windows) {
            // `start /B` detaches a grandchild that inherits our pipe handles.
            "start /B ping -n 90 127.0.0.1 & ping -n 90 127.0.0.1"
        } else {
            // The backgrounded sleep inherits stdout and outlives its parent shell.
            "sleep 90 & sleep 90"
        };
        let mut spawn = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        spawn.timeout = Duration::from_millis(600);

        let started = std::time::Instant::now();
        let outcome = run(spawn).await.unwrap();

        assert!(outcome.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the drain waited on a straggler's pipe for {:?}",
            started.elapsed()
        );
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

    #[tokio::test]
    async fn the_log_file_is_readable_while_the_child_is_still_running() {
        // Regression guard for the bug this change closes: the log used to be written in one shot
        // only after the child exited, so it was empty for the entire planning phase somebody
        // wanted to inspect. The marker must be on disk while the child is still alive.
        let log = std::env::temp_dir().join(format!("kage-proc-live-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);

        // Echo a marker, then block for ~2s (`ping` is the portable way to block on Windows).
        let command = if cfg!(windows) {
            "echo live-marker & ping -n 3 127.0.0.1"
        } else {
            "echo live-marker; sleep 2"
        };
        let mut spawn = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        spawn.log_path = Some(log.clone());

        let child = tokio::spawn(run(spawn));

        let marker_found = tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&log)
                    && text.contains("live-marker")
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;

        assert_eq!(
            marker_found,
            Ok(true),
            "the marker must appear before the child exits"
        );

        let outcome = child.await.unwrap().unwrap();
        assert!(outcome.success(), "{outcome:?}");
        let _ = std::fs::remove_file(&log);
    }

    #[tokio::test]
    async fn the_log_keeps_stdout_and_stderr_apart() {
        let log = std::env::temp_dir().join(format!("kage-proc-both-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);

        let command = if cfg!(windows) {
            "echo stdout-marker & echo stderr-marker 1>&2"
        } else {
            "echo stdout-marker; echo stderr-marker 1>&2"
        };
        let mut spawn = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        spawn.log_path = Some(log.clone());

        run(spawn).await.unwrap();

        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("stdout-marker"), "{log_text}");
        // The stderr tag keeps the two streams' arrival order without making stdout un-greppable.
        assert!(log_text.contains("[stderr] stderr-marker"), "{log_text}");
        let _ = std::fs::remove_file(&log);
    }

    #[tokio::test]
    async fn the_log_records_how_the_process_ended() {
        let ok_log = std::env::temp_dir().join(format!("kage-proc-ok-{}.log", std::process::id()));
        let mut spawn = shell_spawn("exit 0", std::env::temp_dir(), Duration::from_secs(30));
        spawn.log_path = Some(ok_log.clone());

        run(spawn).await.unwrap();

        assert!(
            std::fs::read_to_string(&ok_log).unwrap().contains("ok in"),
            "a finished run's log must say so"
        );
        let _ = std::fs::remove_file(&ok_log);

        let timed_log =
            std::env::temp_dir().join(format!("kage-proc-timed-{}.log", std::process::id()));
        let command = if cfg!(windows) {
            "ping -n 60 127.0.0.1"
        } else {
            "sleep 60"
        };
        let mut spawn = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        spawn.timeout = Duration::from_millis(500);
        spawn.log_path = Some(timed_log.clone());

        run(spawn).await.unwrap();

        assert!(
            std::fs::read_to_string(&timed_log)
                .unwrap()
                .contains("timed out"),
            "a timed-out run's log must say so"
        );
        let _ = std::fs::remove_file(&timed_log);
    }

    #[tokio::test]
    async fn a_heartbeat_does_not_disturb_the_captured_output() {
        let command = "echo heartbeat-does-not-echo";
        let plain = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        let mut beating = shell_spawn(command, std::env::temp_dir(), Duration::from_secs(30));
        beating.heartbeat = Some(Duration::from_millis(50));

        let plain = run(plain).await.unwrap();
        let beating = run(beating).await.unwrap();

        assert_eq!(beating.stdout, plain.stdout);
        assert_eq!(beating.code, plain.code);
        assert_eq!(beating.success(), plain.success());
    }

    #[test]
    fn a_silent_child_is_reported_as_still_working() {
        assert!(heartbeat_due(
            Duration::from_secs(60),
            Duration::from_secs(0),
            Duration::from_secs(30)
        ));
        assert!(heartbeat_due(
            Duration::from_secs(31),
            Duration::from_secs(1),
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn a_talking_child_gets_no_heartbeat() {
        assert!(!heartbeat_due(
            Duration::from_secs(60),
            Duration::from_secs(59),
            Duration::from_secs(30)
        ));
        assert!(!heartbeat_due(
            Duration::from_secs(29),
            Duration::from_secs(0),
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn the_heartbeat_names_the_elapsed_time_and_the_timeout() {
        let line = heartbeat_line(
            "  [planner]",
            Duration::from_secs(252),
            Duration::from_secs(1800),
        );

        assert!(line.contains("[planner]"), "{line}");
        assert!(line.contains("4m12s"), "{line}");
        assert!(line.contains("30m00s"), "{line}");
    }
}
