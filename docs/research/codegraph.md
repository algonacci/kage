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
