# ADR 001 — Subagents L1+2+3 Across All Phases

> **Status:** Proposed (prototype — `prototype/3-adr-outlines`, throwaway, not merged) · **Date:** 2026-08-31 · **Ticket:** [#12 Prototype — 3 ADR outlines](https://github.com/algonacci/kage/issues/12) (part of [#2 Wayfinder Map](https://github.com/algonacci/kage/issues/2))
> **Deciders:** @amaldevice + decisions [#6](https://github.com/algonacci/kage/issues/6) (lifecycle), [#7](https://github.com/algonacci/kage/issues/7) (communication), [#8](https://github.com/algonacci/kage/issues/8) (budget), [#10](https://github.com/algonacci/kage/issues/10) (aggregate review), [#13](https://github.com/algonacci/kage/issues/13) (preservation)

---

## Context

Kage's loop is `TASK → PLAN → EXECUTE → TEST → REVIEW → DECISION → DONE` with one planner, one executor, one reviewer (`AGENTS.md`). PRD §7.3 proposes three subagent levels:

- **L1** — harness-internal (claude/codex spawn via their own tools, Kage unaware)
- **L2** — Kage-orchestrated parallel (`tokio::join_all` + `proc::run`, per-subagent shards)
- **L3** — L2 + inter-subagent discussion (shared artifacts + message passing + CodeGraph partitioning)

The question is how L1/L2/L3 compose across **all phases** (not just EXECUTE), how they communicate, how budget/stall applies, and how review aggregates — without breaking `AGENTS.md` guarantees.

Research: `research/codegraph` `0360a4f` (partitioning via `impact --depth 2`), `research/dag-feasibility` `1a2bbc4` (DAG deferred). Prototype: `prototype/codegraph-enrich-partition` `a062bfd` (enrich prompt + partitioning layout, 297 lines).

## Decision

**Hybrid L1+L2+L3, all phases, single-worktree + sharded artifacts, CodeGraph-validated partitions, hybrid communication, parent-only dual budget, aggregate-diff single review.**

### Lifecycle (grilling #6)

| Level | Who spawns | When | Kage involvement |
|-------|------------|------|------------------|
| **L1** | Harness (claude/codex) via its own tools | Any phase, harness decides | None — Kage unaware, zero-cost, already happens today |
| **L2** | Kage (`tokio::join_all` + `proc::run` per subagent) | Inside `EXECUTE` when planner's `# Deferred Tasks` signals oversized + CodeGraph disjoint check passes | Full — `SubagentState` in `RunState`, per-subagent shards, collect at `EXECUTE` |
| **L3** | Kage (L2) + inter-subagent discussion | Same as L2, plus shared `discussion.md` | Full — file-append channel, Kage-relayed |

- **All phases** have distinct semantics: planner debate (2 architectures → consensus), executor parallel (disjoint files), reviewer single (aggregate diff). No new `Phase` variant — subagents are an *execution strategy* inside `EXECUTE` (and optionally `PLANNING` debate), not a state-machine change.
- **Trigger:** Planner's `# Deferred Tasks` presence = oversized signal → Kage validates disjointness via `codegraph impact --depth 2` → if disjoint, spawn; if not, fallback sequential (single executor).
- **Glossary:** `subagent` = Kage-spawned child handling a partition; `parent run` = the `RunState` owning the worktree; `partition` = disjoint file set from `impact` depth-2 BFS.

### Communication (grilling #7)

**File-append IS the channel** — no separate socket.

- **Shards:** `.kage-run/subagents/<id>/EXECUTION.md` + `logs/execute.log` + `meta.json` (`{id, task, files, status, cost_usd}`) — durable, mirrored to `.kage/runs/<id>/subagents/`, merged at collect.
- **Shared:** `.kage-run/shared/discussion.md` — append-only, Kage polls and relays (subagents append, Kage forwards to others). No `subagent_message` socket.
- **Enforcement:** Hard pre-spawn (disjoint check) + soft prompt ("avoid files outside your partition") + post-join overlap detection (fail-fast if `git diff` shows same file touched by 2 subagents).
- **Seam:** `src/state/subagent.rs` (~80 LOC, `SubagentState`/`SubagentStatus`) + `Artifacts::subagent_dir/shared_discussion/collect_shards` in `src/state/store.rs`.

### Budget & Stall (grilling #8)

| Concern | Decision |
|---------|----------|
| **Timeout** | Hybrid wall-clock — parent `timeout_secs` is the wall-clock for `join_all`, no N×. Subagents share it. |
| **Stall** | Per-child `stall_secs` (600 s default) + bounded `kill/reap/drain` per child (15 s/20 s/10 s). Global stall = all children silent. |
| **Dual budget** | Parent-only — single `TEST` + single `REVIEW` on aggregate diff, single `max_iterations`/`max_repairs` with refill. No per-subagent budget (no N× cost explosion). |
| **Cost** | Sum `total_cost_usd` across shards (G4 graduated), cap + tiering (3/5 anti-explosion), `kage status` additive shard table. |
| **Partial failure** | Fail-fast — one subagent FAIL → aggregate FAIL, no partial review. |

### Review (grilling #10)

- **Single `git diff` aggregate** + partition map header (which subagent touched which files).
- **Concatenated `EXECUTION.md` shards** with `## Subagent <id>` headers.
- **Single `VERDICT.json` all-or-nothing** — `PASS` only if all shards pass, issues aggregated.
- **Single validation** on aggregate diff (not per-shard).
- **Additive `kage status`** — shard table when subagents exist, unchanged otherwise.
- **Fail-fast** — no partial review, no per-shard verdict.

## Alternatives Considered

| Alternative | Why rejected |
|-------------|--------------|
| L1 only (harness-internal, no Kage orchestration) | Zero Kage value — already happens, no partitioning, no cost tracking, no review aggregation |
| Multi-worktree per subagent (`kage/<run_id>/<sub_id>`) | Breaks `AGENTS.md` guarantee: one `workdir`/`base_commit`/`git diff`/`commit`; multi-branch merge complexity, `kage clean`/`resume` ambiguous |
| New `Phase` variants (`Spawning`/`Collecting`) | State-machine change, not reversible, violates "subagents are execution strategy" — deferred to DAG v1.x if needed |
| Per-subagent dual budget | N× cost explosion (`max_iterations` × subagents), hard to track, violates bounded execution |
| Per-shard `VERDICT.json` + aggregate verdict | N reviewer calls (premium model × shards), complex aggregation, violates single-gate determinism |

## Consequences

- **Positive:** Parallelism for disjoint tasks (auth vs health), CodeGraph-validated safety, single-worktree simplicity, reversible (remove `subagents/` handling, loop unchanged), additive `kage status`.
- **Negative:** `EXECUTE` becomes more complex (disjoint check + `join_all` + collect), `RunState` gains `Option<Vec<SubagentState>>`, `Artifacts` extended.
- **Neutral:** L1 remains zero-cost (no Kage change), L2/L3 opt-in via disjoint check — sequential fallback is always safe.

## Preservation

Per [#13 Preservation contract](https://github.com/algonacci/kage/issues/13) — this ADR preserves:

- Roles never models (subagents inherit parent's `RoleConfig`)
- Artifacts not chat history (file-based handoff, shards + `discussion.md`)
- Worktree isolation (single worktree, one `base_commit`/`git diff`/`commit`)
- `VERDICT.json` PASS/FAIL/BLOCKED gate (single, all-or-nothing)
- `has_content` guard + re-ask (per-shard + aggregate)
- Stall detection + bounded kill/reap/drain (per-child)
- Baseline gate (single, on aggregate)
- Bounded execution (parent-only dual budget, cap+tiering)

Verification: `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` + named regression tests (`subagent_disjoint_check_blocks_overlap`, `subagent_shards_survive_crash`, `aggregate_verdict_all_or_nothing`).

## References

- PRD §7.3 (Spawn Subagents — 3 levels, hybrid discussion)
- Research `research/codegraph` `0360a4f` §7 (upstream `colbymchenry/codegraph`, `impact --depth 2`), `research/dag-feasibility` `1a2bbc4`
- Prototype `prototype/codegraph-enrich-partition` `a062bfd` (enrich prompt + partitioning layout)
- Decisions [#6](https://github.com/algonacci/kage/issues/6), [#7](https://github.com/algonacci/kage/issues/7), [#8](https://github.com/algonacci/kage/issues/8), [#10](https://github.com/algonacci/kage/issues/10), [#13](https://github.com/algonacci/kage/issues/13)
- Code: `src/engine/workflow.rs::run_phases`, `src/state/run.rs` (`Phase`/`RunState`), `src/state/store.rs` (`Artifacts`), `src/adapters/proc.rs` (`proc::run`), `src/engine/prompts.rs`
