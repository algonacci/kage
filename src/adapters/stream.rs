//! Turning a streaming harness's output into what a human should see.
//!
//! A streaming harness emits machine events as a JSON stream; dumping those envelopes is as
//! unusable as printing nothing, and a raw envelope in `Outcome.stdout` would hide a verdict from
//! the gates parser and bury an error under JSON. This module turns each event into one short
//! progress line for the terminal and keeps the prose that matters for the capture. Pure functions
//! only: no I/O, so every rule is unit-testable without spawning anything.

use std::time::Duration;

/// How a child's stdout should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Already human-readable. The line is both shown and captured verbatim.
    #[default]
    Passthrough,
    /// `claude --output-format stream-json`: one JSON event per line.
    ClaudeStreamJson,
}

/// What one input line becomes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// Short progress lines for the terminal. Empty when the event is noise.
    pub display: Vec<String>,
    /// Text worth keeping in `Outcome.stdout`: assistant prose and the final result. This is what
    /// `gates::extract` searches for a verdict, so it must never be JSON-escaped.
    pub captured: Option<String>,
}

/// Decide what a human and `Outcome.stdout` should keep from one line of child output.
///
/// `Passthrough` is the identity case: validation commands and non-claude harnesses go through it
/// unchanged, so today's capture is byte-identical. A `ClaudeStreamJson` line that does not parse
/// as JSON is also passed through verbatim — a crash message or a warning must never be swallowed
/// just because the harness was asked for events.
pub fn render(format: OutputFormat, line: &str) -> Rendered {
    match format {
        OutputFormat::Passthrough => Rendered {
            // Not trimmed: the capture must be byte-identical to what the child wrote.
            display: vec![line.to_string()],
            captured: Some(line.to_string()),
        },
        OutputFormat::ClaudeStreamJson => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Rendered::default();
            }
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(value) => render_event(&value),
                Err(_) => Rendered {
                    display: vec![line.to_string()],
                    captured: Some(line.to_string()),
                },
            }
        }
    }
}

/// Render one well-formed stream-json event. Any shape the event can take ends in a value, never a
/// panic, and never a promise the event did not make.
fn render_event(event: &serde_json::Value) -> Rendered {
    let Some(kind) = event.get("type").and_then(serde_json::Value::as_str) else {
        return Rendered::default();
    };
    match kind {
        "system" => render_system(event),
        "assistant" => render_assistant(event),
        "user" => render_user(event),
        "result" => render_result(event),
        _ => Rendered::default(),
    }
}

/// `system` events frame a session. Only `init` is worth a line; the rest are transport noise.
fn render_system(event: &serde_json::Value) -> Rendered {
    if event.get("subtype").and_then(serde_json::Value::as_str) != Some("init") {
        return Rendered::default();
    }
    let model = event
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("claude");
    Rendered {
        display: vec![format!("session started ({model})")],
        captured: None,
    }
}

/// `assistant` events carry the agent's own output: prose, thinking, and tool calls.
fn render_assistant(event: &serde_json::Value) -> Rendered {
    let Some(blocks) = event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return Rendered::default();
    };

    let mut display = Vec::new();
    let mut prose = Vec::new();

    for block in blocks {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                    display.push(summarize(text));
                    prose.push(text.to_string());
                }
            }
            Some("thinking") => display.push("thinking…".to_string()),
            Some("tool_use") => display.push(tool_call(block)),
            _ => {}
        }
    }

    Rendered {
        display,
        captured: if prose.is_empty() {
            None
        } else {
            Some(prose.join("\n"))
        },
    }
}

/// `{name} {target}` for a tool call, or just the name when the input names no known key.
///
/// The target is the first present *string* among the keys a human recognises, so `TodoWrite` and
/// `Task` show their name alone and a `Read` of a path shows what it succeeds with.
fn tool_call(block: &serde_json::Value) -> String {
    let name = block
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let input = block.get("input");

    const KEYS: [&str; 7] = [
        "file_path",
        "path",
        "pattern",
        "command",
        "url",
        "query",
        "description",
    ];

    let target = KEYS
        .iter()
        .find_map(|key| input?.get(key).and_then(serde_json::Value::as_str))
        .map(summarize);
    let target = target.unwrap_or_default().trim().to_string();

    format!("{name} {target}").trim().to_string()
}

/// `user` events carry the tool results. The bulk of a tool result is a file the model asked to
/// read, which nobody wants on a terminal — only failures are worth a line.
fn render_user(event: &serde_json::Value) -> Rendered {
    let Some(blocks) = event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return Rendered::default();
    };

    let mut display = Vec::new();
    for block in blocks {
        let is_tool_result =
            block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result");
        let is_error = block.get("is_error").and_then(serde_json::Value::as_bool) == Some(true);
        if is_tool_result && is_error {
            let text = content_as_text(block.get("content"));
            display.push(format!("tool error: {}", summarize(&text)));
        }
    }

    Rendered {
        display,
        captured: None,
    }
}

/// `result` events end a turn; `subtype == "success"` means the turn ended the run.
fn render_result(event: &serde_json::Value) -> Rendered {
    let captured = event
        .get("result")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    // Fragments are omitted, never defaulted to zero: a fabricated `0 turns in 0s` would lie about
    // a run that returned no numbers.
    let mut fragments: Vec<String> = Vec::new();
    if let Some(turns) = event.get("num_turns").and_then(serde_json::Value::as_u64) {
        fragments.push(format!("{turns} turns"));
    }
    if let Some(ms) = event.get("duration_ms").and_then(serde_json::Value::as_u64) {
        fragments.push(format!("in {}", brief_duration(Duration::from_millis(ms))));
    }

    let mut line = String::from("done —");
    if !fragments.is_empty() {
        line.push(' ');
        line.push_str(&fragments.join(" "));
    }

    if let Some(cost) = event
        .get("total_cost_usd")
        .and_then(serde_json::Value::as_f64)
    {
        line.push_str(&format!(" (${cost:.2})"));
    }

    let is_error = event.get("is_error").and_then(serde_json::Value::as_bool) == Some(true)
        || event.get("subtype").and_then(serde_json::Value::as_str) != Some("success");
    if is_error {
        line = format!("error: {line}");
    }

    Rendered {
        display: vec![line],
        captured,
    }
}

/// A `content` field as text: either a plain string or an array of text blocks.
fn content_as_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Maximum width of one rendered progress line, in characters.
const DISPLAY_WIDTH: usize = 120;

/// One line, short enough to read at a glance.
///
/// Multi-line prose collapses to its first non-empty line; over-long lines are cut on a character
/// boundary so a multibyte character is never split mid-way into a panic.
fn summarize(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("");

    let mut chars: Vec<char> = first.chars().collect();
    if chars.len() <= DISPLAY_WIDTH {
        return first.to_string();
    }
    chars.truncate(DISPLAY_WIDTH);
    let mut out: String = chars.into_iter().collect();
    out.push('…');
    out
}

/// Compact durations for progress output: `45s`, `4m12s`, `1h05m`.
pub fn brief_duration(duration: Duration) -> String {
    let total = duration.as_secs();

    if total < 60 {
        return format!("{total}s");
    }
    if total < 3600 {
        return format!("{}m{:02}s", total / 60, total % 60);
    }

    format!("{}h{:02}m", total / 3600, (total % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::gates;

    fn claude(line: &str) -> Rendered {
        render(OutputFormat::ClaudeStreamJson, line)
    }

    #[test]
    fn passthrough_returns_the_line_unchanged() {
        let rendered = render(OutputFormat::Passthrough, "some text");

        assert_eq!(rendered.display, vec!["some text".to_string()]);
        assert_eq!(rendered.captured, Some("some text".to_string()));
    }

    #[test]
    fn a_line_that_is_not_json_is_never_swallowed() {
        // A crash message or a Node warning must reach both the terminal and the capture; hiding
        // it would remove the only explanation a stuck run leaves behind.
        let rendered = claude("Error: not logged in");

        assert_eq!(rendered.display, vec!["Error: not logged in".to_string()]);
        assert_eq!(rendered.captured, Some("Error: not logged in".to_string()));
    }

    #[test]
    fn an_assistant_tool_call_becomes_one_short_line() {
        let rendered = claude(
            r#"{"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#,
        );

        assert_eq!(rendered.display, vec!["Read src/main.rs".to_string()]);
        assert_eq!(rendered.captured, None);
    }

    #[test]
    fn assistant_prose_is_captured_in_full_but_displayed_short() {
        let text = format!("{}\nword", "a".repeat(400));
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": text }] }
        })
        .to_string();
        let rendered = claude(&line);

        assert_eq!(rendered.captured.as_deref(), Some(text.as_str()));
        assert_eq!(rendered.display.len(), 1);
        assert_eq!(rendered.display[0].chars().count(), 121);
        assert!(rendered.display[0].ends_with('…'));
    }

    #[test]
    fn the_final_result_text_is_captured_so_a_verdict_can_still_be_found() {
        // Regression guard: `gates::read` falls back to the reviewer's stdout, and a JSON-escaped
        // envelope would hide the verdict inside it (`{\"verdict\":\"PASS\"}`). The capture must
        // hold the assistant's real prose, not the event wrapper.
        let rendered = claude(
            r#"{"type":"result","subtype":"success","is_error":false,
                "result":"{\"verdict\":\"PASS\"}"}"#,
        );

        let captured = rendered.captured.unwrap();
        assert!(gates::extract(&captured).is_some_and(|v| v.passed()));
    }

    #[test]
    fn the_result_line_names_the_work_done() {
        let rendered = claude(
            r#"{"type":"result","subtype":"success","is_error":false,
                "duration_ms":1001000,"num_turns":34,"result":"done"}"#,
        );

        assert_eq!(
            rendered.display,
            vec!["done — 34 turns in 16m41s".to_string()]
        );
        assert_eq!(rendered.captured, Some("done".to_string()));
    }

    #[test]
    fn a_whole_session_is_recognised_as_starting() {
        let rendered =
            claude(r#"{"type":"system","subtype":"init","model":"claude-opus-5","cwd":"/tmp"}"#);

        assert_eq!(
            rendered.display,
            vec!["session started (claude-opus-5)".to_string()]
        );
        assert_eq!(rendered.captured, None);
    }

    #[test]
    fn successful_tool_results_are_silent_and_failing_ones_are_not() {
        let quiet = claude(
            r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t","content":"entire file contents","is_error":false}]}}"#,
        );
        assert!(quiet.display.is_empty(), "{:?}", quiet.display);

        let loud = claude(
            r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t","content":"boom","is_error":true}]}}"#,
        );
        assert_eq!(loud.display, vec!["tool error: boom".to_string()]);
    }

    #[test]
    fn a_well_formed_event_never_reaches_the_terminal_as_json() {
        let events = [
            r#"{"type":"system","subtype":"init","model":"m"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x","is_error":true}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"bye"}"#,
        ];

        for event in events {
            for line in claude(event).display {
                assert!(!line.contains("\"type\":"), "envelope leaked: {line}");
            }
        }
    }

    #[test]
    fn an_unknown_event_type_is_ignored() {
        let rendered = claude(r#"{"type":"stream_event"}"#);

        assert!(rendered.display.is_empty());
        assert!(rendered.captured.is_none());
    }

    #[test]
    fn a_tool_call_with_an_unrecognized_input_shows_the_name_alone() {
        let rendered = claude(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","name":"TodoWrite","input":{"todo":"x"}}]}}"#,
        );

        assert_eq!(rendered.display, vec!["TodoWrite".to_string()]);
    }

    #[test]
    fn the_session_line_has_no_model_when_the_event_omits_one() {
        let rendered = claude(r#"{"type":"system","subtype":"init"}"#);

        assert_eq!(
            rendered.display,
            vec!["session started (claude)".to_string()]
        );
    }

    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        // Regression guard: cutting a display line on a byte boundary panics by slicing into a
        // multibyte `—` or `é`. Agent prose is full of both, so the cut must count characters.
        let text = format!("{}—{}", "é".repeat(120), "é".repeat(180));
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": text }] }
        })
        .to_string();
        let rendered = claude(&line);

        assert_eq!(rendered.display[0].chars().count(), 121);
        assert!(rendered.display[0].ends_with('…'));
    }

    #[test]
    fn a_failed_result_is_flagged_as_an_error() {
        let rendered = claude(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"gave up"}"#,
        );

        assert_eq!(rendered.display, vec!["error: done —".to_string()]);
        assert_eq!(rendered.captured, Some("gave up".to_string()));
    }

    #[test]
    fn durations_are_printed_compactly() {
        assert_eq!(brief_duration(Duration::from_secs(45)), "45s");
        assert_eq!(brief_duration(Duration::from_secs(252)), "4m12s");
        assert_eq!(brief_duration(Duration::from_secs(3905)), "1h05m");
    }

    #[test]
    fn output_format_is_passthrough_by_default() {
        assert_eq!(OutputFormat::default(), OutputFormat::Passthrough);
    }
}
