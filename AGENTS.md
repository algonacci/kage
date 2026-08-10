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

**A broken build and a review finding spend different budgets.** `loop.max_iterations` counts
review rejections — judgment, paid for with a premium model. `loop.max_repairs` counts attempts to
make validation pass — mechanical, caught by an exit code — and refills each time a rejection opens
a new fix cycle, because each review's findings are a fresh implementation job with the same right
to a compiling result. When both drew on one budget, three failed builds could end a run before the
reviewer had seen the code at all. The cause also frames the fixer's prompt: a build no reviewer
saw is never described as "reviewed and rejected".

**The executor's account is enforced like the plan.** A phase that produces `EXECUTION.md` may not
end without one. When it is missing the executor is asked once more for just the summary — the
implementation is already on disk and is the expensive artifact, while the account is prose that can
be rebuilt from the diff — and if that produces nothing the run fails rather than handing the
reviewer a placeholder where the executor's claims belong. The gate and the placeholder share one
predicate (`Artifacts::has_content`) so a blank file cannot pass one and trip the other.

**A review with no machine-readable verdict blocks the run.** Guessing PASS lets unreviewed code
through; guessing FAIL burns an iteration on nothing.

**One run works one repository; cross-repo tasks are a boundary, not a gap.** Decided, not
deferred: every guarantee Kage makes — isolation, the diff against the base commit, the review's
scope, commit-on-finish, what `kage clean` may safely remove — is defined per worktree. A second
repository has no base commit, no run branch, and no place in the diff the reviewer judges, so
work done there would be unreviewed by construction: exactly what the gates exist to prevent.
Supporting it means one agent per repository with explicit dependencies, which is graph
engineering (v1.x), not a patch on this loop. Until then the two sanctioned routes stand: copy
the material into the repository (a read need — and the material the executor saw is then
versioned with the run), or `--no-isolate` when the user knowingly trades away isolation.

**Task sizing is the planner's call, declared before the executor spends anything.** A three-part
task once overran an executor budget that had been generous for everything before it, and the user
learned only after the hour was gone. A string heuristic cannot judge task size, but the planner —
the most capable model in the loop, having just read the repository — can: it is told a plan is
for one executor run, to plan only the first coherent piece of an oversized task, and to declare
the rest in a `# Deferred Tasks` section, each piece a one-line task for a later `kage run`. Kage
relays that section to the terminal the moment the plan lands and never judges it; presence of the
section *is* the signal, so the planner omits it when the task fits. `--skip-plan` runs are
exempt: the person typing the task already claimed its shape.

**A silent phase is aborted early as presumed hung.** Every harness Kage spawns prints as it
works, so total silence for `stall_secs` (default 600; `0` disables) is the one mechanical "this
will not finish" signal available — a harness stuck on a hidden prompt or a dead connection looks
exactly like this, and it used to bill its entire timeout before anything said so. A stall takes
the same bounded kill/reap/drain path as a timeout but is reported apart, because the remedies
differ: a timeout wants a bigger budget, a stall wants to know why the harness went quiet.
Validation commands and git are exempt — a compiler is legitimately silent for minutes.

**The raw transcript is never filtered; a streaming phase gets a rendered twin instead.**
`<label>.log` keeps every raw line — it once held the only surviving copy of a plan — but a claude
planning phase buries it under hundreds of kilobytes of machine events, so the file a human is
invited to tail is `<label>.progress.log`: the terminal's rendered lines, stderr, the heartbeat,
and how the run ended. Only a streaming backend gets one (`AgentAdapter::progress_log`); for a
passthrough harness the raw log is already readable and a twin would leave two identical files and
a user guessing which to trust.

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

**Planning is skipped by the user, never by a heuristic.** `kage run --skip-plan` starts the run at
EXECUTE with the task as the executor's whole instruction. The person typing the task already knows
whether it needs design, and a guess is expensive in both directions. TEST and REVIEW still run:
skipping the plan must not mean skipping the gate. The executor and reviewer prompts are told there
is no plan rather than handed an empty one — "PLAN.md is missing" reads to an agent as a fault to
report, and to a reviewer as an invitation to return BLOCKED.

**A worktree must be prepared before it can be judged.** `setup.commands` runs once after the
worktree is created. A clean checkout has no `node_modules`, so a JavaScript project's gate fails on
every phase for reasons the change did not cause. Cargo's shared registry cache hid this until Kage
was first pointed at a project it had not built.

**The gate is checked green before the run depends on it, and warns rather than refuses.** A red
gate cannot judge the work. But "fix the failing build" is a legitimate task, so the check records
the baseline in `TEST_RESULTS.md` instead of blocking — the failure must not be silent, and it must
not be charged to the change.

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

Keep this current. It is read before work starts, so a stale entry sends the next agent to fix
something already fixed — and a fixed entry left here is a lie told with authority.

- **A worktree prepared once is not prepared again.** `setup.commands` runs at `kage run`, not at
  `kage resume` — a resumed run whose worktree was rebuilt by `kage clean` starts with an empty
  `node_modules` and no setup to fill it.
- **A phase that keeps talking while going nowhere still costs its full budget.** Silence is now
  acted on (see the stall decision above), but a harness looping noisily is mechanically
  indistinguishable from one working — telling them apart needs judgment about the output, not a
  clock on it.
- **A resumed run's recreated worktree starts with no artifacts in it.** `.kage/` is never
  committed, so a worktree rebuilt on `kage resume` has no `PLAN.md` or `TEST_RESULTS.md`, and a
  prompt built there embeds their placeholders. The missing `EXECUTION.md` is re-asked for; the
  others are not restored from the project's mirror, which holds them.
- **The `# Deferred Tasks` contract has not met a live planner.** The instruction, the extraction,
  and the surfacing are tested; whether real planners honour the "omit when it fits" rule is
  exactly the kind of thing a green suite cannot prove. Watch the first oversized run.

## Where the work stands

Kage has built five of its own features, and the scoping-and-budgeting queue that followed is
clear: the live log has a readable twin, broken builds and review findings spend separate budgets,
a silent phase is aborted early, task sizing is declared by the planner at the point of use, and
cross-repo work is a recorded boundary rather than a pending gap. What remains above is small; the
next real work is the roadmap's graph engineering, which this file's own rule gates behind the
loop staying reliable.

Two lessons worth carrying into whatever comes next, both learned the expensive way:

**A fix that bounds one call and not its neighbours looks complete and is not.** The drains were
bounded after a timeout; the kill and the reap beside them were not, and a one-hour budget became
three hours and twenty-two minutes. When bounding anything, bound everything downstream of the same
decision.

**A constant cost nobody questions is where a bug hides best.** Every `cargo test` took 59 seconds
for a day, blamed on cargo, because a killed child's grandchild held the stdout pipe. It runs in two
seconds now. A number that never moves deserves the same suspicion as one that spikes.

## Definition of done

A change is done when the three validation commands pass, new behaviour has a test that would fail
without it, past bugs have a regression test that says what broke, and any user-facing promise
(`kage status` advice, README, terminal output) is actually true. Several bugs in this repository
were Kage confidently telling the user something that was not.
