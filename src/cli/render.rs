//! Terminal styling for what a person reads in a conversation.
//!
//! The shaper answers in Markdown, and printed raw a long reply is a wall of `**` and backticks —
//! the first real session was called painful to read. Styling is applied only when stdout is a
//! terminal: a gateway relaying `kage shape say` to a phone gets the plain Markdown, which a chat
//! app renders itself, and an escape code there would arrive as garbage. Hand-rolled rather than a
//! crate because the subset a reply uses — headings, bullets, bold, code — is a page of code.

use std::io::IsTerminal;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const NOT_BOLD: &str = "\x1b[22m";
const DIM: &str = "\x1b[2m";
const CODE: &str = "\x1b[36m";
const NOT_CODE: &str = "\x1b[39m";
const HEADING: &str = "\x1b[1;35m";
const BULLET: &str = "\x1b[33m";

/// Whether to style output: stdout is a terminal and `NO_COLOR` is unset.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub on: bool,
}

impl Style {
    pub fn detect() -> Self {
        Self {
            on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    /// A horizontal rule carrying a label, in the given SGR colour (`"36"` is cyan).
    pub fn rule(&self, label: &str, colour: &str) -> String {
        let line = format!(
            "── {label} {}",
            "─".repeat(56usize.saturating_sub(label.len()))
        );
        if self.on {
            format!("\x1b[{colour}m{line}{RESET}")
        } else {
            line
        }
    }

    pub fn dim(&self, text: &str) -> String {
        self.wrap(DIM, text)
    }

    pub fn bold(&self, text: &str, colour: &str) -> String {
        self.wrap(&format!("\x1b[1;{colour}m"), text)
    }

    fn wrap(&self, open: &str, text: &str) -> String {
        if self.on {
            format!("{open}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    /// Markdown as the terminal should show it; unchanged when styling is off.
    pub fn markdown(&self, text: &str) -> String {
        if !self.on {
            return text.to_string();
        }
        let mut out = String::new();
        let mut fenced = false;
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                out.push_str(&format!("  {CODE}{line}{RESET}\n"));
                continue;
            }
            let indent = &line[..line.len() - trimmed.len()];
            let heading = trimmed.trim_start_matches('#');
            if heading.len() < trimmed.len() && heading.starts_with(' ') {
                out.push_str(&format!(
                    "{HEADING}{}{RESET}\n",
                    strip_inline(heading.trim())
                ));
            } else if let Some(item) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                out.push_str(&format!("{indent}{BULLET}•{RESET} {}\n", inline(item)));
            } else if let Some(quote) = trimmed.strip_prefix("> ") {
                out.push_str(&format!("{indent}{DIM}│ {}{RESET}\n", strip_inline(quote)));
            } else {
                out.push_str(&format!("{indent}{}\n", inline(trimmed)));
            }
        }
        out.truncate(out.trim_end().len());
        out
    }
}

/// `**bold**` and `` `code` `` within one line. An unclosed marker styles to the end of the line
/// and is reset there, so one stray `**` cannot bold the rest of the reply.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let (mut bold, mut code) = (false, false);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if !code && rest.starts_with("**") {
            out.push_str(if bold { NOT_BOLD } else { BOLD });
            bold = !bold;
            rest = &rest[2..];
        } else if c == '`' {
            out.push_str(if code { NOT_CODE } else { CODE });
            code = !code;
            rest = &rest[1..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if bold || code {
        out.push_str(RESET);
    }
    out
}

/// A heading or quote carries its own style; its inline markers are dropped, not nested.
fn strip_inline(text: &str) -> String {
    text.replace("**", "").replace('`', "")
}

/// A line as typed, minus what the terminal added. Shift+Enter in terminals that report modified
/// keys (`ESC[27;2;13~`, or `ESC[13;2u`) arrives as text on the same line rather than as a newline,
/// and was sent to the shaper verbatim as `^[[27;2;13~`; it means "new line", so it becomes one.
/// Any other escape sequence — an arrow key pressed mid-line — is dropped.
pub fn clean_input(line: &str) -> String {
    let line = line
        .replace("\x1b[27;2;13~", "\n")
        .replace("\x1b[13;2u", "\n");
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // A CSI sequence ends at its first byte in @..~.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: Style = Style { on: true };
    const OFF: Style = Style { on: false };

    #[test]
    fn markdown_is_styled_for_a_terminal_and_left_alone_for_a_pipe() {
        let reply = "## Plan\n- **T3** adds `business_line`\n```\nnpm test\n```";

        let styled = ON.markdown(reply);

        assert!(!styled.contains("**") && !styled.contains("```") && !styled.contains("## "));
        assert!(
            styled.contains(&format!("{BOLD}T3{NOT_BOLD}")),
            "{styled:?}"
        );
        assert!(styled.contains(&format!("{CODE}business_line{NOT_CODE}")));
        assert!(styled.contains('•'));
        assert_eq!(
            OFF.markdown(reply),
            reply,
            "a gateway gets the Markdown itself"
        );
    }

    #[test]
    fn an_unclosed_marker_is_reset_at_the_end_of_its_line() {
        let styled = ON.markdown("a **stray\nnext");

        assert!(
            styled.starts_with(&format!("a {BOLD}stray{RESET}\n")),
            "{styled:?}"
        );
    }

    #[test]
    fn shift_enter_becomes_a_new_line_instead_of_escape_garbage() {
        // The first live chat sent "sihh^[[27;2;13~terus" to the shaper: Shift+Enter in a terminal
        // that reports modified keys.
        assert_eq!(clean_input("sihh\x1b[27;2;13~terus"), "sihh\nterus");
        assert_eq!(clean_input("a\x1b[13;2ub"), "a\nb");
        assert_eq!(
            clean_input("ab\x1b[Dc"),
            "abc",
            "an arrow key leaves nothing"
        );
    }
}
