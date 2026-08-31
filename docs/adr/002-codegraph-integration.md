# ADR 002 — CodeGraph Integration (Read-Only Context + Partitioning, DAG Deferred)

> **Status:** Proposed (prototype — `prototype/3-adr-outlines`, throwaway, not merged) · **Date:** 2026-08-31 · **Ticket:** [#12 Prototype — 3 ADR outlines](https://github.com/algonacci/kage/issues/12) (part of [#2 Wayfinder Map](https://github.com/algonacci/kage/issues/2))
> **Deciders:** @amaldevice + research [#3](https://github.com/algonacci/kage/issues/3) (CodeGraph DB), prototype [#11](https://github.com/algonacci/kage/issues/11) (enrich prompt + partitioning), research [#5](https://github.com/algonacci/kage/issues/5) (DAG feasibility)

---

## Context

Kage has a local `.codegraph/codegraph.db` (SQLite, 27 files, 692 nodes, 1902 edges, FTS5, WAL) from `colbymchenry/codegraph` 1.5.0 (upstream Rust-kernel, 68k stars, 20+ langs, MCP+CLI+library). The DB is machine-local (`.codegraph/.gitignore` `*` + `!.gitignore`), auto-synced via watcher (debounce 2 s), and already verified via CLI:

- `codegraph query` (FTS5 scored), `explore` (1 call → source + blast radius), `node`/`files`/`callers`/`callees`/`impact`/`affected` — all verified on this repo.
- Pi tools `@vndv/pi-codegraph@0.1.10` (`codegraph_search` etc.) are a thin wrapper over the upstream — for Kage (Rust binary) the upstream CLI is the direct surface.

The question is how Kage consumes CodeGraph for (1) enriching planner/reviewer prompts and (2) partitioning tasks for subagents — without changing `Phase`/`RunState` — and whether DAG execution is in scope.

Research: `research/codegraph` `0360a4f` (517 lines, §7 upstream `colbymchenry/codegraph` — CLI/MCP/library, framework routes, RN/Expo bridging, `affected`). Prototype: `prototype/codegraph-enrich-partition` `a062bfd` (297 lines — `## Codebase Graph` section, `impact --depth 2` partitioning, per-subagent artifacts).

## Decision

**Read-only CodeGraph context via CLI (`proc::run`), per-role prompt injection, `impact --depth 2` disjoint partitioning for subagents, DAG deferred to v1.x opt-in.**

### Prompt Enrichment — Read-Only, Infallible

```rust
// src/engine/codegraph.rs — new helper, ~60 LOC, read-only, infallible
pub fn context_for_task(workdir: &Path, task: &str) -> Option<String> { … }
pub fn impact_for_diff(workdir: &Path, diff: &str) -> Option<String> { … }
```

Called at top of each prompt builder in `src/engine/prompts.rs`:

```rust
pub fn planner(task: &str, workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String {
    let graph_ctx = codegraph::context_for_task(workdir, task).unwrap_or_default();
    format!("{preamble}\n## Task\n\n{task}\n\n{graph_ctx}\n## Required structure\n…")
}
```

`None` (DB missing, binary absent, 2 s timeout) → prompt unchanged — zero risk to loop, no `Phase`/`RunState` change.

| Role | Query | Injected Section | When |
|------|-------|------------------|------|
| **Planner** | `codegraph explore <task>` + `codegraph files --json` (maxDepth 3) | `## Codebase Map` — file tree + top 5 hits with `file:line` + blast radius | Every plan |
| **Reviewer** | `codegraph impact <symbol> --depth 2 --json` on symbols from `diff` (`+++ b/` parse) | `## Impact Analysis` — dependents beyond the diff | Every review |
| **Fixer** | Same as reviewer (diff-scoped) | `## Impact of Current Change` — narrow, keep fix small | Every fix |
| **Executor** | `codegraph explore` on plan's `# Files` table | `## Relevant Source` — verbatim blocks for files the plan names | When plan exists |
| **TEST (future)** | `codegraph affected <changed files> --stdin --quiet` | Select `cargo test` subset or annotate `TEST_RESULTS.md` | When `affected` available |

**Token budget:** ~800 tokens cap (5 hits + file tree), truncate with `… (truncated)`, wrap in `--- CodeGraph ---` delimiters (prevents task injection of `## Verdict`).

**Implementation options (in order):**

| Option | How | Effort | Risk |
|--------|-----|--------|------|
| **C — Snapshot** | `setup.commands: ["codegraph sync --quiet"]` then read snapshot or rely on CLI at prompt time | 1 hour | Zero — no Rust change |
| **A — CLI subprocess** | `tokio::process::Command::new("codegraph")` via `proc::run` (timeout 2 s) | 2–3 days | Low — no new crate |
| **B — Direct SQLite** | `rusqlite` `SQLITE_OPEN_READONLY` on `nodes_fts` + `files` | 2–3 days + dep | Medium — schema drift |

Recommended: **C today, A next** (A is the follow-up, B only if latency matters).

### Partitioning — `impact --depth 2` Disjoint Check

For subagent partitioning (ADR 001), CodeGraph determines independence:

```bash
codegraph impact "ApiAdapter" --depth 2 --json  # → 14 nodes, 13 edges, filePaths
codegraph impact "validate"   --depth 2 --json  # → 19 nodes, 17 edges, filePaths
# Overlap = filePaths intersection → if empty, disjoint → safe to parallelize
# If overlap, fallback sequential (single executor) — no deadlock, no overwrite
```

- Depth 2 BFS — depth 1 too narrow (misses transitive), depth 3 too broad (everything overlaps). Verified: `RunState` depth 2 = 68 nodes/121 edges — manageable.
- Hard pre-spawn (disjoint check) + soft prompt ("avoid files outside your partition") + post-join overlap detection (fail-fast if `git diff` shows same file touched by 2 subagents).

### DAG — Deferred to v1.x Opt-In

**Full DAG (`Phase → Graph {nodes, edges}` + topo sort + `JoinSet` scheduler + per-node Artifacts/worktree + `GraphConfig`) is feasible but cross-cutting (4–6 weeks, highest risk in scheduler + per-node isolation/merging + `resume`/`status`) — P2 deferred to v1.x opt-in `kage run --graph plan.yaml`, not default, not earlier.**

- Per-worktree guarantees (isolation, `base_commit`/`git diff`/review scope/`commit`/`kage clean`) redefined per-node/per-graph.
- `deferred_tasks` retained for linear, superseded by `--graph` in DAG mode.
- Additive `Option<GraphState>` for reversibility — linear loop unchanged when `--graph` not used.
- Research: `research/dag-feasibility` `1a2bbc4` (429 lines — rewrite inventory, effort/risk matrix).

## Alternatives Considered

| Alternative | Why rejected |
|-------------|--------------|
| Direct SQLite (`rusqlite`) as primary | Schema drift risk (table layout changes across codegraph versions), adds dep, CLI is stable API |
| Library (`@colbymchenry/codegraph` Node 22.5+ in-process) | Kage is Rust single binary (`cargo install`) — requires Node 22.5+ in-process, per-platform package, not justified |
| MCP client in Kage | MCP is for agents (Claude/Cursor/Codex) — Kage *drives* those agents, should not itself be MCP client |
| DAG now (not deferred) | Cross-cutting rewrite (every invariant: `Phase` single cursor, `RunState` single worktree/diff/commit, `Artifacts` single mirror, `worktree.rs` single branch), multiplies P0 gaps (`# Deferred Tasks` live validation, noisy-loop) |
| No CodeGraph (status quo) | Planner has no RepoMap, reviewer has no impact analysis, subagent partitioning has no disjoint validation — all manual/heuristic |

## Consequences

- **Positive:** Planner has RepoMap (file tree + hits + blast radius), reviewer has impact analysis beyond diff, subagents have CodeGraph-validated disjointness, TEST can select `cargo test` subset via `affected`, all read-only and reversible (remove helper, loop unchanged).
- **Negative:** `codegraph` CLI must be on PATH (or degrade to `None`), 2 s timeout per prompt, ~800-token overhead, DB per-machine not per-commit (stale index risk).
- **Neutral:** DAG remains P2 — linear loop is the default until v1.x proves graph scheduler.

## Preservation

Per [#13 Preservation contract](https://github.com/algonacci/kage/issues/13):

- No `Phase`/`RunState` change — CodeGraph is prompt enrichment, not state.
- `Artifacts` not chat history — CodeGraph output is prompt context, not artifact.
- Worktree isolation — CodeGraph reads `.codegraph/` outside `Artifacts::dir`, no worktree change.
- Bounded execution — 2 s timeout, `None` fallback, no budget impact.

Verification: `cargo test` + `cargo clippy` + `cargo fmt` + `codegraph status` (index up to date, `Journal: wal`) + regression `codegraph_context_degrades_to_none_when_db_missing`.

## References

- PRD §7.2 (Graph/CodeGraph — read-only context + DAG)
- Research `research/codegraph` `0360a4f` (517 lines, §7 upstream `colbymchenry/codegraph` — CLI `query/explore/node/files/callers/impact/affected`, MCP `codegraph_explore`, library `CodeGraph.init`, framework routes, RN/Expo bridging, `affected`), `research/dag-feasibility` `1a2bbc4`
- Prototype `prototype/codegraph-enrich-partition` `a062bfd` (297 lines — `## Codebase Graph`, `impact --depth 2` partitioning, per-subagent artifacts)
- Decisions [#13](https://github.com/algonacci/kage/issues/13) (preservation), [#6](https://github.com/algonacci/kage/issues/6) (lifecycle — CodeGraph disjoint), [#7](https://github.com/algonacci/kage/issues/7) (communication — CodeGraph enforcement)
- Code: `src/engine/prompts.rs`, `src/state/store.rs` (`Artifacts`), `src/adapters/proc.rs` (`proc::run`), `src/engine/codegraph.rs` (new), `src/engine/workflow.rs`
- Upstream: `https://github.com/colbymchenry/codegraph` (CLI/MCP/library, `codegraph.json` config, troubleshooting)
