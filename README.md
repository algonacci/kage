# Kage

Engineering workflow orchestrator for AI coding agents.

Kage does not implement a coding agent. It drives the ones you already have installed:

```text
TASK -> PLAN -> EXECUTE -> TEST -> REVIEW -> DECISION -> DONE
                            ^                    |
                            +------- FIX <-------+
```

The idea is to spend intelligence where it has leverage. Architecture and review are worth a premium
model; reading files, editing code, and re-running tests are not.

```bash
kage run "Implement rate limiting for the API"
```

A premium model plans, a cheap model implements, your test suite decides whether it works, and a
third model reviews. If the review fails, the executor fixes the named findings and the loop runs
again — up to a hard iteration cap, so a run can never spin forever.

## Install

```bash
cargo install --path .
```

## Quick start

```bash
cd your-project
kage init
kage doctor
kage run "Add a health check endpoint"
```

`kage init` writes `.kage/config.yaml` and guesses your validation commands from the repository
(Cargo, npm/pnpm/yarn, Go, or pytest). `kage doctor` reports which harnesses are installed and
whether the project is ready.

## Commands

| Command | What it does |
| ------- | ------------ |
| `kage init` | Create `.kage/` with a starter config |
| `kage run "<task>"` | Run the full loop on a task |
| `kage status [run_id]` | Show a run's state, artifacts, and history |
| `kage status --all` | List every run |
| `kage resume [run_id]` | Continue an interrupted run |
| `kage clean [--all]` | Remove worktree checkouts of finished runs (branches are kept; a run whose work was never committed is kept unless `--all` is passed) |
| `kage doctor` | Check which tools are available |

`kage run` exits non-zero when a run does not complete, so it chains into scripts and CI.

Before the first phase, `kage run` and `kage resume` resolve the program behind every role and abort
with the same report `kage doctor` gives if one is unreachable — so a run cannot spend a planner and
an executor before discovering the reviewer was never installed.

## Configuration

Roles are bound to harnesses in `.kage/config.yaml`. The workflow never names a model.

```yaml
roles:
  planner:
    adapter: claude-code
    model: opus

  executor:
    adapter: opencode

  reviewer:
    adapter: codex

loop:
  max_iterations: 3

git:
  isolate: true

validation:
  commands:
    - cargo test
    - cargo clippy --all-targets -- -D warnings
```

Adapters: `claude-code`, `codex`, `opencode`, `kamui`, and `command`. The last one runs any binary
that takes a prompt:

```yaml
executor:
  adapter: command
  command: ['my-agent', '--yolo', '{prompt}']
  env:
    MY_AGENT_MODE: fast
```

`{prompt}` is replaced with the instruction and `{prompt_file}` with the path to it. By default the
prompt is written to a file and the agent is given a one-line pointer, which keeps the command line
small no matter how large the plan grows. Set `prompt_delivery: arg` or `stdin` to change that.

Leave `model` unset to use whatever the harness is already configured with, which is usually what
you want with a subscription CLI. A name the harness does not recognise fails the whole run, so only
set one you have verified with that tool — `claude --model` takes `opus` or `claude-opus-5`, and not
every spelling of a model id works.

Per-role `timeout_secs` (default 1800) and `extra_args` are available for harness flags Kage does
not model.

> On Windows, quote config paths with single quotes. A double-quoted YAML string treats `\U` in
> `C:\Users\...` as an escape sequence and fails to parse.

Kage owns none of these tools' credentials. Authentication stays each harness's business.

## How it works

Agents share no chat history. Context moves between them through artifacts on disk, so three
different harnesses with three different context windows can take part in one workflow, and a run
that crashes can be resumed and inspected afterwards.

```text
.kage/runs/run_20260809_001/
├── REQUEST.md          what you asked for
├── PLAN.md             the executable engineering contract
├── EXECUTION.md        what the executor says it did
├── TEST_RESULTS.md     what your test suite says actually happened
├── REVIEW.md           the reviewer's findings
├── VERDICT.json        PASS | FAIL | BLOCKED
├── state.json          resumable run state
├── prompts/            exactly what each agent was told
└── logs/               what each agent printed, written live
```

Each agent's output is streamed to the terminal as it arrives — claude's events are rendered as
short progress lines rather than JSON — and its log under `logs/` is written line by line, so
`tail -f` (or `Get-Content -Wait`) shows what the agent is doing while the phase is still running.
If an agent goes silent for 30 seconds, Kage prints an elapsed-time line so a long think is not
mistaken for a hang.

The reviewer must emit a machine-readable verdict. A review with no verdict blocks the run rather
than being guessed at, because guessing `PASS` would let unreviewed code through.

Validation commands run in Kage's own process, not inside an agent. An agent reporting "all tests
pass" is a claim; an exit code is evidence.

### Git safety

By default agents work in a dedicated git worktree under `.kage/worktrees/`, on a branch named
`kage/<run_id>`. Your working tree is never touched, and nothing is merged for you:

```bash
git diff <base>            # review what the agents did
git merge kage/run_20260809_001
```

When a run finishes — whether it completed, failed, or was blocked — Kage commits the worktree to
`kage/<run_id>`, excluding Kage's own `.kage/` directory, so the branch really does hold the work
after `kage clean` removes the checkout. The commit is a preservation snapshot: it skips hooks and
signing (`--no-verify`, `commit.gpgsign=false`), so it can be amended, re-signed, or reworded before
you merge it. It captures whatever the agents wrote, secrets and all — the same `.gitignore` and
reviewer's diff that governed the run still apply, but the branch is a real commit of the tree.

Use `--no-isolate` to let agents edit your working tree directly; Kage then commits nothing, so the
changes stay uncommitted in your tree.

### RTK

If [RTK](https://github.com/rtk-ai/rtk) is installed, simple validation commands are automatically
prefixed with `rtk` so their output is compressed before it reaches the fixer and reviewer prompts.
RTK is optional; commands containing shell operators and systems without RTK run directly, and
`TEST_RESULTS.md` always records the command that actually ran.

## Scope

Kage v0.1 is one planner, one executor, one reviewer. No DAGs, no parallel agents, no dynamic
delegation — none of that is worth building before this line is reliable end to end.

Kage is an engineering workflow orchestrator, not a general assistant. Personal memory, email,
calendar, and chat belong in [Kamui](https://github.com/algonacci/kamui) and
[Kumo](https://github.com/algonacci/kumo). Kage is fully usable without either.

## License

MIT
