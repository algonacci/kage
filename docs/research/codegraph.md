# Research: CodeGraph DB Schema & Query API — Ticket #3

> Branch: `research/codegraph` · Date: 2026-08-31 · DB: `.codegraph/codegraph.db` (SQLite, codegraph 1.5.0, extraction v24)

## 1. SQLite Schema

### 1.1 Overview

| Fact | Value |
|------|-------|
| File | `.codegraph/codegraph.db` (2.4 MB) + `-shm`/`-wal` |
| Version | `1.5.0` (`project_metadata.indexed_with_version`), extraction `24` |
| Index state | `complete` — 27 files discovered/accounted |
| Counts (this repo) | `nodes` 692 · `edges` 1902 · `files` 27 · `unresolved_refs` 2314 · `nodes_fts` 692 |
| `.codegraph/.gitignore` | `*` + `!.gitignore` — DB, daemon pid/sockets/logs are machine-local, never committed |

```sh
sqlite3 .codegraph/codegraph.db "SELECT name, type FROM sqlite_master ORDER BY type, name;"
sqlite3 .codegraph/codegraph.db ".schema"
```

### 1.2 Tables

#### `nodes` — one row per symbol

```sql
CREATE TABLE nodes (
    id TEXT PRIMARY KEY,              -- "<kind>:<hash>" e.g. "function:dae17d9f05e4e0f6d695e442cd5421e3"
    kind TEXT NOT NULL,               -- function|method|struct|enum|enum_member|trait|variable|import|file
    name TEXT NOT NULL,               -- short name
    qualified_name TEXT NOT NULL,     -- e.g. "Role::Planner" for enum_member
    file_path TEXT NOT NULL,          -- repo-relative, e.g. "src/engine/prompts.rs"
    language TEXT NOT NULL,           -- rust|yaml
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    docstring TEXT,                   -- extracted doc comment
    signature TEXT,                   -- e.g. "(task: &str, workdir: &Path) -> String"
    visibility TEXT,                  -- public|private|null
    is_exported INTEGER DEFAULT 0,
    is_async INTEGER DEFAULT 0,
    is_static INTEGER DEFAULT 0,
    is_abstract INTEGER DEFAULT 0,
    decorators TEXT,                  -- JSON array
    type_parameters TEXT,             -- JSON array
    return_type TEXT,                 -- normalized return type
    updated_at INTEGER NOT NULL
);
```

Kind breakdown in this repo: `function` 381, `import` 107, `method` 72, `enum_member` 42, `struct` 31, `file` 25, `variable` 21, `enum` 12, `trait` 1. Indexes on `kind`, `name`, `qualified_name`, `file_path`, `language`, `(file_path, start_line)`, `lower(name)`.

Sample row (`planner`):

```
id: function:4ee855a81e85fe2985252c9dd8974661
kind: function  name: planner  qualified_name: planner
file_path: src/engine/prompts.rs  language: rust  startLine: 76  endLine: 123
signature: (task: &str, workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String
```

#### `edges` — directed relationships between nodes

```sql
CREATE TABLE edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,   -- FK nodes(id) ON DELETE CASCADE
    target TEXT NOT NULL,   -- FK nodes(id) ON DELETE CASCADE
    kind TEXT NOT NULL,     -- calls|contains|references|instantiates|imports|implements
    metadata TEXT,          -- JSON object, e.g. {"valueRef":true}
    line INTEGER,
    col INTEGER,
    provenance TEXT DEFAULT NULL,
    FOREIGN KEY (source) REFERENCES nodes(id),
    FOREIGN KEY (target) REFERENCES nodes(id)
);
CREATE UNIQUE INDEX idx_edges_identity ON edges(source, target, kind, IFNULL(line,-1), IFNULL(col,-1));
```

Edge-kind breakdown: `calls` 816, `contains` 735, `references` 227, `instantiates` 81, `imports` 41, `implements` 2. Indexes on `kind`, `(source,kind)`, `(target,kind)`, `provenance`. `contains` edges connect `file:<path>` → child symbols.

#### `files` — indexed file inventory

```sql
CREATE TABLE files (
    path TEXT PRIMARY KEY,        -- repo-relative
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL,
    size INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL,
    node_count INTEGER DEFAULT 0,
    errors TEXT                   -- JSON array
);
```

27 files: 25 Rust + 2 YAML. `node_count` ranges 0–70 (`src/adapters/proc.rs` is largest at 70 nodes).

#### `nodes_fts` — FTS5 full-text index

```sql
CREATE VIRTUAL TABLE nodes_fts USING fts5(
    id, name, qualified_name, docstring, signature,
    content='nodes', content_rowid='rowid'
);
-- triggers nodes_ai / nodes_ad / nodes_au keep FTS in sync
```

Aux tables: `nodes_fts_data`, `nodes_fts_idx`, `nodes_fts_docsize`, `nodes_fts_config`. Powers `codegraph_search` ranking (BM25 `score` field).

#### `unresolved_refs` — references that could not be resolved

```sql
CREATE TABLE unresolved_refs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_node_id TEXT NOT NULL,   -- FK nodes(id)
    reference_name TEXT NOT NULL,
    reference_kind TEXT NOT NULL, -- imports|calls|...
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    candidates TEXT,              -- JSON array
    file_path TEXT NOT NULL DEFAULT '',
    language TEXT NOT NULL DEFAULT 'unknown',
    status TEXT NOT NULL DEFAULT 'pending',  -- pending|failed
    name_tail TEXT NOT NULL DEFAULT ''
);
```

2314 rows, mostly external imports (`std`, `anyhow`, etc. with `status='failed'`). Indexes on `from_node_id`, `reference_name`, `file_path`, `(from_node_id, reference_name)`, `status`, `name_tail WHERE status='failed'`.

#### Other tables

| Table | Purpose |
|-------|---------|
| `project_metadata` | `index_state`, `indexed_with_version`, `indexed_with_extraction_version`, `index_files_discovered/accounted` |
| `schema_versions` | `version`, `applied_at`, `description` — currently `1` and `8` |
| `name_segment_vocab` | `(segment, name) WITHOUT ROWID` — vocabulary for name segmentation |
| `sqlite_sequence` / `sqlite_stat1` | SQLite internals |

### 1.3 Sample Queries (direct SQLite, read-only)

```sql
-- Find all functions in a file
SELECT name, start_line, signature FROM nodes
 WHERE file_path='src/engine/prompts.rs' AND kind='function';

-- Callers of a symbol (via edges)
SELECT n.name, n.file_path FROM edges e JOIN nodes n ON n.id = e.source
 WHERE e.target = 'function:4ee855a81e85fe2985252c9dd8974661' AND e.kind='calls';

-- FTS search
SELECT id, name, rank FROM nodes_fts WHERE nodes_fts MATCH 'planner' ORDER BY rank LIMIT 5;

-- File map
SELECT path, language, node_count FROM files ORDER BY path;
```

---

## 2. Query API — Pi Tools & CLI

### 2.1 Source: `@vndv/pi-codegraph@0.1.10`

Declared in `~/.pi/agent/settings.json` `packages`. Extension file:

```
~/.pi/agent/npm/node_modules/@vndv/pi-codegraph/extensions/codegraph.ts
```

Mechanism: spawns `codegraph serve --mcp --path <cwd>` as a child process, speaks JSON-RPC over stdin/stdout (`tools/call`), returns `content[].text`. Windows uses PowerShell shim discovery. Guidance injected via `before_agent_start` system-prompt addition.

Binary: `codegraph` 1.5.0 at `~/.local/state/fnm_multishells/.../bin/codegraph` (also via `npx`).

### 2.2 Tool Definitions (8 tools)

| Tool | Description | Inputs | Output |
|------|-------------|--------|--------|
| `codegraph_search` | Quick symbol search by name. Returns locations only. | `query: string` (required), `kind?: "function"|"method"|"class"|"interface"|"type"|"variable"|"route"|"component"`, `limit?: number` (default 10), `projectPath?: string` | Ranked list of `{node, score}` — id, kind, name, qualifiedName, filePath, language, lines, signature, docstring, score |
| `codegraph_callers` | Find all functions/methods that call a symbol. | `symbol: string` (required), `limit?: number` (default 20), `projectPath?: string` | `{symbol, callers: [{name, kind, filePath, startLine}]}` |
| `codegraph_callees` | Find all functions/methods that a symbol calls. | `symbol: string`, `limit?: number` (default 20), `projectPath?: string` | `{symbol, callees: [...]}` |
| `codegraph_impact` | Analyze impact radius of changing a symbol. | `symbol: string`, `depth?: number` (default 2), `projectPath?: string` | `{symbol, depth, nodeCount, edgeCount, affected: [{name, kind, filePath, startLine}]}` |
| `codegraph_explore` | Return source for several related symbols grouped by file. | `query: string` (symbols/files/terms), `maxFiles?: number` (default 12), `projectPath?: string` | Markdown with blast-radius header + verbatim file blocks (line-numbered, byte-identical to Read) |
| `codegraph_node` | One symbol's details + callers/callees trail, or file read. | `symbol?: string`, `includeCode?: boolean` (default false), `projectPath?: string`; file mode: `file`, `offset`, `limit`, `symbols-only` | Symbol header + signature + source + `Calls →` / `Called by ←` trails |
| `codegraph_files` | Project file structure from index. | `path?: string`, `pattern?: string` (glob), `format?: "tree"|"flat"|"grouped"` (default tree), `includeMetadata?: boolean` (default true), `maxDepth?: number`, `projectPath?: string` | File list with `path, language, nodeCount, size` (tree/flat/grouped); `annotateFilesResult` adds hint when empty |
| `codegraph_status` | Index status. | `projectPath?: string` | Index state, version, file counts |

CLI equivalents (same outputs, `--json` for machine-readable):

```sh
codegraph query <search>    [-k kind] [-l limit] [-j]
codegraph callers <symbol>  [-l limit] [-j]
codegraph callees <symbol>  [-l limit] [-j]
codegraph impact <symbol>   [-d depth] [-j]
codegraph explore <query...> [--max-files N]
codegraph node [name]       [-f file] [--offset N] [--limit N] [--symbols-only]
codegraph files             [--filter dir] [--pattern glob] [--format tree|flat|grouped] [--max-depth N] [--no-metadata] [-j]
codegraph status
```

### 2.3 Verified Outputs (this repo)

`codegraph query "planner" --json` → 4+ hits, top score 97.5 (`function:planner` in `prompts.rs:76`).
`codegraph impact "planner" --json` → `nodeCount:4, edgeCount:3, affected: [planner + 3 test functions]`.
`codegraph callers "planner" --json` → 3 callers (all tests).
`codegraph files --json` → 27 entries with language/size/nodeCount.
`codegraph explore "planner reviewer prompts"` → 26 symbols, 1 file, blast-radius header + full source block.
`codegraph node "planner"` → signature + source + `Calls → Artifacts, Delivery` / `Called by ← 3 tests`.

### 2.4 When to Use Which

- **Broad / unknown area** → `codegraph_explore` first (one call gives source + call paths).
- **Known symbol name** → `codegraph_search` (fast, ranked) then `codegraph_node` for detail.
- **Dependency question** → `codegraph_callers` / `codegraph_callees`.
- **Change impact** → `codegraph_impact` (see §3).
- **File map** → `codegraph_files`.
- Fallback to `grep`/`read` only when CodeGraph misses literal constants or generated names (per extension guidance).

---

## 3. Partitioning via `codegraph_impact`

### 3.1 What `codegraph_impact` Does

BFS over `edges` (`calls`/`references`/`instantiates`/`imports`) starting from the named symbol, depth-limited (default 2). Returns the transitive closure of affected symbols. `depth` controls radius: 1 = direct callers/callees, 2 = callers-of-callers, etc.

### 3.2 Using It to Determine Module Independence

Two symbols (or file groups) are **independent** iff their impact sets are disjoint — no shared node, no path between them within the chosen depth. Concretely:

```sh
# Are src/engine/prompts.rs and src/config/schema.rs independently changeable for task T?
codegraph impact "planner" --json   # → files: {prompts.rs}
codegraph impact "default_planner" --json  # → files: {schema.rs}
# disjoint → safe to partition
```

For a task touching N symbols, compute `impact(sym, depth=2)` for each, group by `filePath`, and partition into sets with no overlapping `affected` files. Overlapping sets must be sequenced or assigned to one subagent.

### 3.3 Practical Partitioning Recipe (for future subagents)

1. Planner extracts candidate symbols from task (or `codegraph_search` on task keywords).
2. For each candidate, `codegraph_impact <sym> --depth 2 --json` → collect `affected[].filePath`.
3. Build file-level overlap graph: edge between two candidates if their affected file sets intersect.
4. Connected components = partitions that must stay together; disconnected components = safe to parallelize.
5. Inject partition map into executor prompt: "These file groups are independent per CodeGraph; you may work on them in any order / delegate."

Current repo note: `planner` impact is 4 nodes in one file — trivially partitionable. Larger symbols like `Artifacts` or `RunState` have wider impact and would not partition.

---

## 4. Prompt Injection Proposal (Read-Only, No Phase/RunState Change)

### 4.1 Constraint

> Preserve: Artifacts not chat history, Roles never models. No change to `Phase` or `RunState`.

All prompts are pure functions in `src/engine/prompts.rs`:

```rust
pub fn planner(task: &str, workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String
pub fn executor(workdir: &Path, artifacts: &Artifacts, brief: Brief<'_>, delivery: Delivery) -> String
pub fn reviewer(workdir: &Path, artifacts: &Artifacts, brief: Brief<'_>, diff: &str, delivery: Delivery) -> String
pub fn fixer(workdir: &Path, artifacts: &Artifacts, brief: Brief<'_>, verdict: &Verdict, fix: FixAttempt, delivery: Delivery) -> String
pub fn account(workdir: &Path, artifacts: &Artifacts, brief: Brief<'_>, changes: &str, delivery: Delivery) -> String
```

Call sites in `src/engine/workflow.rs::run_phases` (lines ~401, 447, 528, plus fixer/account). `Artifacts` (`src/state/store.rs`) is already threaded through every prompt — it is the read-only injection point.

### 4.2 Injection Point

Add a **non-fallible, read-only helper** that enriches the prompt string without touching state:

```rust
// src/engine/codegraph.rs (new, ~60 LOC) — or inline in prompts.rs
pub fn context_for_task(workdir: &Path, task: &str) -> Option<String> { ... }
pub fn impact_for_diff(workdir: &Path, diff: &str) -> Option<String> { ... }
```

Called at the top of each prompt builder, result appended as a markdown section:

```rust
pub fn planner(task: &str, workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String {
    let graph_ctx = codegraph::context_for_task(workdir, task).unwrap_or_default();
    format!(
        "{}\nYour job is ...\n## Task\n\n{task}\n\n{graph_ctx}\n## Required structure\n...",
        preamble("planner", workdir),
        deliverable(delivery, &artifacts.plan(), "the plan"),
    )
}
```

If the helper returns `None` (DB missing, query fails, timeout), the prompt is unchanged — zero risk to the loop.

### 4.3 What to Inject, Per Role

| Role | Query | Injected Section |
|------|-------|------------------|
| **Planner** | `codegraph_search` on task keywords + `codegraph_files` (tree, maxDepth 3) | `## Codebase Map` — file tree + top 5 search hits with `file:line` |
| **Reviewer** | `codegraph_impact` on symbols extracted from `diff` (parse `+++ b/` lines → search those symbols) | `## Impact Analysis` — affected files/symbols beyond the diff, so reviewer checks unmodified dependents |
| **Fixer** | Same as reviewer (diff-scoped) | `## Impact of Current Change` — narrow, to keep fix diff small |
| **Executor** | Optional: `codegraph_explore` on plan's `# Files` table | `## Relevant Source` — verbatim source blocks for files the plan names |

Token budget: cap injected context to ~800 tokens (e.g. 5 search hits + file tree). Truncate with `… (truncated)` marker.

### 4.4 Implementation Options (in order of preference)

**Option A — CLI subprocess (recommended, 0 deps):**
`tokio::process::Command::new("codegraph").args(["query", task, "--json", "--path", workdir])` with 2 s timeout. No new crate, no SQLite dep, respects `.codegraph/` location. Requires `codegraph` on PATH (already true for Pi users; otherwise degrades to `None`).

**Option B — Direct SQLite read (rusqlite, read-only):**
Open `.codegraph/codegraph.db` with `SQLITE_OPEN_READONLY`, run `SELECT ... FROM nodes_fts WHERE nodes_fts MATCH ?` and `SELECT path FROM files`. Faster, no subprocess, but adds `rusqlite` + `fts5` handling.

**Option C — Snapshot file (zero code):**
`setup.commands: ["npx @vndv/pi-codegraph --out .codegraph/snapshot.json"]` then planner reads it as a normal file. No Rust change at all; already in worktree. Best for immediate value.

All three are read-only and leave `Phase`/`RunState` untouched. Option C can ship today; Option A is the 2–3 day follow-up.

### 4.5 Where `Artifacts` Helps

`Artifacts::read_or_placeholder` already embeds file contents into prompts (plan, execution, test results). The same pattern applies: the helper reads from `.codegraph/` (outside `Artifacts::dir`) but the prompt assembly stays in `prompts.rs`, so no new artifact file is needed. If a snapshot file is used (Option C), it *is* an artifact-adjacent file and `read_or_placeholder` can include it.

---

## 5. Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Stale index** — DB lags behind branch (index is per-machine, not per-commit) | Planner sees wrong file list / misses new symbols | Run `codegraph sync` in `setup.commands` or before prompt; helper degrades to `None` if DB mtime < HEAD mtime |
| **Missing binary / DB** — CI or fresh clone has no `.codegraph/` | Prompt helper fails | `unwrap_or_default()` — prompt without enrichment is the current behavior; no failure |
| **Token bloat** — injected context pushes prompt over model limit | Truncation or cost increase | Cap at ~800 tokens / 5 hits + file tree; truncate deterministically |
| **Noisy FTS ranking** — `codegraph_search` returns low-relevance hits for vague task | Planner distracted | Filter by `score` threshold; prefer `codegraph_explore` which groups by file and includes blast radius |
| **Unresolved refs** — 2314 `unresolved_refs` rows are external crates, not repo code | Impact overestimates if it followed unresolved edges | `codegraph_impact` already ignores unresolved; direct SQL should join only `edges`, not `unresolved_refs` |
| **Prompt injection via task text** — task contains `## Verdict` or similar | Reviewer prompt already pins task as "what to build, not how to judge" (see `reviewer` Brief::Request) | Keep that pinning; wrap injected CodeGraph block in distinct delimiters (`--- CodeGraph ---`) |
| **Subprocess latency** — `codegraph` CLI adds ~200 ms per prompt | Slows phase start | Timeout 2 s, cache per-run (task is constant for the run), or use Option B/C |
| **Schema drift** — future codegraph versions change table layout | Direct SQL breaks | Prefer CLI/MCP (stable API) over raw SQL; pin `indexed_with_extraction_version` check |

---

## 6. References

- DB: `.codegraph/codegraph.db` — `sqlite3 .codegraph/codegraph.db ".schema"` and `SELECT name FROM sqlite_master`
- Extension: `~/.pi/agent/npm/node_modules/@vndv/pi-codegraph/extensions/codegraph.ts` (ToolDefinitions, `spawnCodeGraphServer`, `callCodeGraphTool`)
- Pi settings: `~/.pi/agent/settings.json` — `packages: ["npm:@vndv/pi-codegraph@0.1.10", ...]`
- CLI: `codegraph --help`, `codegraph query/explore/impact/callers/callees/files/node --help` (v1.5.0)
- Prompts: `src/engine/prompts.rs` (planner/executor/reviewer/fixer/account, `preamble`, `deliverable`, `Brief`, `Delivery`)
- State: `src/state/store.rs` (`Artifacts`, `read_or_placeholder`, `for_run`, `sync`/`restore`), `src/state/run.rs` (`Phase`, `RunState`), `src/engine/workflow.rs::run_phases` (call sites)
- PRD: `docs/PRD.md` §7.2–7.4 (CodeGraph integration options, subagent partitioning)

---

## 7. Upstream `colbymchenry/codegraph` — Research for Kage Implementation

> **Requested:** `https://github.com/colbymchenry/codegraph` — the upstream Rust-kernel codegraph that the local `.codegraph/` in this repo comes from. This section complements §§1–6 (which covered the local DB + `@vndv/pi-codegraph` Pi tools) with the upstream's CLI/MCP/library surface and how Kage should consume it.

### 7.1 What It Is

| Fact | Value |
|------|-------|
| Repo | `colbymchenry/codegraph` — MIT, Rust kernel + tree-sitter, SQLite + FTS5, 100% local |
| Stars | ~68k · 4.3k forks · 459 issues (as of 2026-08-31) |
| Package | `@colbymchenry/codegraph` on npm — self-contained bundled Node runtime, no native build |
| Installed here | `codegraph 1.5.0` at `~/.local/state/fnm_multishells/.../bin/codegraph` · npm latest `1.6.0` |
| Local index | `.codegraph/codegraph.db` 2.37 MB · 27 files · 692 nodes · 1902 edges · WAL (`node:sqlite` built-in) · `index_state=complete` |
| Languages | 20+ with full extraction (TS/JS, Python, Go, Rust, Java, C#, PHP, Ruby, C/C++, Swift, Kotlin, Scala, Dart, Svelte, Vue, Astro, Lua, CFML, etc.) — per-file tree-sitter, cross-file resolution |
| Agents wired | Claude Code, Cursor, Codex CLI, opencode, Hermes Agent, Gemini CLI, Antigravity IDE, Kiro, GitHub Copilot (VS Code/CLI/JetBrains) |

**Relationship to `@vndv/pi-codegraph@0.1.10`:** That Pi extension is a thin MCP/tool wrapper (`codegraph_search`, `codegraph_callers`, …) that spawns the upstream `codegraph` MCP server / CLI under the hood. The upstream *is* the index — `pi-codegraph` just surfaces it as Pi tools. For Kage (Rust binary, no Pi runtime), the upstream CLI/MCP/library is the direct dependency; the Pi tools are not needed.

### 7.2 Install & Index (How the DB Gets Built)

```bash
# One-time per machine (no Node required — bundled runtime)
curl -fsSL https://raw.githubusercontent.com/colbymchenry/codegraph/main/install.sh | sh
# or: npm i -g @colbymchenry/codegraph

# Wire MCP to your agents (once, global)
codegraph install            # auto-detects Claude/Cursor/Codex/opencode/…
codegraph install --yes --init  # non-interactive + build current project's index

# Per project (one step — creates .codegraph/ and builds graph)
codegraph init
codegraph status             # verify: Files/Nodes/Edges/DB Size/Journal: wal
```

After `init`, the MCP server watches the project with native OS file events — debounce 2 s, source-files only, incremental sync, WAL mode so reads never block on writes. Stale-file banner (`⚠️`) and connect-time `(size, mtime)` + content-hash catch-up handle edits made outside the watcher (e.g. `git pull`). Manual `codegraph sync` only needed when watcher is disabled (`CODEGRAPH_NO_DAEMON=1` or sandboxed env). Verified here: `codegraph status` reports `✓ Index is up to date` with `Journal: wal`.

`.codegraph/.gitignore` is `*` + `!.gitignore` — DB, daemon pid/sockets/logs are machine-local, never committed (same as in §1.1).

### 7.3 CLI Surface (Verified Locally on This Repo)

```bash
codegraph query <search> [--kind <kind>] [--limit N] [--json]   # FTS5 search, scored
codegraph explore <query...>                                      # ONE call → verbatim source grouped by file + call paths + blast radius (same as MCP codegraph_explore)
codegraph node [name]                                             # one symbol's source + callers/callees, or read file with line numbers
codegraph files [--json] [--max-depth N] [--filter <glob>]       # file structure from index
codegraph callers <symbol> [--limit N] [--json]                  # who calls this
codegraph callees <symbol> [--limit N] [--json]                  # what this calls
codegraph impact <symbol> [--depth N] [--json]                   # transitive blast radius (affected nodes/edges)
codegraph affected [files...] [--stdin] [--depth N] [--filter <glob>] [--json]  # transitive test-file tracing (import deps)
codegraph status [path]                                           # stats + staleness
codegraph sync [path] | index [path] | daemon | upgrade | version
```

**Verified outputs on `algonacci/kage` (v1.5.0):**

- `codegraph query "workflow" --limit 3 --json` → `file:src/engine/workflow.rs` (score 55.7) + imports — FTS5 ranked.
- `codegraph callers "run_phases" --json` → `drive` in `src/engine/workflow.rs:291` — single caller.
- `codegraph impact "RunState" --depth 2 --json` → `nodeCount 68, edgeCount 121`, affected includes `RunState` struct + `RunState::new/transition` + callers across `cli/mod.rs`, `cli/status.rs`, `engine/workflow.rs` — depth-2 BFS, suitable for partitioning.
- `codegraph explore "how does a run start"` (no `--json` flag) → `Found 50 symbols across 2 files`, **Blast radius** per symbol (callers + `⚠️ no covering tests found`), then **verbatim source** grouped by file (`src/engine/workflow.rs` lines 73–… with `start()`, `required_roles()`, `branch_names()` etc.) — one call replaces multiple `Read`/`Grep`.
- `codegraph files --json` → `[{path, language, nodeCount, size}]` for 27 files (Rust 25, YAML 2).
- `codegraph affected src/engine/workflow.rs --json` → `{changedFiles: ["src/engine/workflow.rs"], affectedTests: [], totalDependentsTraversed: 13}` — import-transitive, depth 5 default.
- `codegraph node "RunState"` → struct outline (4 members: `new`, `transition`, `remaining_iterations`, `remaining_repairs`) + `Called by ←` trail (20+ callers).

> Note: `explore` has no `--json` flag (markdown output); `query`/`callers`/`callees`/`impact`/`files`/`affected` do.

### 7.4 MCP & Library Surfaces

**MCP server** (`codegraph serve --mcp`, wired by `codegraph install` into each agent's `mcpServers`):

- Single listed tool by default: `codegraph_explore` — one call returns source + call flow + blast radius, including dynamic-dispatch hops (callbacks, React re-render, interface→impl) that grep cannot follow. Guidance is delivered in the MCP `initialize` response (`src/mcp/server-instructions.ts`).
- Other tools (`codegraph_node`, `codegraph_search`, `codegraph_callers`, `codegraph_callees`, `codegraph_impact`, `codegraph_files`, `codegraph_status`) remain functional but unlisted; re-enable via `CODEGRAPH_MCP_TOOLS=explore,node,search,callers` or use CLI equivalents.
- Per-project: pass `projectPath` to query any indexed project (monorepo sub-service or second repo) in one session; unindexed path returns guidance, not failure.

**Library (npm)** — `import CodeGraph from '@colbymchenry/codegraph'` (Node 22.5+ for built-in `node:sqlite`, or Electron with bundled Node 22.5+):

```ts
const cg = await CodeGraph.init('/path/to/project');
await cg.indexAll({ onProgress: p => console.log(`${p.phase}: ${p.current}/${p.total}`) });
const results = cg.searchNodes('UserService');
const callers = cg.getCallers(results[0].node.id);
const impact = cg.getImpactRadius(results[0].node.id, 2);
const ctx = await cg.buildContext('fix login bug', { maxNodes: 20, includeCode: true, format: 'markdown' });
cg.watch(); cg.close();
```

Also exports `DatabaseConnection`, `QueryBuilder`, `getDatabasePath`, `initGrammars`/`loadGrammarsForLanguages`, `FileLock`. CLI/MCP are unaffected by the Node version — they run on the bundled runtime.

**For Kage (Rust binary):** The library is not directly usable (requires Node 22.5+ in-process). The CLI is the correct surface — same pattern as `proc::run` already used for `git`/`cargo`/`rtk`.

### 7.5 Extra Capabilities Relevant to Kage

- **Framework-aware routes** — detects `route` nodes (Django `urls.py`, Flask/FastAPI `@app.route`, Express `app.get`, NestJS `@Controller`, Rails `get`, Spring `@GetMapping`, etc.) linked by `references` to handlers; `callers` of a view surfaces its URL pattern.
- **Mixed iOS / React Native / Expo bridging** — Swift↔ObjC (`@objc` bridging), RN legacy bridge (`NativeModules`), TurboModules, `sendEventWithName`↔`addListener`, Expo `Module { Name("X") }`, Fabric/Paper view managers — edges tagged `provenance:'heuristic'` + `metadata.synthesizedBy` (e.g. `swift-objc-bridge`, `rn-event-channel`).
- **`codegraph affected`** — transitive import-dependency tracing to find affected test files from changed sources; ideal for Kage's `TEST` phase to select `cargo test` subset or to enrich `TEST_RESULTS.md` with "tests that should have been run".
- **Measured cross-file coverage** — fair coverage = share of symbol-bearing files with ≥1 resolved cross-file dependent on a real benchmark repo per language (TS/JS 95.8%, Python 100%, Go 96.6%, Rust 86.7%, etc.) — honest frontier, not gamed.

### 7.6 How Kage Should Implement It (Recommendation)

**Use the upstream CLI directly — same 3 options as §4.4, now with upstream specifics:**

| Option | How | Effort | When |
|--------|-----|--------|------|
| **C — Snapshot (today, 0 Rust change)** | `setup.commands: ["codegraph sync --quiet"]` then `Artifacts::read_or_placeholder` reads `.codegraph/snapshot.json` if you emit one, or just rely on CLI at prompt time | 1 hour | Immediate value, already in worktree |
| **A — CLI subprocess (recommended, 2–3 days)** | `tokio::process::Command::new("codegraph").args([...]).timeout(2s)` — no new crate, no SQLite dep, respects `.codegraph/` location | 2–3 days | Follow-up after C |
| **B — Direct SQLite (rusqlite, read-only)** | Open `.codegraph/codegraph.db` with `SQLITE_OPEN_READONLY`, query `nodes_fts` + `files` | 2–3 days + dep | Only if subprocess latency matters |

**Preferred CLI invocations for Kage (verified above):**

```rust
// src/engine/codegraph.rs — new helper, ~60 LOC, read-only, infallible (Option<String>)
pub fn context_for_task(workdir: &Path, task: &str) -> Option<String> {
    // 1. codegraph files --json  → file tree (maxDepth 3, cap ~20 files)
    // 2. codegraph explore <task>  → verbatim source + blast radius (cap 800 tokens, truncate with "… (truncated)")
    // Both with 2s timeout, None on missing DB/binary/timeout
}
pub fn impact_for_diff(workdir: &Path, diff: &str) -> Option<String> {
    // parse diff "+++ b/<path>" → extract symbols → codegraph impact <symbol> --depth 2 --json
    // return "## Impact Analysis" markdown: affected files/symbols beyond the diff
}
```

Injected in `src/engine/prompts.rs` at the top of each prompt builder (same pattern as §4.2):

| Role | Query | Injected Section |
|------|-------|------------------|
| Planner | `codegraph explore <task>` + `codegraph files --json` (tree, maxDepth 3) | `## Codebase Map` — file tree + top hits with `file:line` |
| Reviewer | `codegraph impact` on symbols from `diff` (`+++ b/` parse) | `## Impact Analysis` — dependents beyond the diff |
| Fixer | Same as reviewer (diff-scoped) | `## Impact of Current Change` — narrow, keep fix diff small |
| Executor | `codegraph explore` on plan's `# Files` table | `## Relevant Source` — verbatim blocks for files the plan names |
| TEST (future) | `codegraph affected <changed files> --stdin --quiet` | Select `cargo test` subset or annotate `TEST_RESULTS.md` |

Token budget: cap injected context to ~800 tokens (5 hits + file tree), truncate deterministically with `… (truncated)`. Wrap in distinct delimiters (`--- CodeGraph ---`) so task text cannot inject `## Verdict`.

**Why CLI over library/MCP for Kage:**

- Kage is a Rust single binary (`cargo install`) — library requires Node 22.5+ in-process, adds `npm:@colbymchenry/codegraph` dep and per-platform package, not justified.
- MCP is for agents (Claude/Cursor/Codex) — Kage *drives* those agents; it should not itself be an MCP client. The CLI is the harness-agnostic surface that `proc::run` already handles (timeout, heartbeat, stall, Windows `cmd /C` + PATHEXT).
- `codegraph` is already on PATH here (1.5.0); `preflight::check` pattern can verify it like other harnesses, degrading to `None` (current behavior) when absent — zero risk to loop.

**Configuration (zero-config by default, optional `codegraph.json` at repo root):**

```json
{ "exclude": ["static/", "**/vendor/**"], "include": ["Tools/"], "deprioritize": ["scripts/", "optional-skills/"], "extensions": {".dota_lua": "lua"} }
```

Built-in skips: `node_modules`, `vendor`, `dist`, `build`, `target`, `.venv`, `Pods`, `.next`, `>1 MB` files, plus `.gitignore` (honored via git or direct read). `deprioritize` keeps paths indexed but demotes their rank — useful for `scripts/` with generic names (`run`, `status`). No Kage change needed unless custom extensions are used.

**Subagent partitioning (ties to §3 + PRD §7.3):** `codegraph impact --depth 2 --json` returns `nodeCount`/`edgeCount` + `affected[]` with `filePath`. For a task that touches multiple files, run `impact` per candidate symbol, compute disjoint file sets via depth-2 BFS (as in §3.3), and only spawn parallel subagents for disjoint sets — prevents deadlock/overwrite. This is the same heuristic proposed in §3.3, now verified with upstream `impact` output (68 nodes/121 edges for `RunState`).

### 7.7 Risks & Mitigations (Upstream-Specific, Extends §5)

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Version drift** — local 1.5.0 vs npm 1.6.0, future schema changes | CLI flags or table layout drift | Prefer CLI/MCP (stable) over raw SQL; check `indexed_with_version` / `indexed_with_extraction_version` (currently 1.5.0 / 24); `codegraph upgrade` to pin |
| **Daemon lock / WSL2 `/mnt` socket** — MCP `Transport closed`, `database is locked` | `codegraph status` shows `Journal:` ≠ `wal` or daemon fallback | `CODEGRAPH_NO_DAEMON=1` to skip shared server; move project to Linux-native FS (`~/` not `/mnt/c`); `codegraph unlock` for stale lock |
| **Large repo WAL growth** — `-wal` grows during big index | Disk pressure | Tunables `CODEGRAPH_WAL_VALVE_MB` (soft threshold during index, default max(256 MB, index/4) up to 2 GB) + `CODEGRAPH_WAL_HEAL_MB` (resting 64 MB); `CODEGRAPH_WAL_VALVE_DEBUG=1` |
| **Stale index vs branch** — DB per-machine, not per-commit | Planner sees wrong files | `codegraph sync` in `setup.commands` or before prompt; helper degrades to `None` if DB mtime < HEAD mtime; watcher auto-sync covers normal edits |
| **Sharing checkout Windows↔WSL** — lock + SQLite cross-FS unreliable | Corrupt or stale index | `CODEGRAPH_DIR=.codegraph-win` on one side; CodeGraph skips sibling `.codegraph-*` when indexing |

### 7.8 References (Upstream)

- Repo: `https://github.com/colbymchenry/codegraph` — README (install, CLI ref, MCP tools, library, config, troubleshooting), `package.json` (`@colbymchenry/codegraph` 1.6.0), `CHANGELOG.md`, `install.sh`/`install.ps1`
- Docs site: `https://colbymchenry.github.io/codegraph/` — guides/indexing, MCP server instructions (`src/mcp/server-instructions.ts`)
- Local verification: `codegraph --help`, `codegraph status`, `codegraph query/explore/node/files/callers/impact/affected --help`, `sqlite3 .codegraph/codegraph.db ".schema"` + `SELECT * FROM project_metadata`, `codegraph explore "how does a run start"`, `codegraph impact "RunState" --depth 2 --json` (all on `algonacci/kage` 27 files, 1.5.0)
- Existing research: `docs/research/codegraph.md` §§1–6 (local DB schema, Pi tools, prompt injection proposal), `docs/research/pi-harness.md` (AdapterKind::Pi), `docs/research/dag-feasibility.md` (Phase→Graph)
- Kage integration points: `src/engine/prompts.rs` (planner/executor/reviewer/fixer/account), `src/state/store.rs` (`Artifacts`), `src/adapters/proc.rs` (`proc::run` timeout/heartbeat/Windows), `src/config/schema.rs` (LoopConfig), `src/engine/workflow.rs::run_phases`
