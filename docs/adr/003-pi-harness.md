# ADR 003 — Pi/Ohmypi Harness (`AdapterKind::Pi` Single Alias)

> **Status:** Proposed (prototype — `prototype/3-adr-outlines`, throwaway, not merged) · **Date:** 2026-08-31 · **Ticket:** [#12 Prototype — 3 ADR outlines](https://github.com/algonacci/kage/issues/12) (part of [#2 Wayfinder Map](https://github.com/algonacci/kage/issues/2))
> **Deciders:** @amaldevice + research [#4](https://github.com/algonacci/kage/issues/4) (Pi harness interface), grilling [#9](https://github.com/algonacci/kage/issues/9) (Pi preset), prototype [#11](https://github.com/algonacci/kage/issues/11) (CodeGraph — pi already verified as harness)

---

## Context

Kage's `AdapterKind` has variants `ClaudeCode`, `Codex`, `OpenCode`, `Kamui`, `Command`, `Api` (`src/config/schema.rs`, `src/adapters/cli.rs`). PRD §7.1 proposes adding `pi`/`ohmypi` (`@oh-my-pi/pi-coding-agent`) — verified CLI:

- `pi -p "prompt"` (non-interactive), `pi --model <model> -p "prompt"`, `pi --provider <provider> --model <model> -p "prompt"`, `pi --append-system-prompt <file> -p "prompt"`, `pi --session <jsonl> --tools <allowlist> -e <ext> -p "prompt"` (subagent mode)
- `ohmypi` is the same binary as `pi` (distribution alias, `pi_natives` build via `bun install` on macOS arm64) — not a separate harness.

The question is whether to add a native `AdapterKind::Pi` preset, how it maps to `program_for`/`preset_template`/`default_extra_args`/`output_format`/`prompt_delivery`, and the migration path from `adapter: command`.

Research: `research/pi-harness` `c1a460b` (840 lines, 14 sections — `pi --help` argv, prompt delivery `{prompt}`/`{prompt_file}`/stdin, streaming JSON vs passthrough, Windows `.cmd` shim, `AdapterKind` variants, `preflight`/`kage doctor`, single-alias design).

## Decision

**Single `AdapterKind::Pi` variant — `pi` and `ohmypi` are the same, one preset, migration `adapter: command` → native preset.**

### Preset

```rust
// src/config/schema.rs
pub enum AdapterKind {
    ClaudeCode, Codex, OpenCode, Kamui, Pi, Command, Api,
    // Pi covers both `pi` and `ohmypi` — single variant, single display name "pi"
}

// src/adapters/cli.rs
impl AdapterKind {
    fn program_for(&self) -> &str {
        match self {
            Self::Pi => "pi",  // no omp fallback in v1 — PATHEXT + cmd /C handles .cmd shim
            // …
        }
    }
    fn preset_template(&self) -> Vec<String> {
        match self {
            Self::Pi => vec!["pi".into(), "-p".into(), "{prompt}".into()],
            // extra_args appended after prompt (permission flags pattern, same as claude/codex)
        }
    }
    fn default_extra_args(&self) -> Vec<String> {
        match self {
            Self::Pi => vec![],  // no mandatory permission flags (unlike claude --permission-mode)
        }
    }
    fn output_format(&self) -> OutputFormat {
        match self {
            Self::Pi => OutputFormat::Passthrough,  // defer PiJson — pi --mode json NDJSON not yet stable
        }
    }
    fn prompt_delivery(&self) -> PromptDelivery {
        match self {
            Self::Pi => PromptDelivery::File,  // file pointer — Windows 32k cap, same as claude/codex
        }
    }
}
```

| Concern | Decision |
|---------|----------|
| **Variant** | Single `Pi` — `pi` display, `ohmypi` is alias (same binary, same preset). No `Ohmypi` variant. |
| **`program_for`** | `"pi"` — no `omp` fallback in v1. `proc::resolve_program` handles `PATHEXT` + `cmd /C` for `.cmd` shim (same as `claude`/`codex` via npm). |
| **`preset_template`** | `["pi", "-p", "{prompt}"]` + `--model <model>` when `model` set (opaque forward, `provider/model` or bare). `extra_args` appended after prompt. |
| **`default_extra_args`** | `[]` — no mandatory permission flags (unlike `claude --permission-mode acceptEdits`). |
| **`output_format`** | `Passthrough` — defer `PiJson` (pi `--mode json` NDJSON schema not yet stable for `stream.rs` rendering). |
| **`prompt_delivery`** | `File` pointer — Windows 32k cap, same as claude/codex. `{prompt}` replaced with file path, `{prompt_file}` also supported. |
| **Model** | Opaque forward — `pi --model <model>` when `model` set, unset → harness default. Fuzzy `provider/id:thinking` passthrough (same hazard as `claude --model opus` vs `claude-opus-5`). Prefer unset. |
| **Streaming** | Passthrough — raw log is readable, no `progress.log` twin (same as non-claude harnesses). Future `PiJson` would add `codegraph`-style rendering if pi stabilizes NDJSON. |

### Migration Path

| Step | How | Effort | When |
|------|-----|--------|------|
| **1. `adapter: command` (today)** | `roles: {executor: {adapter: command, command: ["pi", "-p", "{prompt}"]}}` — no code change, `kage doctor` via `proc::resolve_program` already checks `pi` | 1 hour | Today — reversible (1 line config) |
| **2. Native preset (after 2–3 weeks stability)** | Add `AdapterKind::Pi` + `program_for`/`preset_template`/`default_extra_args`/`output_format`/`prompt_delivery` | 1 day | After step 1 stable, argv verified |
| **3. Optional streaming (future)** | `OutputFormat::PiJson` + `stream.rs` rendering if `pi --mode json` stabilizes | 1–2 days | When pi NDJSON schema stable |

Step 1 is already usable — `CliAdapter::from_config` handles `command:` generically, `preflight::check` resolves `pi` via `proc::resolve_program` (PATHEXT + `shim_for` + `cmd /C`).

### Config Example

```yaml
# .kage/config.yaml — step 1 (today, no code change)
roles:
  executor:
    adapter: command
    command: ["pi", "-p", "{prompt}"]
    timeout_secs: 1800

# .kage/config.yaml — step 2 (after preset, native)
roles:
  executor:
    adapter: pi
    model: orvix/muse-spark-1.2  # optional — opaque forward as pi --model
    timeout_secs: 1800
```

## Alternatives Considered

| Alternative | Why rejected |
|-------------|--------------|
| Two presets `Pi` + `Ohmypi` | Same binary, same argv — two variants double the surface for zero value. `ohmypi` is distribution alias, not separate harness. |
| Stay `adapter: command` forever (no preset) | Works, but loses `kage doctor` preset awareness, `AdapterKind` exhaustiveness, and `output_format`/`prompt_delivery` defaults — preset is the idiomatic Kage way (same as `claude-code`/`codex`/`opencode`/`kamui`). |
| `AdapterKind::Pi` with `omp` fallback (`program_for` tries `pi` then `omp`) | Adds fallback complexity, `omp` is not a separate binary in this repo's context — `pi` is the canonical name. Defer fallback if `omp` ever diverges. |
| `PiJson` streaming now | `pi --mode json` NDJSON schema not yet verified stable — `stream.rs` would need `PiJson` variant + rendering, risk of drift. Passthrough is safe, raw log already readable. |

## Consequences

- **Positive:** `pi`/`ohmypi` usable today via `adapter: command` (no code change), native preset after 2–3 weeks (1 day), single variant (minimal surface), `kage doctor` covers `pi` via `proc::resolve_program`, model opaque forward (same pattern as other harnesses).
- **Negative:** One new `AdapterKind` variant + 4 match arms (`program_for`, `preset_template`, `default_extra_args`, `output_format`/`prompt_delivery`), `pi` must be on PATH (or `command:` with full path).
- **Neutral:** Streaming deferred — passthrough is sufficient for v1, `PiJson` is future.

## Preservation

Per [#13 Preservation contract](https://github.com/algonacci/kage/issues/13):

- Roles never models — `pi` is a harness, `model` is opaque forward.
- `preflight::check` before run (same as other harnesses) — missing `pi` fails before alloc, not after spend.
- `proc::resolve_program` Windows handling (`PATHEXT`, `shim_for`, `cmd /C`) — same as `claude`/`codex`/`opencode`.
- `prompt_delivery: File` — Windows 32k cap, same as other harnesses.
- Bounded execution — `timeout_secs`/`stall_secs` per role, same as other harnesses.

Verification: `cargo test` + `cargo clippy` + `cargo fmt` + `kage doctor` (pi detected) + `kage run --skip-plan "health check"` with `pi` via `command:` (verify `EXECUTION.md`).

## References

- PRD §7.1 (Harness `pi`/`ohmypi` — Opsi A `adapter: command`, Opsi B preset native)
- Research `research/pi-harness` `c1a460b` (840 lines — `pi --help` argv, prompt delivery, streaming, Windows shim, `AdapterKind` design, `preflight`/`kage doctor`, single-alias, migration path, model hazard)
- Decisions [#9](https://github.com/algonacci/kage/issues/9) (Pi preset — single alias, `["pi","-p"]`+`--model`, `[]`, Passthrough, File), [#13](https://github.com/algonacci/kage/issues/13) (preservation)
- Code: `src/config/schema.rs` (`AdapterKind`, `RoleConfig`), `src/adapters/cli.rs` (`program_for`, `preset_template`, `default_extra_args`, `output_format`), `src/adapters/proc.rs` (`resolve_program`, `shim_for`), `src/adapters/preflight.rs` (`check`/`not_ready`), `src/cli/doctor.rs`
