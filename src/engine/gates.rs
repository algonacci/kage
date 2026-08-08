//! The acceptance gate: turning a reviewer's output into a state transition.
//!
//! The loop needs a decision, not prose. A reviewer that writes three paragraphs of nuance gives
//! the state machine nothing to act on, so the review phase demands a machine-readable verdict and
//! treats its absence as a blocked run rather than guessing.

use serde::{Deserialize, Serialize};

/// The three outcomes a review can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VerdictKind {
    /// The work satisfies the plan. The run is done.
    Pass,
    /// There are fixable problems. The executor gets another iteration.
    Fail,
    /// Something outside the agents' reach is wrong — a contradictory plan, a missing credential,
    /// an impossible requirement. Iterating cannot help, so the run stops for a human.
    Blocked,
}

/// One reviewer finding, addressable by id so a fix prompt can refer to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    #[serde(default)]
    pub id: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// The parsed contents of `VERDICT.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub verdict: VerdictKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.verdict == VerdictKind::Pass
    }

    /// Issues rendered for a fix prompt, numbered so the executor can work through them.
    pub fn issues_markdown(&self) -> String {
        if self.issues.is_empty() {
            return self
                .summary
                .clone()
                .unwrap_or_else(|| "_(the reviewer failed the work but listed no issues)_".into());
        }

        self.issues
            .iter()
            .enumerate()
            .map(|(index, issue)| {
                let id = if issue.id.is_empty() {
                    format!("REV-{:03}", index + 1)
                } else {
                    issue.id.clone()
                };
                let where_ = issue
                    .file
                    .as_ref()
                    .map(|file| format!(" ({file})"))
                    .unwrap_or_default();
                let severity = issue
                    .severity
                    .as_ref()
                    .map(|severity| format!(" [{severity}]"))
                    .unwrap_or_default();
                format!("- **{id}**{severity}{where_}: {}", issue.description)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Recover a verdict from a reviewer that wrote prose instead of a clean file.
///
/// Agents routinely wrap JSON in a ```json fence or bracket it with commentary. Failing the whole
/// run over that would waste a completed review, so the raw text is scanned for the first balanced
/// JSON object that parses as a verdict.
pub fn extract(text: &str) -> Option<Verdict> {
    if let Ok(verdict) = serde_json::from_str::<Verdict>(text.trim()) {
        return Some(verdict);
    }

    let bytes = text.as_bytes();
    for (start, _) in text.char_indices().filter(|(_, c)| *c == '{') {
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;

        for end in start..bytes.len() {
            let ch = bytes[end] as char;

            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if in_string => escaped = true,
                '"' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        // `end` indexes a byte, but `}` is ASCII so it is also a char boundary.
                        if let Ok(verdict) = serde_json::from_str::<Verdict>(&text[start..=end]) {
                            return Some(verdict);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    None
}

/// Read the verdict for a run, preferring the file and falling back to the reviewer's stdout.
pub fn read(verdict_path: &std::path::Path, reviewer_stdout: &str) -> Option<Verdict> {
    if let Ok(raw) = std::fs::read_to_string(verdict_path)
        && let Some(verdict) = extract(&raw)
    {
        return Some(verdict);
    }

    extract(reviewer_stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_verdict_file_parses() {
        let verdict = extract(
            r#"{"verdict":"FAIL","severity":"major","issues":[
                {"id":"REV-001","description":"Refresh token rotation is not atomic."}]}"#,
        )
        .unwrap();

        assert_eq!(verdict.verdict, VerdictKind::Fail);
        assert!(!verdict.passed());
        assert_eq!(verdict.issues.len(), 1);
    }

    #[test]
    fn a_fenced_verdict_is_recovered_from_surrounding_prose() {
        let verdict = extract(
            "I reviewed the change and it looks good.\n\n\
             ```json\n{\"verdict\": \"PASS\"}\n```\n\nLet me know if you need more.",
        )
        .unwrap();

        assert!(verdict.passed());
    }

    #[test]
    fn braces_inside_strings_do_not_end_the_object_early() {
        let verdict = extract(
            r#"noise {"verdict":"FAIL","summary":"unbalanced { brace and \" quote in code"} tail"#,
        )
        .unwrap();

        assert_eq!(verdict.verdict, VerdictKind::Fail);
        assert!(verdict.summary.unwrap().contains("unbalanced"));
    }

    #[test]
    fn an_unrelated_json_object_is_skipped_for_a_real_verdict() {
        let verdict = extract(r#"{"unrelated":true} then {"verdict":"BLOCKED"}"#).unwrap();

        assert_eq!(verdict.verdict, VerdictKind::Blocked);
    }

    #[test]
    fn output_with_no_verdict_yields_nothing() {
        assert!(extract("The code looks fine to me, ship it.").is_none());
        assert!(extract("{ not json at all").is_none());
    }

    #[test]
    fn a_lowercase_verdict_is_rejected_rather_than_guessed() {
        // The prompt asks for uppercase; silently accepting other spellings would hide a reviewer
        // that is not following the contract.
        assert!(extract(r#"{"verdict":"pass"}"#).is_none());
    }

    #[test]
    fn issues_render_with_generated_ids_when_the_reviewer_omits_them() {
        let verdict = extract(
            r#"{"verdict":"FAIL","issues":[
                {"description":"no test for the empty case","severity":"minor","file":"src/a.rs"}]}"#,
        )
        .unwrap();

        let markdown = verdict.issues_markdown();

        assert!(markdown.contains("REV-001"));
        assert!(markdown.contains("[minor]"));
        assert!(markdown.contains("(src/a.rs)"));
    }

    #[test]
    fn a_failing_verdict_with_no_issues_still_says_something_usable() {
        let verdict = extract(r#"{"verdict":"FAIL","summary":"tests are too shallow"}"#).unwrap();

        assert_eq!(verdict.issues_markdown(), "tests are too shallow");
    }

    #[test]
    fn the_file_wins_over_stdout_and_stdout_is_the_fallback() {
        let dir = std::env::temp_dir().join(format!("kage-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("VERDICT.json");

        assert_eq!(
            read(&path, r#"{"verdict":"FAIL"}"#).unwrap().verdict,
            VerdictKind::Fail,
            "with no file, stdout is used"
        );

        std::fs::write(&path, r#"{"verdict":"PASS"}"#).unwrap();
        assert!(
            read(&path, r#"{"verdict":"FAIL"}"#).unwrap().passed(),
            "the file is authoritative"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
