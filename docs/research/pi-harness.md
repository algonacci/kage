# Research: Pi / Ohmypi Harness Interface (`@oh-my-pi/pi-coding-agent`)

> **Branch:** `research/pi-harness` · **Ticket:** #4 (part of #2) · **Date:** 2026-08-31
> **Scope:** Exact CLI surface of `pi`/`omp` for Kage adapter integration — argv, prompt delivery, streaming, Windows shims, `AdapterKind` design, and migration path.

---

## 1. Summary

`pi` (npm `@earendil-works/pi-coding-agent` v0.84.4 locally; republished as `@oh-my-pi/pi-coding-agent` v18.0.11 with binary renamed to `omp`) is a Node-based coding-agent CLI that matches Kage's existing harness shape: a single binary that takes a prompt, runs tools autonomously, and exits. Its non-interactive entry point is `pi -p <prompt>` (alias `--print`), directly analogous to `claude --print` and `kamui -p`.

**Decision confirmed:** Add a single `AdapterKind::Pi` variant (serde `pi`, display `pi`) — not two variants for `pi` vs `ohmypi`/`omp`. The package rename is a distribution detail; the argv is identical and `proc::resolve_program` already handles binary-name aliasing via PATH lookup. Migration is two-phase: (1) immediate use via `adapter: command` escape hatch with `command: ["pi", "-p", "{prompt}"]`, then (2) native preset in `preset_template` / `program_for` / `default_extra_args`.

**Model hazard:** `pi --model` accepts fuzzy patterns (`opus`, `provider/id`, `model:thinking`) — a Kage `model:` string containing `:` or `/` is forwarded verbatim and interpreted by pi, which is correct but means Kage must not split or validate the model string.

---

## 2. Binary & Package Identity

### 2.1 `which pi` / `pi --version`

```
$ which pi
/Users/amalanfadil/.local/state/fnm_multishells/18880_1788099460362/bin/pi
  -> ../lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js

$ file $(which pi)
a /usr/bin/env node script text executable

$ pi --version
0.84.4

$ head -1 $(which pi)
#!/usr/bin/env node
import { createRequire as __piCreateRequire } from "node:module"; ...
```

- **Type:** ESM Node bundle (`dist/bundle/cli.js`), not a compiled binary. `process.title = APP_NAME`, `PI_CODING_AGENT=true`, `AI_AGENT=pi` set at startup.
- **Install:** `fnm` Node v24.16.0, installed as `@earendil-works/pi-coding-agent` (old scope). No `@oh-my-pi` prefix on this machine — the rename happened after v0.84.4.

### 2.2 npm package `@oh-my-pi/pi-coding-agent`

```
$ npm view @oh-my-pi/pi-coding-agent version
18.0.11

$ npm view @oh-my-pi/pi-coding-agent dist --json | jq .tarball
https://registry.npmjs.org/@oh-my-pi/pi-coding-agent/-/pi-coding-agent-18.0.11.tgz

$ cat package.json | jq '{name, version, bin}'
{"name": "@earendil-works/pi-coding-agent", "version": "0.84.4", "bin": {"pi": "dist/bundle/cli.js"}}
```

| Scope | Latest | Binary | Notes |
|-------|--------|--------|-------|
| `@earendil-works/pi-coding-agent` | 0.84.4 | `pi` | Installed locally; ESM bundle |
| `@oh-my-pi/pi-coding-agent` | 18.0.11 | `omp` | Current publish; `bin: omp` (see `omp --help`); `pi` retained as alias on some installs |

`omp` (v18) is the same codebase, renamed. Verified:

```
$ which omp
/opt/homebrew/bin/omp -> ../Cellar/omp/18.0.11/bin/omp   (Bun-compiled binary)

$ omp --version
omp/18.0.11

$ pi --help  ≡  omp --help   (modulo binary name in usage line)
```

**Implication for Kage:** `program_for` should return `"pi"` as the canonical program name. On machines with v18 only, `pi` may not be on PATH but `omp` is — users can override via `command: ["omp", "-p", "{prompt}"]` during the transition, or Kage could try both. The single-alias decision means Kage ships one preset (`Pi`) whose program is `pi`; the `command` escape hatch covers the `omp` spelling without a second variant.

---

## 3. `pi --help` — Full argv

```
Usage:
  pi [options] [--] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
  pi list                      List installed extensions
  pi config [-l]               Open TUI to enable/disable package resources
  pi auth <command>            Print credentials or check provider readiness
  pi <command> --help          Show help for subcommand
```

### 3.1 Options (complete)

| Flag | Alias | Value | Description | Kage-relevant |
|------|-------|-------|-------------|---------------|
| `--provider` | — | `<name>` | Provider name (default: `google`) | ✅ `--provider` |
| `--model` | — | `<pattern>` | Model pattern/ID; supports `provider/id` and `:thinking` suffix | ✅ `--model` |
| `--api-key` | — | `<key>` | API key (defaults to env vars) | — |
| `--system-prompt` | — | `<text>` | System prompt (default: coding assistant prompt) | — |
| `--append-system-prompt` | — | `<text>` | Append text/file contents to system prompt (repeatable) | ✅ `--append-system-prompt` |
| `--mode` | — | `text\|json\|rpc` | Output mode (default: `text`) | streaming |
| `--print` | `-p` | — | **Non-interactive: process prompt and exit** | ✅ **required** |
| `--continue` | `-c` | — | Continue previous session | — |
| `--resume` | `-r` | — | Select session to resume | — |
| `--session` | — | `<path\|id>` | Use specific session file / partial UUID | ✅ `--session` |
| `--session-id` | — | `<id>` | Use exact project session ID, creating if missing | — |
| `--fork` | — | `<path\|id>` | Fork session into new session | — |
| `--session-dir` | — | `<dir>` | Directory for session storage | — |
| `--no-session` | — | — | Don't save session (ephemeral) | — |
| `--name` | `-n` | `<name>` | Session display name | — |
| `--models` | — | `<patterns>` | Comma-separated model patterns for Ctrl+P cycling | — |
| `--no-tools` | `-nt` | — | Disable all tools (built-in + extension) | — |
| `--no-builtin-tools` | `-nbt` | — | Disable built-in tools, keep extension/custom | — |
| `--tools` | `-t` | `<tools>` | Comma-separated allowlist of tool names | ✅ `--tools` |
| `--exclude-tools` | `-xt` | `<tools>` | Comma-separated denylist | — |
| `--thinking` | — | `off\|minimal\|low\|medium\|high\|xhigh\|max` | Thinking level | — |
| `--extension` | `-e` | `<path>` | Load extension file (repeatable) | ✅ `-e` |
| `--no-extensions` | `-ne` | — | Disable extension discovery (explicit `-e` still works) | — |
| `--skill` | — | `<path>` | Load skill file/dir (repeatable) | — |
| `--no-skills` | `-ns` | — | Disable skills discovery | — |
| `--prompt-template` | — | `<path>` | Load prompt template file/dir (repeatable) | — |
| `--no-prompt-templates` | `-np` | — | Disable prompt template discovery | — |
| `--theme` | — | `<path>` | Load theme file/dir (repeatable) | — |
| `--use-theme` | — | `<name>` | Set initial interactive theme | — |
| `--no-themes` | — | — | Disable theme discovery | — |
| `--no-context-files` | `-nc` | — | Disable AGENTS.md / CLAUDE.md discovery | — |
| `--export` | — | `<file>` | Export session file to HTML and exit | — |
| `--list-models` | — | `[search]` | List available models (fuzzy search) | — |
| `--verbose` | — | — | Force verbose startup | — |
| `--tui-mode` | — | `regular\|fullscreen` | TUI mode | — |
| `--approve` | `-a` | — | Trust project-local files for this run | — |
| `--no-approve` | `-na` | — | Ignore project-local files | — |
| `--offline` | — | — | Disable startup network ops (`PI_OFFLINE=1`) | — |
| `--` | — | — | End option parsing; remaining args are messages/files | prompt delivery |
| `--help` | `-h` | — | Show help | — |
| `--version` | `-v` | — | Show version | — |

Extension-registered flags (via discovered extensions):

| Flag | Description |
|------|-------------|
| `--mcp-config <value>` | Path to MCP config file |
| `--fff-mode <value>` | FFF mode: `tools-and-ui\|tools-only\|override` |
| `--fff-frecency-db <value>` | Frecency DB path |
| `--fff-history-db <value>` | Query history DB path |
| `--fff-enable-root-scan` | Allow indexing from filesystem root |
| `--fff-enable-home-scan` | Index home dir when launched from `$HOME` |

**Positional:** `[@files...] [messages...]` — files prefixed with `@` are included in the initial message; remaining strings are messages. Multiple messages are supported in interactive mode; in `-p` mode they are concatenated.

**Examples from help:**

```sh
pi                                    # interactive
pi "List all .ts files in src/"       # interactive with initial prompt
pi @prompt.md @image.png "What color is the sky?"
pi -p "List all .ts files in src/"    # non-interactive (Kage mode)
pi -p -- "- Summarize these points"   # prompt beginning with dash
pi --provider openai --model gpt-4o-mini "Help me refactor"
pi --model openai/gpt-4o "Help me refactor"   # provider prefix, no --provider needed
pi --model sonnet:high "Solve this"           # thinking suffix
pi --tools read,grep,find,ls -p "Review the code"
pi --exclude-tools ask_question
```

### 3.2 v18 (`omp`) delta

`omp --help` adds over `pi` v0.84.4:

- `--smol`, `--slow`, `--plan`, `--prewalk`, `--plan-yolo` (model-role routing)
- `--profile`, `--alias`, `--cwd`, `--config`, `--add-dir`
- `--from-claude`, `--from-codex` (session import)
- `--no-tools`, `--no-lsp`, `--no-pty`, `--tools` (slightly different tool list)
- `--thinking` gains `auto`, `--service-tier`, `--hide-thinking`, `--advisor`, `--external-thinking`
- `--hook` (alias for extension), `--skills` filter, `--no-rules`, `--no-title`, `--print-thoughts`, `--max-time`, `--auto-approve`, `--approval-mode`
- Commands: `acp`, `agents`, `auth-broker`, `browser-relay`, `cleanse`, `commit`, `gallery`, `gc`, `git`, `grep`, `grievances`, `images`, `join`, `models`, `plugin`, `ps`, `read`, `render`, `say`, `search`, `setup`, `share`, `shell`, `ssh`, `stats`, `tiny-models`, `token`, `ttsr`, `update`, `usage`, `worktree`

For Kage's non-interactive use, the relevant flags are unchanged: `-p`, `--model`, `--provider`, `--append-system-prompt`, `--tools`, `-e` all exist in both versions with identical semantics.

---

## 4. Prompt Delivery Modes

### 4.1 Kage's three modes (`src/config/schema.rs:323-336`, `src/adapters/cli.rs:20-23`)

```rust
pub enum PromptDelivery {
    #[default] File,   // placeholder -> pointer prompt, file path via {prompt_file}
    Arg,               // placeholder -> full prompt text inline
    Stdin,             // placeholder removed, prompt piped on stdin
}
const PROMPT_PLACEHOLDER: &str = "{prompt}";
const PROMPT_FILE_PLACEHOLDER: &str = "{prompt_file}";
```

| Kage mode | `PROMPT_PLACEHOLDER` substitution | `{prompt_file}` | argv size | Use case |
|-----------|-----------------------------------|-----------------|-----------|----------|
| `File` (default) | `Read the file at {path} and carry out the instructions in it exactly...` (one-line pointer, `pointer_prompt()` at `cli.rs:246`) | absolute path to prompt file | O(1) — one line | Default; survives Windows 32k argv cap; leaves prompt on disk for debugging |
| `Arg` | full prompt text inline | absolute path | O(prompt) — entire prompt in argv | Small prompts; harnesses that don't read files |
| `Stdin` | placeholder argument **removed** (`filter_map` drops it) | absolute path (still available if template uses `{prompt_file}`) | O(1) — prompt on stdin | Harnesses that read stdin |

All three modes **always write the prompt to disk** (`cli.rs:154-159`) so `what was the agent told?` is answerable after a crash. The delivery mode only controls how the child process receives it.

`render()` at `cli.rs:217-239`:

```rust
fn render(&self, prompt: &str, prompt_file: &Path) -> Vec<String> {
    let prompt_value = match self.delivery {
        Arg   => prompt.to_string(),
        File  => pointer_prompt(&file_display),
        Stdin => String::new(), // placeholder dropped
    };
    self.template.iter().filter_map(|part| {
        if delivery == Stdin && part == PROMPT_PLACEHOLDER { return None; }
        Some(part.replace(PROMPT_PLACEHOLDER, &prompt_value)
                .replace(PROMPT_FILE_PLACEHOLDER, &file_display))
    }).collect()
}
```

### 4.2 How `pi` receives prompts

`pi`'s positional `messages...` are the prompt. Verified:

```sh
$ pi -p "say hello"          # positional arg -> prompt
Hello!

$ echo "hello from stdin pipe" | pi -p
hello — stdin pipe received. what's up?   # stdin also works, but concatenated

$ echo "what is 2+2" | pi -p
4

$ pi -p --mode json "say hi"  # json envelope wraps the same prompt
{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"say hi"}]}}
```

- **Positional (`{prompt}`):** `pi -p "{prompt}"` — the prompt is a positional argument after `-p`. This is the natural mapping for `PromptDelivery::Arg` and `File` (pointer).
- **File pointer (`{prompt_file}`):** `pi -p "Read the file at {prompt_file} and..."` — pi reads the file via its `read` tool. Works because pi has `read` as a built-in tool. The pointer prompt at `cli.rs:248` is exactly this: `Read the file at {path} and carry out the instructions in it exactly.`
- **Stdin:** `echo "$prompt" | pi -p` with no positional prompt arg — pi reads stdin as the prompt (verified above). Kage's `Stdin` mode sets `Spawn.stdin = Some(prompt)` at `cli.rs:166` and drops the placeholder arg.

**Recommendation for Pi preset:** Use `PromptDelivery::File` (default) — same as every other harness. The pointer prompt is one line, keeps argv tiny, and pi's `read` tool handles it. `Stdin` is a viable alternative but less debuggable (no prompt file to inspect after a crash, though Kage still writes it).

### 4.3 Placeholder support in `adapter: command`

Any `command:` argv may use `{prompt}` and `{prompt_file}` anywhere, including multiple times and in any position:

```yaml
roles:
  planner:
    adapter: command
    command: ["pi", "-p", "{prompt}"]
  # or file-pointer style:
  executor:
    adapter: command
    command: ["pi", "-p", "--mode", "json", "{prompt}"]
    prompt_delivery: file
  # or explicit file placeholder:
  reviewer:
    adapter: command
    command: ["pi", "-p", "--session", "{prompt_file}"]
```

The `command` escape hatch is the **only** way to use a harness Kage has no preset for — and the intended first step for pi integration (no code change).

---

## 5. Streaming vs Passthrough

### 5.1 Kage's output handling (`src/adapters/cli.rs:118-136`, `src/adapters/stream.rs`)

```rust
fn output_format(template: &[String]) -> stream::OutputFormat {
    let asks_for_stream = template.windows(2)
        .any(|pair| pair[0] == "--output-format" && pair[1] == "stream-json")
        || template.iter().any(|arg| arg == "--output-format=stream-json");
    if asks_for_stream && is_claude(program) {
        stream::OutputFormat::ClaudeStreamJson
    } else {
        stream::OutputFormat::Passthrough
    }
}
fn is_claude(program: &str) -> bool {
    Path::new(program).file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("claude"))
}
```

- **Decision is argv-based, not kind-based:** A user who overrides `command:` to spawn `claude` without `--output-format stream-json` gets `Passthrough`; a non-claude tool that happens to use the same flag does not get decoded as Claude events.
- **`ClaudeStreamJson`:** Each stdout line is parsed as JSON and rendered to a one-line progress summary (`stream::render`). A `.progress.log` twin is written alongside the raw log (`progress_path` at `proc.rs:520`). Only streaming harnesses get a progress twin.
- **`Passthrough`:** Lines are forwarded verbatim to terminal, log, and `Outcome.stdout`. No JSON parsing.

### 5.2 `pi --mode json` — NDJSON event stream

Verified against live `pi -p --mode json "say hello in json mode"`:

```json
{"type":"session","version":3,"id":"01a05375-...","timestamp":"...","cwd":"..."}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"..."}]}}
{"type":"message_end",...}
{"type":"message_start","message":{"role":"assistant","content":[{"type":"text","text":"{\"hello\": \"hello\"}"}],"api":"openai-completions","provider":"orvix","model":"orvix/muse-spark-1.2","usage":{...},"stopReason":"pending"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"{\"hello\": \"hello\"}"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","content":"{\"hello\": \"hello\"}"}}
{"type":"message_end",...}
{"type":"turn_end",...}
{"type":"agent_end","messages":[...],"willRetry":false}
{"type":"agent_settled"}
```

Schema: `session` → `agent_start` → `turn_start` → `message_start`/`message_update`* / `message_end` → `turn_end` → `agent_end` → `agent_settled`. Each `message_update` carries `assistantMessageEvent` with `text_start`/`text_delta`/`text_end`.

This is **not** Claude's `stream-json` schema (which uses `system`/`assistant`/`user`/`result` event types at `stream.rs:62-225`). Pi's json mode would currently be classified as `Passthrough` by Kage (no `--output-format stream-json` flag, program is not `claude`), so its NDJSON would be captured verbatim in `Outcome.stdout` and the terminal would show raw JSON lines — usable but noisy.

**Options for Pi streaming:**

1. **Passthrough (default, no code change):** `pi -p "{prompt}"` without `--mode json` emits plain text — already correct for `Passthrough`. This is the recommended default for the `command` escape-hatch phase.
2. **JSON passthrough:** `pi -p --mode json "{prompt}"` with `Passthrough` — raw NDJSON in logs, still parseable by `gates.rs` if the final assistant text is extracted. Works today, no code change.
3. **Future native streaming:** Add `OutputFormat::PiJson` and a `render_pi_event()` that extracts `text_delta` and tool calls, mirroring `render_event()` for Claude. Requires extending `output_format()` to detect `pi`/`omp` + `--mode json` and adding a new renderer in `stream.rs`. Not required for v1; passthrough is sufficient.

**Text mode (default):** `pi -p "say hi"` emits plain text with no envelope — ideal for `Passthrough` and for `gates.rs` VERDICT parsing.

---

## 6. Windows Handling — `proc::resolve_program` (`src/adapters/proc.rs:591-695`)

### 6.1 Problem

Rust's `Command` calls `CreateProcess`, which only executes real PE binaries. Node-based CLIs (`claude`, `opencode`, `pi`, `omp`) install on Windows as `*.cmd` batch shims (`claude.cmd`, `pi.cmd`). Handing `pi.cmd` to `CreateProcess` fails with `%1 is not a valid Win32 application`.

### 6.2 Solution — `resolve_program` + `shim_for` + `search_in`

```rust
pub struct Resolved {
    pub program: String,        // what to spawn (e.g. "cmd" for shims)
    pub prefix_args: Vec<String>, // prepended args (e.g. ["/C", "C:\...\pi.cmd"])
    pub path: PathBuf,          // file that was found, for doctor reporting
}

pub fn resolve_program(program: &str) -> Result<Resolved> {
    let found = if Path::new(program).components().count() > 1 || is_absolute {
        direct.is_file().then(|| direct.to_path_buf())
    } else {
        search_path(program)  // PATH lookup with PATHEXT
    };
    Ok(shim_for(found))
}

fn shim_for(found: PathBuf) -> Resolved {
    let is_batch = ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat");
    if is_batch {
        Resolved { program: "cmd".into(), prefix_args: vec!["/C".into(), found.to_string_lossy().into()], path: found }
    } else {
        Resolved { program: found.to_string_lossy().into(), prefix_args: vec![], path: found }
    }
}
```

**PATH search with PATHEXT** (`search_in` at `proc.rs:651-672`):

- On Windows, `executable_extensions()` reads `PATHEXT` (e.g. `.COM;.EXE;.BAT;.CMD`) or falls back to `[".COM", ".EXE", ".BAT", ".CMD"]`.
- For each `PATH` dir, tries `program + ext` for every extension **before** trying the bare name — critical because npm installs both `codex` (POSIX shell script for Git Bash) and `codex.cmd` side by side; preferring the extensionless one would hand a shell script to `CreateProcess`.
- On non-Windows, `executable_extensions()` returns `[]`, so only the bare name is tried.

**How it flows into spawning** (`proc::run` at `proc.rs:117-371`): `Resolved.prefix_args` are prepended to the caller's `args` before `Command::new(&resolved.program)`. So `pi -p "hello"` on Windows becomes `cmd /C "C:\...\pi.cmd" -p "hello"` — `cmd /C` strips the outer quotes and runs the batch file verbatim, preserving inner quoting.

### 6.3 Pi/omp on Windows

- `pi` installed via npm on Windows creates `pi.cmd` (and `pi` shell script) in the npm prefix's `bin` dir, which is on PATH.
- `omp` (v18, Bun-compiled) installs as `omp.exe` — a real binary, no shim needed. But if installed via npm compat layer, it may still have `omp.cmd`.
- Both cases are handled: `resolve_program("pi")` finds `pi.cmd` → `cmd /C pi.cmd`; `resolve_program("omp")` finds `omp.exe` → direct spawn.
- No pi-specific code needed — the existing shim logic covers it.

---

## 7. Current `AdapterKind` Design (`src/config/schema.rs:269-321`, `src/adapters/cli.rs:82-116`)

### 7.1 Variants

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    Api,                    // HTTP endpoint, not a CLI
    ClaudeCode,             // "claude-code" in YAML
    Codex,                  // "codex"
    #[serde(rename = "opencode")]
    OpenCode,               // "opencode" (tool spells it as one word)
    Kamui,                  // "kamui"
    Command,                // generic escape hatch
}
impl Display for AdapterKind { /* api, claude-code, codex, opencode, kamui, command */ }
```

Defaults per role (`schema.rs:78-88`):

```rust
fn default_planner()  -> RoleConfig { RoleConfig::preset(AdapterKind::ClaudeCode) }
fn default_executor() -> RoleConfig { RoleConfig::preset(AdapterKind::OpenCode) }
fn default_reviewer() -> RoleConfig { RoleConfig::preset(AdapterKind::Codex) }
```

### 7.2 `RoleConfig` shape (`schema.rs:90-131`)

```rust
pub struct RoleConfig {
    pub adapter: AdapterKind,
    pub model: Option<String>,              // None = harness default (subscription CLIs)
    pub command: Option<Vec<String>>,       // required for Command, overrides preset otherwise
    pub provider: Option<String>,           // required for Api
    pub prompt_delivery: PromptDelivery,    // default File
    pub timeout_secs: u64,                  // default 1800
    pub stall_secs: u64,                    // default 600, 0 = disabled
    pub extra_args: Option<Vec<String>>,    // None = adapter defaults, Some([]) = none
    pub env: BTreeMap<String, String>,
}
```

`extra_args` is `Option<Vec<String>>` (not plain `Vec`) so `None` (key omitted) can fall back to `default_extra_args()` while `Some([])` (explicit `extra_args: []`) means "no extra args" — the fix for the bug where every hand-written config silently lost its permission flags.

### 7.3 `preset_template` (`cli.rs:82-116`)

```rust
fn preset_template(kind: AdapterKind, model: Option<&str>) -> Vec<String> {
    let mut argv = match kind {
        ClaudeCode => vec!["claude", "--print", "--output-format", "stream-json", "--verbose"],
        Codex      => vec!["codex", "exec"],
        OpenCode   => vec!["opencode", "run"],
        Kamui      => vec!["kamui", "-p"],
        Api | Command => vec![],
    };
    if let Some(model) = model {
        let flag = match kind {
            ClaudeCode              => Some("--model"),
            Codex | OpenCode        => Some("-m"),
            Kamui | Api | Command   => None, // Kamui picks model from its own config
        };
        if let Some(flag) = flag { argv.push(flag.into()); argv.push(model.into()); }
    }
    argv.push(PROMPT_PLACEHOLDER.into());
    argv
}
```

Key details:

- `ClaudeCode` requires `--verbose` — without it `claude --print --output-format stream-json` exits with an error.
- `Kamui` ignores `model` entirely — it reads its own config file.
- Every preset ends with `{prompt}` as the last element; `resolved_extra_args()` are appended **after** it (`cli.rs:58`).

### 7.4 `program_for` (`preflight.rs:40-59`)

```rust
pub fn program_for(config: &RoleConfig) -> String {
    if let Some(command) = &config.command { return command.first().unwrap().clone(); }
    match config.adapter {
        ClaudeCode => "claude".into(),
        Codex      => "codex".into(),
        OpenCode   => "opencode".into(),
        Kamui      => "kamui".into(),
        Api        => config.provider.clone().unwrap_or_else(|| "<unconfigured>".into()),
        Command    => "<unconfigured>".into(),
    }
}
```

Used by both `preflight::inspect` (doctor) and `preflight::check` (pre-run gate). For `Api`, the "program" is the provider name (no binary to find); for bare `Command` without `command:`, it's `<unconfigured>`.

### 7.5 `default_extra_args` (`schema.rs:292-306`)

```rust
pub fn default_extra_args(self) -> Vec<String> {
    match self {
        ClaudeCode => vec!["--permission-mode".into(), "acceptEdits".into()],
        Codex      => vec!["--sandbox".into(), "workspace-write".into()],
        Kamui      => vec!["--auto-approve".into()],
        Api | OpenCode | Command => vec![],
    }
}
```

- `ClaudeCode` needs `--permission-mode acceptEdits` — without it `--print` denies file writes and the planner exits with an empty `PLAN.md`.
- `Codex` needs `--sandbox workspace-write` — defaults to read-only.
- `Kamui` needs `--auto-approve` — otherwise every tool call prompts.
- `OpenCode` needs nothing — `run` mode already permits edits.

Resolved via `resolved_extra_args()` (`schema.rs:165-169`):

```rust
pub fn resolved_extra_args(&self) -> Vec<String> {
    self.extra_args.clone().unwrap_or_else(|| self.adapter.default_extra_args())
}
```

### 7.6 `adapter: command` escape hatch (`cli.rs:42-55`)

```rust
let template = match (&config.command, config.adapter) {
    (Some(command), _) => command.clone(),                          // command: always wins
    (None, AdapterKind::Command) => bail!("adapter: command but no command"),
    (None, kind) => preset_template(kind, config.model.as_deref()), // preset
};
template.extend(config.resolved_extra_args());
```

- If `command:` is set, it **replaces** the preset entirely, regardless of `adapter:` value. This lets a user run `claude` with custom flags without forking the preset.
- `adapter: command` **requires** `command:` — fails at `CliAdapter::from_config` with a message naming the role.
- Placeholders `{prompt}` and `{prompt_file}` are substituted in `render()`; any position is valid.

---

## 8. Preflight / Doctor (`src/adapters/preflight.rs`)

### 8.1 `inspect` — the shared lookup

```rust
pub fn inspect(roles: &Roles) -> Vec<RoleProgram<'_>> {
    [(Planner, &roles.planner), (Executor, &roles.executor), (Reviewer, &roles.reviewer)]
        .into_iter().map(|(role, config)| {
            let program = program_for(config);
            let resolved = proc::resolve_program(&program).ok();
            RoleProgram { role, config, program, resolved }
        }).collect()
}
pub struct RoleProgram<'a> {
    pub role: Role,
    pub config: &'a RoleConfig,
    pub program: String,
    pub resolved: Option<proc::Resolved>,
    // found() -> resolved.is_some()
    // label() -> "planner (claude)"
}
```

- Resolves every role's program via `proc::resolve_program` (PATH + PATHEXT + shim handling).
- `None` means not found — not an error, just `found() == false`.
- Order is loop order: planner → executor → reviewer.

### 8.2 `check` — pre-run gate

```rust
pub fn check(config: &Config, roles: &[Role]) -> Result<()> {
    for status in inspect(&config.roles) {
        if !roles.contains(&status.role) { continue; } // skip roles this run won't use
        crate::adapters::build(status.role, config)?;  // validates Api provider/model
        if status.config.adapter == AdapterKind::Api { continue; } // no binary to find
        if !status.found() { missing.push(status.label()); }
    }
    if !missing.is_empty() { bail!("{not_ready(&missing)}\nNo agent was spawned — run `kage doctor` for the full report."); }
    Ok(())
}
```

- Called before a run touches disk — no run dir, worktree, or state file if a harness is missing.
- Only checks roles the run will actually spawn (a `--skip-plan` run doesn't require the planner).
- `build()` is called first so a misconfigured `Api` role (missing provider/model) gets its specific error rather than `<unconfigured> is not on PATH`.
- `Api` roles are skipped for binary lookup — their reachability is the endpoint's, and a missing key already failed in `build()`.

### 8.3 `not_ready` — shared remedy line

```rust
pub fn not_ready(missing: &[String]) -> String {
    format!("Not ready: install or reconfigure {}.", missing.join(", "))
}
```

Both `kage doctor` and `preflight::check` call this, so the two messages can never drift.

### 8.4 `kage doctor` (`src/cli/doctor.rs`)

- Calls `preflight::inspect`, prints per-role `found`/`missing` with resolved paths, ends with `not_ready` if anything is missing.
- **Never a gate** — exits `Ok` even when not ready (per `AGENTS.md` repo map). It reports; `preflight::check` gates.

### 8.5 Pi in preflight

With `adapter: command` + `command: ["pi", "-p", "{prompt}"]`, `program_for` returns `"pi"` and `resolve_program("pi")` finds the Node shim (or `pi.cmd` on Windows). With a future `AdapterKind::Pi`, `program_for` would return `"pi"` directly — same lookup, no special case.

---

## 9. Proposed `AdapterKind::Pi` Design

### 9.1 Single alias — `Pi`, not `Pi` + `Ohmypi`

**Decision:** One variant `Pi` (serde `pi`, display `pi`). The npm rename (`@earendil-works/pi-coding-agent` → `@oh-my-pi/pi-coding-agent`, binary `pi` → `omp`) is a distribution detail, not a harness difference. The argv, tools, and behavior are identical. Two variants would duplicate `preset_template`, `program_for`, and `default_extra_args` with no divergence to justify it.

Users on v18 where only `omp` is on PATH use the escape hatch:

```yaml
roles:
  planner:
    adapter: command
    command: ["omp", "-p", "{prompt}"]
```

Or, if Kage later wants to handle both spellings transparently, `program_for` for `Pi` could try `pi` then `omp` — but that belongs to a follow-up, not the initial preset.

### 9.2 Sketch — what changes in `config/schema.rs` + `adapters/cli.rs`

**`config/schema.rs` — `AdapterKind`:**

```rust
pub enum AdapterKind {
    Api,
    ClaudeCode,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    Kamui,
    Pi,         // ← new
    Command,
}
// Display: Self::Pi => "pi"

impl AdapterKind {
    pub fn default_extra_args(self) -> Vec<String> {
        match self {
            ClaudeCode => vec!["--permission-mode".into(), "acceptEdits".into()],
            Codex      => vec!["--sandbox".into(), "workspace-write".into()],
            Kamui      => vec!["--auto-approve".into()],
            Pi         => vec![], // pi runs unattended with no permission flag; verify --approve is not needed for -p mode
            Api | OpenCode | Command => vec![],
        }
    }
}
```

`Pi` needs no `default_extra_args` — `pi -p` runs unattended without a permission flag (unlike `claude --print` which denies writes, and `codex exec` which defaults to read-only). If pi later gates writes behind `--approve`, add it here.

**`adapters/cli.rs` — `preset_template`:**

```rust
fn preset_template(kind: AdapterKind, model: Option<&str>) -> Vec<String> {
    let mut argv = match kind {
        ClaudeCode => vec!["claude".into(), "--print".into(), "--output-format".into(), "stream-json".into(), "--verbose".into()],
        Codex      => vec!["codex".into(), "exec".into()],
        OpenCode   => vec!["opencode".into(), "run".into()],
        Kamui      => vec!["kamui".into(), "-p".into()],
        Pi         => vec!["pi".into(), "-p".into()], // ← new
        Api | Command => vec![],
    };
    if let Some(model) = model {
        let flag = match kind {
            ClaudeCode         => Some("--model"),
            Codex | OpenCode   => Some("-m"),
            Pi                 => Some("--model"), // ← pi uses --model like claude
            Kamui | Api | Command => None,
        };
        if let Some(flag) = flag { argv.push(flag.into()); argv.push(model.into()); }
    }
    argv.push(PROMPT_PLACEHOLDER.into());
    argv
}
```

`Pi` uses `--model` (same as `ClaudeCode`), not `-m`. Verified: `pi --model openai/gpt-4o "prompt"` and `pi --model sonnet:high "prompt"` both work per `pi --help`.

**`adapters/preflight.rs` — `program_for`:**

```rust
match config.adapter {
    ClaudeCode => "claude".into(),
    Codex      => "codex".into(),
    OpenCode   => "opencode".into(),
    Kamui      => "kamui".into(),
    Pi         => "pi".into(), // ← new
    Api        => config.provider.clone().unwrap_or_else(|| UNCONFIGURED.into()),
    Command    => UNCONFIGURED.into(),
}
```

**`adapters/cli.rs` — `output_format`:** No change required for v1. Pi stays `Passthrough` (no `--output-format stream-json`, program is not `claude`). A future `PiJson` variant would extend this:

```rust
// Future (not v1):
fn output_format(template: &[String]) -> stream::OutputFormat {
    let isPi = template.first().is_some_and(|p| {
        Path::new(p).file_stem().and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("pi") || s.eq_ignore_ascii_case("omp"))
    });
    let asksPiJson = template.contains(&"--mode".into()) && template.contains(&"json".into());
    if asksPiJson && isPi { return stream::OutputFormat::PiJson; }
    // ... existing claude check
}
```

### 9.3 Config examples

```yaml
# After preset lands — idiomatic
roles:
  planner:
    adapter: pi
    model: orvix/muse-spark-1.2   # optional; None = pi's default (subscription)
  executor:
    adapter: pi
    # no model = pi's configured default
  reviewer:
    adapter: pi
    model: anthropic/claude-sonnet-4

# With extra args (e.g. restrict tools for reviewer)
roles:
  reviewer:
    adapter: pi
    model: orvix/muse-spark-1.2
    extra_args: ["--tools", "read,grep,glob"]

# Explicitly no extra args (override defaults if Pi ever gains any)
roles:
  planner:
    adapter: pi
    extra_args: []
```

---

## 10. Migration Path

### Phase 1 — `adapter: command` (today, no code change)

```yaml
roles:
  planner:
    adapter: command
    command: ["pi", "-p", "{prompt}"]
  executor:
    adapter: command
    command: ["pi", "-p", "{prompt}"]
  reviewer:
    adapter: command
    command: ["pi", "-p", "{prompt}"]
```

- Works on the current `main` — no Rust change, no release.
- `program_for` returns `pi`, `resolve_program` finds it (or `pi.cmd` on Windows), `preflight::check` and `kage doctor` both report it.
- `prompt_delivery: file` (default) is correct — pointer prompt via `read` tool.
- `model:` is not supported in this phase (no flag injection) — use `command: ["pi", "-p", "--model", "orvix/muse-spark-1.2", "{prompt}"]` if a per-role model is needed.
- Validate with `kage doctor` and a dry run before committing.

### Phase 2 — Native `AdapterKind::Pi` preset (one PR)

1. Add `Pi` variant to `AdapterKind` (`schema.rs`) with `Display`, `default_extra_args`.
2. Add `Pi => vec!["pi", "-p"]` to `preset_template` and `Pi => Some("--model")` to the model-flag match (`cli.rs`).
3. Add `Pi => "pi"` to `program_for` (`preflight.rs`).
4. Add tests mirroring `cli.rs` existing suite: `pi_runs_unattended_and_uses_model_flag`, `program_for_pi_is_pi`, `default_extra_args_pi_is_empty`.
5. Update `README.md` / `docs/` to list `pi` alongside `claude-code`, `codex`, `opencode`, `kamui`.

No change to `proc::resolve_program`, `stream.rs`, or `PromptDelivery` is required for Phase 2. Streaming support (`PiJson`) is a separate follow-up.

### Phase 3 — Optional streaming (follow-up)

- Add `OutputFormat::PiJson`, extend `output_format()` to detect `pi`/`omp` + `--mode json`, implement `render_pi_event()` in `stream.rs`.
- Only if raw NDJSON in `Passthrough` proves too noisy for `kage status` / progress logs.

---

## 11. Model Hazard

### 11.1 Pi's `--model` is fuzzy and overloaded

`pi --model` accepts:

- Bare id: `--model gpt-4o-mini`
- Provider-qualified: `--model openai/gpt-4o`, `--model orvix/muse-spark-1.2`
- Thinking suffix: `--model sonnet:high`, `--model gpt-5:medium`
- Combined: `--model openai/gpt-4o:high`
- Glob for cycling: `--models claude-sonnet,claude-haiku,gpt-4o` (separate flag, not `--model`)

Kage's `RoleConfig.model` is an opaque `Option<String>` forwarded verbatim as the value after the model flag. This is correct — Kage must not parse or split it. But it means:

- A `model: "orvix/muse-spark-1.2"` containing `/` is forwarded as one argv element — pi interprets the `/` as provider separator, which is intended.
- A `model: "sonnet:high"` containing `:` is forwarded as one element — pi interprets `:high` as thinking level, which is intended.
- A typo like `model: "openai/gpt-4o:high:extra"` would be silently accepted by Kage and rejected by pi at runtime — no validation at config-load time.

**Mitigation:** Document that `model:` for `pi` is passed through verbatim and must match `pi --list-models` output. No code validation — pi's own error is the source of truth, and Kage's `preflight::check` does not run the harness.

### 11.2 Default model (None) is intentional

```rust
pub fn preset(adapter: AdapterKind) -> Self {
    Self { adapter, model: None, .. }
}
```

`None` means "let the harness pick its default" — for subscription CLIs like pi, this is the user's configured default model (often the subscription's premium model). Forcing a model when the user has a subscription default would be wrong. The `Api` adapter is the exception: `Config::validate` rejects `adapter: api` with no `model` because an HTTP endpoint has no default.

For `Pi`, `None` is correct — same as `ClaudeCode`/`Codex`/`Kamui`. Only `Api` requires a model.

### 11.3 `--provider` is separate from `--model`

`pi --provider openai --model gpt-4o-mini` and `pi --model openai/gpt-4o-mini` are equivalent — the `provider/id` form implies the provider. Kage's `RoleConfig` has no `provider` field for CLI adapters (only for `Api`); the provider is encoded in the `model` string when needed. No change required.

---

## 12. Verification

Commands run on 2026-08-31 (macOS arm64, Node v24.16.0, `pi` 0.84.4, `omp` 18.0.11):

```sh
which pi                          # -> .../bin/pi -> .../pi-coding-agent/dist/bundle/cli.js
pi --help                         # full argv captured in §3
pi --version                      # 0.84.4
npm view @oh-my-pi/pi-coding-agent version  # 18.0.11
which omp && omp --help           # omp 18.0.11, same flags plus --smol/--slow/--plan etc.
pi -p "say hello"                  # -> Hello!
echo "hello from stdin pipe" | pi -p  # -> stdin received
echo "what is 2+2" | pi -p         # -> 4
pi -p --mode json "say hello in json mode"  # -> NDJSON session/agent_start/turn_start/message_start/message_update/message_end/turn_end/agent_end/agent_settled
```

Source files read:

- `src/adapters/cli.rs` — `CliAdapter::from_config`, `preset_template`, `output_format`, `is_claude`, `render`, `pointer_prompt`
- `src/config/schema.rs` — `AdapterKind`, `RoleConfig`, `PromptDelivery`, `default_extra_args`, `resolved_extra_args`, `Config::validate`
- `src/adapters/proc.rs` — `Resolved`, `resolve_program`, `shim_for`, `search_path`, `search_in`, `executable_extensions`, `shell_spawn`
- `src/adapters/preflight.rs` — `RoleProgram`, `program_for`, `inspect`, `check`, `not_ready`
- `src/adapters/stream.rs` — `OutputFormat`, `render`, `render_event`
- `src/adapters/mod.rs` — `Role`, `AgentAdapter`, `build`

---

## 13. Open Questions

1. **Binary name on fresh installs:** If `@oh-my-pi/pi-coding-agent` v18 only installs `omp` (not `pi`), should `program_for(Pi)` try `pi` then `omp`, or should users on v18 use `adapter: command` with `omp` until Kage updates? Recommendation: ship `Pi` with `pi`, document the `omp` workaround, and add fallback only if users report breakage.
2. **`--approve` for pi:** Does `pi -p` ever gate file writes behind an approval prompt? Manual testing shows no — but if a future pi version adds a sandbox, `default_extra_args` for `Pi` would need `["--approve"]` or similar. Verify against pi's changelog before adding.
3. **Streaming priority:** Is `Passthrough` sufficient for pi, or does the NDJSON noise warrant a `PiJson` renderer? Defer until a real `pi` run shows the progress log is unreadable.

---

## 14. References

- `pi --help` (v0.84.4, 2026-08-31)
- `omp --help` (v18.0.11, 2026-08-31)
- npm: `@oh-my-pi/pi-coding-agent` 18.0.11, `@earendil-works/pi-coding-agent` 0.84.4
- Kage source: `src/adapters/cli.rs`, `src/config/schema.rs`, `src/adapters/proc.rs`, `src/adapters/preflight.rs`, `src/adapters/stream.rs`, `src/adapters/mod.rs`
- Kage docs: `AGENTS.md`, `docs/PRD.md`
