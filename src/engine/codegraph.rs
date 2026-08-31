//! CodeGraph read-only context for prompt enrichment.
//!
//! ponytail: sync std::process::Command, no async needed for prompt enrichment
//! — prompt builders are sync and must not require a tokio runtime.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(2);
// ponytail: ~800-token cap ≈ 3200 chars, truncate with … (truncated)
const TOKEN_CAP: usize = 3200;

fn db_present(workdir: &Path) -> bool {
    workdir.join(".codegraph/codegraph.db").exists()
}

fn run_codegraph(workdir: &Path, args: &[&str]) -> Option<String> {
    if !db_present(workdir) {
        return None;
    }
    // Resolve via proc::resolve_program so Windows .cmd shims (npm) are handled
    // via PATHEXT + cmd /C, like every other external program in Kage.
    let resolved = crate::adapters::proc::resolve_program("codegraph").ok()?;
    let workdir = workdir.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let prefix = resolved.prefix_args.clone();
    let program = resolved.program.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut cmd = Command::new(&program);
        cmd.args(&prefix).args(&args).arg("--path").arg(&workdir);
        let out = cmd.output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(o)) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        _ => None,
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= TOKEN_CAP {
        s.to_string()
    } else {
        format!("{}… (truncated)", &s[..TOKEN_CAP])
    }
}

/// File tree + hits for the planner — the planner has no RepoMap without it.
pub fn context_for_task(workdir: &Path, task: &str) -> Option<String> {
    if !db_present(workdir) || task.trim().is_empty() {
        return None;
    }
    let files_json = run_codegraph(workdir, &["files", "--json"])?;
    let explore = run_codegraph(workdir, &["explore", task.trim()])?;
    let files: Vec<serde_json::Value> = serde_json::from_str(&files_json).unwrap_or_default();
    let mut tree = String::from("File tree:\n");
    for f in files.iter().take(30) {
        let pp = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let n = f.get("nodeCount").and_then(|v| v.as_u64()).unwrap_or(0);
        tree.push_str(&format!("- {pp} ({n} nodes)\n"));
    }
    let combined = format!(
        "--- CodeGraph ---\n\n## Codebase Map\n\n{tree}\nTop hits for \"{}\":\n\n{explore}\n\n--- End CodeGraph ---",
        task.trim()
    );
    Some(truncate(&combined))
}

/// Dependents beyond the diff — the reviewer needs them to judge blast radius.
pub fn impact_for_diff(workdir: &Path, diff: &str) -> Option<String> {
    if !db_present(workdir) {
        return None;
    }
    let mut syms = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            let pp = rest.trim();
            if pp.is_empty() || pp == "/dev/null" {
                continue;
            }
            if !syms.contains(&pp.to_string()) {
                syms.push(pp.to_string());
            }
        }
    }
    if syms.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for sym in syms.iter().take(3) {
        if let Some(j) = run_codegraph(workdir, &["impact", sym, "--depth", "2", "--json"])
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&j)
            && let Some(arr) = v.get("affected").and_then(|a| a.as_array())
        {
            for it in arr.iter().take(10) {
                let n = it.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let ff = it.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
                let ll = it.get("startLine").and_then(|v| v.as_u64()).unwrap_or(0);
                out.push(format!("- {n} ({ff}:{ll})"));
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    out.truncate(15);
    let combined = format!(
        "--- CodeGraph ---\n\n## Impact Analysis\n\nBlast radius (depth 2):\n{}\n\n--- End CodeGraph ---",
        out.join("\n")
    );
    Some(truncate(&combined))
}

/// Partition disjoint check needs file sets per task — degrade to None when unavailable.
///
/// Uses `explore` to find candidate symbols, then `impact --depth 2 --json` per symbol
/// to collect disjoint file sets via depth-2 BFS as spec requires.
pub fn impact_files_for_task(workdir: &Path, task: &str) -> Option<Vec<String>> {
    if !db_present(workdir) || task.trim().is_empty() {
        return None;
    }
    let explore = run_codegraph(workdir, &["explore", task.trim()])?;
    // Extract candidate symbols from explore output — look for src/ paths as symbol hints
    let mut candidates = Vec::new();
    for word in explore.split_whitespace() {
        let cleaned = word.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '-' && c != '_'
        });
        if cleaned.starts_with("src/") && cleaned.contains('.') {
            let sym = cleaned.split(':').next().unwrap_or(cleaned);
            // Use file path as symbol for impact — codegraph impact accepts file paths too
            if !candidates.contains(&sym.to_string()) {
                candidates.push(sym.to_string());
            }
        }
        if candidates.len() >= 3 {
            break;
        }
    }
    if candidates.is_empty() {
        return None;
    }
    // For each candidate, run impact --depth 2 --json and collect filePaths
    let mut files = Vec::new();
    for sym in candidates.iter().take(3) {
        if let Some(j) = run_codegraph(workdir, &["impact", sym, "--depth", "2", "--json"])
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&j)
        {
            // Try affected[].filePath (new shape) or filePaths (old shape)
            #[allow(clippy::collapsible_if)]
            if let Some(arr) = v.get("affected").and_then(|a| a.as_array()) {
                for it in arr {
                    if let Some(fp) = it.get("filePath").and_then(|v| v.as_str()) {
                        if !files.contains(&fp.to_string()) {
                            files.push(fp.to_string());
                        }
                    }
                }
            }
            if let Some(arr) = v.get("filePaths").and_then(|a| a.as_array()) {
                for it in arr {
                    #[allow(clippy::collapsible_if)]
                    if let Some(fp) = it.as_str() {
                        #[allow(clippy::collapsible_if)]
                        if !files.contains(&fp.to_string()) {
                            files.push(fp.to_string());
                        }
                    }
                }
            }
        }
    }
    // Fallback: if impact returned nothing, use candidates directly as file hints
    if files.is_empty() {
        files = candidates;
    }
    if files.is_empty() {
        return None;
    }
    files.truncate(10);
    Some(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn context_for_task_returns_none_when_db_missing() {
        let dir = std::env::temp_dir().join(format!("kage-cg-miss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(context_for_task(&dir, "add auth").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn impact_for_diff_returns_none_when_db_missing() {
        let dir = std::env::temp_dir().join(format!("kage-cg-miss2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(impact_for_diff(&dir, "+++ b/src/adapters/api.rs\n").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_for_task_returns_some_with_file_tree_when_db_present() {
        let dir = std::env::temp_dir().join(format!("kage-cg-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".codegraph")).unwrap();
        let src = Path::new(".codegraph/codegraph.db");
        if !src.exists() {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        fs::copy(src, dir.join(".codegraph/codegraph.db")).unwrap();
        for suf in ["-wal", "-shm"] {
            let p = format!(".codegraph/codegraph.db{suf}");
            let s = Path::new(&p);
            if s.exists() {
                let _ = fs::copy(s, dir.join(format!(".codegraph/codegraph.db{suf}")));
            }
        }
        if Command::new("codegraph").arg("--version").output().is_err() {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let r = context_for_task(&dir, "validate");
        assert!(r.is_some(), "expected Some when DB present");
        let s = r.unwrap();
        assert!(s.contains("CodeGraph"), "missing delimiter: {s}");
        assert!(s.contains("Codebase Map"), "missing map: {s}");
        let _ = fs::remove_dir_all(&dir);
    }
}
