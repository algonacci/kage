# Kage Development Guide

Read this before changing anything. It records the decisions behind the code and the traps that are
invisible from reading it — the part that is expensive to re-derive.

Kage is an engineering workflow orchestrator for AI coding agents. It does not implement a coding
agent; it drives the ones already installed.

Core principle: **be flexible on approach, strict on results.** A run may take any route, but a
verdict and an exit code decide whether it is accepted.

Positioning: [Kamui](https://github.com/algonacci/kamui) is a general-purpose agent, Kumo is a
messaging gateway, Kage manages engineering processes. Kage must stay fully usable without either.
Personal memory, email, calendar, and chat belong in those projects, not here.

## The loop

```text
TASK -> PLAN -> EXECUTE -> TEST -> REVIEW -> DECISION -> DONE
                            ^                    |
                            +------- FIX <-------+
```

One planner, one executor, one reviewer. No DAGs, no parallel agents, no dynamic delegation — those
belong to later versions and are not worth building before this line is reliable end to end.

## Repository map

```text
src/
├── main.rs              clap CLI definition; every subcommand delegates to cli/
├── cli/
│   ├── mod.rs           init, run, resume, clean; exit-code mapping
│   ├── status.rs        run report and the "what do I do now" line
│   └── doctor.rs        environment report (never a gate — it exits Ok even when not ready)
├── engine/
│   ├── workflow.rs      the state machine: start, resume, drive
│   ├── prompts.rs       what each role is told; the highest-leverage file in the repo
│   ├── gates.rs         VERDICT.json parsing, PASS/FAIL/BLOCKED
│   └── runner.rs        validation commands, TEST_RESULTS.md
├── adapters/
│   ├── mod.rs           AgentAdapter trait, Role, the role -> backend seam
│   ├── cli.rs           spawning coding-agent CLIs; per-harness argv presets
│   ├── api.rs           an HTTP endpoint filling a role, for planner and reviewer only
│   ├── preflight.rs     resolve every role's program before a run spends anything
│   ├── proc.rs          all process spawning: capture, timeout, PATH, rtk routing, live logs, heartbeat
│   └── stream.rs        rendering a streaming harness's events as progress lines
├── state/
│   ├── run.rs           RunState, Phase
│   └── store.rs         atomic persistence, Artifacts and their worktree mirror
├── git/
│   ├── worktree.rs      isolation
│   └── diff.rs          what the executor changed, for the reviewer
├── config/
│   ├── mod.rs           project discovery, kage init, starter config
│   └── schema.rs        serde shapes for .kage/config.yaml
└── paths.rs             path normalisation shared by anything handing a path to another program
```

Every external program — agents, validation commands, git — is spawned through `proc::run`, so
timeout and logging behaviour is identical everywhere. Add nothing that spawns a process directly.

## Decisions and their reasons

**Roles, never models.** The engine asks for "the planner". Which harness or model fills it is a
config edit, not a code change. Do not let a model name reach `engine/`.

**Artifacts, not chat history.** Three harnesses with three context windows cannot share a
conversation, and a file on disk survives a crash and can be read afterwards. Every prompt is
assembled from files and stands alone.

**Artifacts live inside the worktree, mirrored back per phase.** Agents sandbox themselves to their
working directory. Artifacts in the project's own `.kage/` are unreachable: the planner cannot read
its own prompt or write `PLAN.md`, and every isolated run dies at PLAN. The mirror runs after each
phase, not at the end, so a crash still leaves a readable plan behind. `.kage` is excluded from the
diff, or the reviewer is shown its own artifacts as the executor's work.

**Failing validation skips review.** Asking a premium reviewer to judge code that does not build
spends real money to learn what an exit code already proved.

**A review with no machine-readable verdict blocks the run.** Guessing PASS lets unreviewed code
through; guessing FAIL burns an iteration on nothing.

**Validation runs in Kage's process, never inside an agent.** An agent reporting "tests pass" is a
claim. An exit code is evidence.

**Prompts are delivered as a file pointer, not an argument.** Windows caps a command line near 32k
characters and a real `PLAN.md` exceeds that. The prompt file is also the record of exactly what an
agent was told, which is the first thing you want when a run goes wrong.

**Isolation is on by default.** An autonomous executor with write access must not be pointed at
uncommitted work. Branches outlive their checkouts, so `kage clean` keeps them.

**Kage owns no harness credentials.** Authentication is each tool's business. A harness that is not
logged in is a `kage doctor` finding, not something Kage fixes. An API provider is the exception
only in that its key is read from an environment variable named in the config — never from the
config itself, which gets committed, pasted into issues, and read by the agents Kage spawns.

**Only the planner and the reviewer may be backed by an API.** Both are text-in, text-out: Kage
assembles their context from artifacts and writes their deliverable from the reply. The executor
must read, edit, compile and re-run tests, so backing it with a completions endpoint would mean
growing a tool loop here — rebuilding the agents Kage exists to orchestrate. `Config::validate`
rejects that configuration outright rather than half-supporting it.

## Harness facts

Invocations verified against each tool's own `--help`, not guessed. They are the piece most likely
to drift, which is why any role can override the whole argv with `command:`.

```text
claude   --print --output-format stream-json --verbose [--model M] <prompt> --permission-mode acceptEdits
codex    exec [-m M] <prompt> --sandbox workspace-write
opencode run [-m M] <prompt>
kamui    -p <prompt> --auto-approve
```

The argv order is literal: `extra_args` (the permission flags) are appended *after* the prompt,
which is how the constructed command actually reads (REV-003 caught this file claiming otherwise).

`--output-format stream-json` is what makes the planning phase observable at all: without it claude
buffers its entire transcript until exit and the terminal shows nothing for the whole phase.
`--verbose` is mandatory alongside it under `--print` — claude exits with an error without it.

The permission flags are not optional. Without them the harness stops to ask a human who is not
there, cannot write its deliverable, and the run dies having produced nothing.

Model names are a live hazard. A name the harness does not recognise fails the run *after* earlier
phases have been paid for. `opus-5` is not valid for claude (`opus` or `claude-opus-5` is), and
opencode needs `provider/model`, not a bare name. Prefer leaving `model` unset so each harness uses
what it is already configured with.

All three harnesses read this file. `AGENTS.md` alone is enough — verified, no `CLAUDE.md` needed.

## Platform traps

Every one of these silently breaks runs on Windows and none are visible from reading the code.

- `std::fs::canonicalize` returns `\\?\C:\...`. Git rejects that form outright when creating a
  worktree. Always go through `paths::canonical`.
- Rust's `Command` cannot execute `.cmd` batch shims, which is how npm installs `claude`, `codex`,
  and `opencode`. They must run through `cmd /C` — `proc::resolve_program` handles it.
- npm also installs an extensionless POSIX script beside the `.cmd`. PATH lookup must prefer PATHEXT
  extensions or it finds the unrunnable one.
- Per-argument escaping mangles quotes inside a `cmd /C` command line, so a validation command
  containing quotes fails with a syntax error. Use `Spawn::raw_command`.
- A branch outlives its checkout, so `git worktree add -b` fails on resume or after `clean`. Attach
  to the existing branch instead.

## Conventions

Match the surrounding code. Specifically:

- Doc comments explain **why**, not what. A comment restating the signature is noise; a comment
  naming the failure a line prevents is the reason the file is maintainable.
- `anyhow` with `.context()` for errors the user will read. Error messages name the remedy, not just
  the fault.
- Tests live in `#[cfg(test)] mod tests` at the bottom of the module they cover. There is no
  `tests/` directory and none should be added.
- Test names are sentences: `a_missing_program_is_an_error_rather_than_a_panic`. A test guarding a
  past bug says so in a comment, including what broke.
- Tests must not mutate `PATH` or any process-global state — the suite runs in parallel and one
  test corrupting the environment fails unrelated ones. Pass the environment in as a parameter
  instead, the way `proc::search_in` does.
- No test may depend on `claude`, `codex`, `opencode`, `kamui`, or `rtk` being installed. CI runs on
  Linux, macOS, and Windows.

## Verification

These are the project's validation commands and the exact CI steps:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

If [RTK](https://github.com/rtk-ai/rtk) is installed, simple validation commands are routed through
it to compress output. Optional; commands with shell operators always run directly.

## Known gaps

Honest list of what does not work yet. Do not assume the code is complete because it is tested.

- **A completed run does not commit the agent's work.** The branch stays empty, the `git merge`
  instruction Kage prints is a no-op, and `kage clean` force-removes the worktree — destroying work
  it just told the user was safe. This is the most damaging open bug.
- **The executor often skips `EXECUTION.md`.** Only `PLAN.md` is enforced, so the reviewer receives
  a placeholder instead of what the executor claims it did.
- **Agent failures dump raw harness output** — session ids, transport errors, prompt echoes — where
  one line explains the problem.
- **Cross-repo tasks do not work.** Agents are sandboxed to the worktree, and the prompts tell them
  to work only inside it. Use `--no-isolate` or copy the material in.

## Definition of done

A change is done when the three validation commands pass, new behaviour has a test that would fail
without it, past bugs have a regression test that says what broke, and any user-facing promise
(`kage status` advice, README, terminal output) is actually true. Several bugs in this repository
were Kage confidently telling the user something that was not.
