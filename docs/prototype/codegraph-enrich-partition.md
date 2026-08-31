# Prototype — CodeGraph Enrich Prompt + Partitioning Layout

> **Ticket:** [#11 Prototype — CodeGraph enrich prompt + partitioning layout](https://github.com/algonacci/kage/issues/11) · **Branch:** `prototype/codegraph-enrich-partition` · **Date:** 2026-08-31
> **Status:** Throwaway — do not merge. Answers 3 questions before locking ADR-2.
> **Upstream:** `colbymchenry/codegraph` 1.5.0 (local `.codegraph/codegraph.db`, 27 files, 692 nodes, 1902 edges) + research `research/codegraph` `0360a4f` §7

---

## What This Prototypes

| # | Question | What to react to |
|---|----------|------------------|
| 1 | How does CodeGraph query output appear in a planner prompt? | `## Codebase Graph` section — file tree + top search hits + blast radius |
| 2 | How does `codegraph_impact` partition a sample task into independent sub-tasks? | Depth-2 BFS → disjoint file sets → subagent assignment |
| 3 | How are per-subagent artifacts laid out? | `.kage-run/subagents/<id>/` + `.kage-run/shared/discussion.md` |

All three are **read-only, infallible (`Option<String>`)** — `None` degrades to current behavior (no enrichment), zero risk to `Phase`/`RunState`.

---

## 1. Planner Prompt — `## Codebase Graph` Section

### 1.1 Where it injects

```rust
// src/engine/codegraph.rs — new helper, ~60 LOC, read-only
pub fn context_for_task(workdir: &Path, task: &str) -> Option<String> { … }

// src/engine/prompts.rs — at top of each prompt builder
pub fn planner(task: &str, workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String {
    let graph_ctx = codegraph::context_for_task(workdir, task).unwrap_or_default();
    format!(
        "{}\n## Task\n\n{task}\n\n{graph_ctx}\n## Required structure\n…",
        preamble("planner", workdir),
    )
}
```

If helper returns `None` (DB missing, binary absent, 2 s timeout), prompt is unchanged.

### 1.2 What it returns — example for task `"Add rate limiting to the API"`

```markdown
--- CodeGraph ---

## Codebase Map

**File tree** (maxDepth 3, 27 files):

```
src/
├── adapters/
│   ├── api.rs          (25 nodes)
│   ├── cli.rs          (38 nodes)
│   ├── mod.rs          (23 nodes)
│   ├── preflight.rs    (24 nodes)
│   ├── proc.rs         (41 nodes)
│   └── stream.rs       (18 nodes)
├── cli/
│   ├── doctor.rs       (12 nodes)
│   ├── mod.rs          (34 nodes)
│   └── status.rs       (28 nodes)
├── config/
│   ├── mod.rs          (22 nodes)
│   └── schema.rs       (45 nodes)
├── engine/
│   ├── gates.rs        (18 nodes)
│   ├── prompts.rs      (32 nodes)
│   ├── runner.rs       (22 nodes)
│   └── workflow.rs     (89 nodes)
├── git/
│   ├── commit.rs       (20 nodes)
│   ├── diff.rs         (15 nodes)
│   └── worktree.rs     (18 nodes)
├── state/
│   ├── run.rs          (31 nodes)
│   └── store.rs        (28 nodes)
├── main.rs             (18 nodes)
└── paths.rs            (8 nodes)
```

**Top hits for "rate limiting API"** (`codegraph explore` — 1 call):

> Found 12 symbols across 4 files. Blast radius shown.

- `runner.rs` — `validate()` (src/engine/runner.rs:45) — 2 callers in `workflow.rs`; ⚠️ no covering tests
- `api.rs` — `ApiAdapter::run()` (src/adapters/api.rs:130) — 3 callers; blast radius: `workflow.rs`, `adapters/mod.rs`
- `proc.rs` — `run()` (src/adapters/proc.rs:112) — 8 callers; blast radius: `runner.rs`, `workflow.rs`, `git/*`

**Verbatim source** (grouped by file, line-numbered, from `codegraph explore`):

```rust
// src/engine/runner.rs:45 — validate()
pub async fn validate(workdir: &Path, config: &Config) -> TestReport { … }

// src/adapters/api.rs:130 — ApiAdapter::run()
async fn run(&self, req: AgentRequest) -> Result<AgentResult> { … }
```

--- End CodeGraph ---
```

**Token budget:** ~800 tokens cap (5 hits + file tree). Truncate with `… (truncated)` marker. Wrapped in `--- CodeGraph ---` delimiters so task text cannot inject `## Verdict`.

### 1.3 Per-role injection

| Role | Query | Injected Section | When |
|------|-------|------------------|------|
| **Planner** | `codegraph explore <task>` + `codegraph files --json` (maxDepth 3) | `## Codebase Map` — file tree + top 5 hits with `file:line` + blast radius | Every plan |
| **Reviewer** | `codegraph impact <symbol> --depth 2 --json` on symbols from `diff` (`+++ b/` parse) | `## Impact Analysis` — dependents beyond the diff | Every review |
| **Fixer** | Same as reviewer (diff-scoped) | `## Impact of Current Change` — narrow, keep fix small | Every fix |
| **Executor** | `codegraph explore` on plan's `# Files` table | `## Relevant Source` — verbatim blocks for files the plan names | When plan exists |
| **TEST (future)** | `codegraph affected <changed files> --stdin --quiet` | Select `cargo test` subset or annotate `TEST_RESULTS.md` | When `affected` available |

---

## 2. Partitioning — `codegraph_impact` → Independent Sub-Tasks

### 2.1 Sample task

> **Task:** `"Add auth middleware to API and add health check endpoint"`

Planner sees this is 2 coherent pieces (auth vs health) — candidate for subagent partitioning.

### 2.2 How `codegraph impact` determines independence

```bash
# For each candidate symbol, depth-2 BFS via CLI (verified on this repo):
codegraph impact "ApiAdapter" --depth 2 --json   # → 68 nodes, 121 edges, affected filePaths
codegraph impact "validate"   --depth 2 --json   # → 12 nodes, 8 edges, affected filePaths
```

**Disjoint check:**

```
impact(ApiAdapter) filePaths = {src/adapters/api.rs, src/adapters/mod.rs, src/engine/workflow.rs, src/state/run.rs}
impact(validate)   filePaths = {src/engine/runner.rs, src/engine/workflow.rs}

Overlap = {src/engine/workflow.rs}  →  NOT disjoint — same file touched → do NOT parallelize these two.
```

For a truly disjoint example:

```
impact("doctor") filePaths = {src/cli/doctor.rs, src/main.rs}
impact("status") filePaths = {src/cli/status.rs, src/state/store.rs}

Overlap = {}  →  disjoint → safe to parallelize.
```

### 2.3 Partition result — what the planner would emit

```markdown
## Partition (CodeGraph-validated)

| Subagent | Task | Files (disjoint) | Impact nodes |
|----------|------|-------------------|--------------|
| `auth` | Add auth middleware to API | `src/adapters/api.rs`, `src/adapters/mod.rs` | 68 nodes, 121 edges |
| `health` | Add health check endpoint | `src/cli/doctor.rs`, `src/engine/runner.rs` | 12 nodes, 8 edges |

Overlap check: ✅ disjoint — safe to run in parallel via `tokio::join_all`.

If overlap detected: fall back to sequential (single executor, no subagents) — no deadlock, no overwrite.
```

**Heuristic:** Depth-2 BFS, disjoint file sets. Depth 1 too narrow (misses transitive), depth 3 too broad (everything overlaps). Verified: `RunState` depth 2 = 68 nodes — manageable.

### 2.4 When partitioning triggers

- Planner's `# Deferred Tasks` already signals oversized — subagent partitioning is the *execution* of that signal.
- Kage validates disjointness via `codegraph impact` before spawning; if not disjoint, skip subagents (single executor).
- No new `Phase` — partitioning is inside `EXECUTE`, not a new state.

---

## 3. Per-Subagent Artifacts Layout

### 3.1 Directory structure

```
.kage-run/                          # WORKTREE_ARTIFACTS — single worktree (per Q12 decision)
├── REQUEST.md                      # task (parent)
├── PLAN.md                         # plan (parent)
├── EXECUTION.md                    # aggregate — concatenated shards with headers (per Q13)
├── TEST_RESULTS.md                 # single validation on aggregate diff
├── REVIEW.md                       # single review on aggregate diff
├── VERDICT.json                    # single verdict, all-or-nothing
├── prompts/                        # per-phase prompts (parent)
├── logs/                           # per-phase logs (parent)
├── subagents/                      # per-subagent shards
│   ├── auth/
│   │   ├── EXECUTION.md            # shard — this subagent's account
│   │   ├── logs/
│   │   │   └── execute.log         # shard — raw transcript
│   │   └── meta.json               # {id, task, files, status, cost_usd}
│   └── health/
│       ├── EXECUTION.md
│       ├── logs/
│       │   └── execute.log
│       └── meta.json
└── shared/
    └── discussion.md               # append-only — hybrid channel (per Q7 decision)
```

**Why single worktree + sharded artifacts (not multi-worktree):**

- Preserves `AGENTS.md` guarantee: one `workdir`, one `base_commit`, one `git diff`, one `commit` (`kage/<run_id>`).
- No multi-branch, no merge — `git diff` is already the aggregate.
- Shards are durable (survive crash, mirrored to `.kage/runs/<id>/subagents/`), merged at collect.

### 3.2 Shared discussion — hybrid channel

```markdown
# .kage-run/shared/discussion.md — append-only, Kage-relayed

## 2026-08-31T00:30:00Z — auth
Auth middleware needs `Config::api_key_env` — adding to `config/schema.rs`.

## 2026-08-31T00:31:00Z — health
Health check will read `state.json` — no conflict with auth files (disjoint ✅).

## 2026-08-31T00:32:00Z — Kage (relay)
Both subagents acknowledged — proceeding.
```

- **File-append IS the channel** — subagents append, Kage polls and relays (no separate socket).
- CodeGraph hard pre-spawn (disjoint check) + soft prompt ("avoid files outside your partition") + post-join overlap detection (fail-fast if overlap slipped through).

### 3.3 Seam — where it lives in code

```rust
// src/state/subagent.rs — new, ~80 LOC
pub struct SubagentState { id: String, task: String, files: Vec<PathBuf>, status: SubagentStatus, cost_usd: Option<f64> }
pub enum SubagentStatus { Pending, Running, Completed, Failed(String) }

// src/state/store.rs — extend Artifacts
impl Artifacts {
    pub fn subagent_dir(&self, id: &str) -> PathBuf { self.dir.join("subagents").join(id) }
    pub fn shared_discussion(&self) -> PathBuf { self.dir.join("shared/discussion.md") }
    pub fn collect_shards(&self) -> Result<String> { /* concatenate subagents/*/EXECUTION.md with headers */ }
}

// src/engine/workflow.rs — inside EXECUTE, not a new Phase
async fn execute_with_subagents(state: &mut RunState, artifacts: &Artifacts, brief: Brief<'_>) -> Result<()> {
    // 1. CodeGraph disjoint check
    // 2. tokio::join_all + proc::run per subagent (per-child stall, parent wall-clock timeout)
    // 3. collect shards → EXECUTION.md aggregate
    // 4. single TEST + single REVIEW on aggregate diff
}
```

No new `Phase` variant — subagents are an *execution strategy* inside `EXECUTE`, not a state-machine change. Reversible: remove `subagents/` handling, loop is unchanged.

### 3.4 What `kage status` shows (additive)

```
Run: run_20260831_001 — EXECUTING (subagents: 2)
  auth    — Running   — src/adapters/api.rs, src/adapters/mod.rs
  health  — Completed — src/cli/doctor.rs, src/engine/runner.rs
  Aggregate diff: 4 files, +120 -10
  Cost: $0.42 (auth $0.28 + health $0.14)
```

Additive table — existing status unchanged when no subagents.

---

## 4. Implementation Options (from research `research/codegraph` §4.4 + §7.6)

| Option | How | Effort | When | Risk |
|--------|-----|--------|------|------|
| **C — Snapshot** | `setup.commands: ["codegraph sync --quiet"]` then read snapshot or rely on CLI at prompt time | 1 hour | Today | Zero — no Rust change |
| **A — CLI subprocess** | `tokio::process::Command::new("codegraph").args([...]).timeout(2s)` via `proc::run` | 2–3 days | Follow-up | Low — no new crate, respects `.codegraph/` location |
| **B — Direct SQLite** | `rusqlite` `SQLITE_OPEN_READONLY` on `nodes_fts` + `files` | 2–3 days + dep | Only if latency matters | Medium — schema drift risk |

All read-only, infallible, no `Phase`/`RunState` change. Recommended: **C today, A next** (same as research).

---

## 5. Risks (extends research §5 + §7.7)

| Risk | Mitigation |
|------|------------|
| Stale index (DB per-machine, not per-commit) | `codegraph sync` in `setup.commands` or before prompt; degrade to `None` if DB mtime < HEAD mtime |
| Missing binary/DB (CI, fresh clone) | `unwrap_or_default()` — current behavior, no failure |
| Token bloat | Cap 800 tokens, truncate `… (truncated)`, wrap in `--- CodeGraph ---` |
| Overlap missed (depth 2 too narrow) | Post-join overlap detection — fail-fast if `git diff` shows same file touched by 2 subagents |
| Overlap false positive (depth 2 too broad) | Fall back to sequential — safe, just not parallel |

---

## 6. References

- Research: `research/codegraph` `0360a4f` (517 lines, §7 upstream `colbymchenry/codegraph`), `research/pi-harness` `c1a460b`, `research/dag-feasibility` `1a2bbc4`
- Decisions: Grilling — Subagent lifecycle [#6](https://github.com/algonacci/kage/issues/6) (hybrid L1/L2/3, single-worktree + sharded artifacts), Hybrid communication [#7](https://github.com/algonacci/kage/issues/7) (file-append channel), Budget [#8](https://github.com/algonacci/kage/issues/8) (hybrid wall-clock + per-child stall), Aggregate review [#10](https://github.com/algonacci/kage/issues/10) (single diff + concatenated EXECUTION.md), Preservation contract [#13](https://github.com/algonacci/kage/issues/13) (14 guarantees)
- Code: `src/engine/prompts.rs` (planner/executor/reviewer/fixer), `src/state/store.rs` (`Artifacts`), `src/adapters/proc.rs` (`proc::run`), `src/config/schema.rs`, `src/engine/workflow.rs::run_phases`
- Upstream: `https://github.com/colbymchenry/codegraph` (CLI `query/explore/node/files/callers/impact/affected`, MCP `codegraph_explore`, library `CodeGraph.init`), local `codegraph 1.5.0` verified on this repo
