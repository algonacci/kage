//! Shaping: turning a conversation into a PRD and tickets the loop can run.
//!
//! A shaping session is a directory, not a process. Every turn is one invocation that reads the
//! session from disk, asks the shaper, and writes the session back — so the same session can be
//! driven from a terminal now and from a phone through Kumo later, with nothing to keep alive in
//! between and nothing lost when the machine restarts.
//!
//! The session's memory is its artifacts. `PRD.md` and `TICKETS.md` are rewritten each turn and
//! carry everything that was decided; only the last few turns of conversation are replayed beside
//! them. That is "artifacts, not chat history" applied to the one place Kage does talk: a harness's
//! own resume would tie the memory to one tool and hide it from the person whose plan it is.
//!
//! The shaper never writes a file. It answers in a fixed envelope and Kage writes the artifacts, so
//! the harness can run without write permission and a reply that breaks the envelope changes
//! nothing on disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Project, ShapeConfig};

/// Markers of the reply envelope. Spelled to be unmistakable in prose and in Markdown.
pub const REPLY_MARKER: &str = "=== REPLY ===";
pub const PRD_MARKER: &str = "=== PRD ===";
pub const TICKETS_MARKER: &str = "=== TICKETS ===";
pub const END_MARKER: &str = "=== END ===";
/// What the shaper writes in place of an artifact it did not change. Re-sending an unchanged PRD
/// every turn would spend output tokens on a copy and invite drift in the parts it retyped.
pub const UNCHANGED: &str = "UNCHANGED";

/// A file inlined from `shape.context` is cut here. A note longer than this is a whole vault
/// folder pasted by mistake, and every turn would pay for it.
const MAX_CONTEXT_CHARS: usize = 40_000;

/// One exchange, as recorded in `turns.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub at: DateTime<Utc>,
    pub user: String,
    pub reply: String,
}

/// A shaping session on disk.
#[derive(Debug, Clone)]
pub struct Session {
    pub slug: String,
    pub dir: PathBuf,
}

impl Session {
    pub fn prd(&self) -> PathBuf {
        self.dir.join("PRD.md")
    }

    pub fn tickets(&self) -> PathBuf {
        self.dir.join("TICKETS.md")
    }

    /// The machine record of the conversation. JSON lines rather than Markdown, so a message that
    /// itself contains a heading cannot be mistaken for the start of the next turn.
    pub fn turns_file(&self) -> PathBuf {
        self.dir.join("turns.jsonl")
    }

    /// The human-readable conversation, regenerated from `turns.jsonl` after every turn.
    pub fn conversation(&self) -> PathBuf {
        self.dir.join("CONVERSATION.md")
    }

    pub fn prompts_dir(&self) -> PathBuf {
        self.dir.join("prompts")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn read_prd(&self) -> String {
        std::fs::read_to_string(self.prd()).unwrap_or_default()
    }

    pub fn read_tickets(&self) -> String {
        std::fs::read_to_string(self.tickets()).unwrap_or_default()
    }

    pub fn turns(&self) -> Result<Vec<Turn>> {
        let path = self.turns_file();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(index, line)| {
                serde_json::from_str(line).with_context(|| {
                    format!(
                        "line {} of {} is not a valid turn",
                        index + 1,
                        path.display()
                    )
                })
            })
            .collect()
    }

    /// Persist one completed turn: the artifacts it changed, then the record of the exchange.
    ///
    /// Artifacts first. A crash between the two leaves an updated plan with the exchange missing
    /// from the log — the next turn still sees every decision — rather than a logged exchange whose
    /// decisions were never written.
    pub fn record(&self, user: &str, reply: &Reply) -> Result<()> {
        if let Some(prd) = &reply.prd {
            write_artifact(&self.prd(), prd)?;
        }
        if let Some(tickets) = &reply.tickets {
            write_artifact(&self.tickets(), tickets)?;
        }

        let turn = Turn {
            at: Utc::now(),
            user: user.to_string(),
            reply: reply.message.clone(),
        };
        let mut line = serde_json::to_string(&turn).context("cannot encode the turn")?;
        line.push('\n');

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.turns_file())
            .with_context(|| format!("cannot open {}", self.turns_file().display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("cannot append to {}", self.turns_file().display()))?;

        let turns = self.turns()?;
        write_artifact(
            &self.conversation(),
            &render_conversation(&self.slug, &turns),
        )
    }
}

fn write_artifact(path: &Path, text: &str) -> Result<()> {
    let mut text = text.trim().to_string();
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

fn render_conversation(slug: &str, turns: &[Turn]) -> String {
    let mut out = format!("# Shaping session `{slug}`\n");
    for turn in turns {
        out.push_str(&format!(
            "\n---\n\n**You** · {}\n\n{}\n\n**Shaper**\n\n{}\n",
            turn.at.format("%Y-%m-%d %H:%M UTC"),
            turn.user.trim(),
            turn.reply.trim()
        ));
    }
    out
}

/// Where every session of a project lives.
pub fn sessions_dir(project: &Project) -> PathBuf {
    project.kage_dir.join("shape")
}

/// Where exported tickets are written, ready for `kage run --task-file`.
pub fn tickets_dir(project: &Project) -> PathBuf {
    project.kage_dir.join("tickets")
}

/// Open an existing session by slug.
pub fn open(project: &Project, slug: &str) -> Result<Session> {
    let dir = sessions_dir(project).join(slug);
    if !dir.is_dir() {
        let known = list(project).unwrap_or_default();
        bail!(
            "no shaping session `{slug}`\nknown: {}\nstart one with:  kage shape new \"<idea>\"",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    }
    Ok(Session {
        slug: slug.to_string(),
        dir,
    })
}

/// Create a session for an idea, named after its first words.
pub fn create(project: &Project, idea: &str) -> Result<Session> {
    let base = slugify(idea);
    let root = sessions_dir(project);
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;

    let slug = (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|slug| !root.join(slug).exists())
        .context("ran out of session names")?;

    let dir = root.join(&slug);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(Session { slug, dir })
}

/// Every session slug, oldest first by name.
pub fn list(project: &Project) -> Result<Vec<String>> {
    let root = sessions_dir(project);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut slugs: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    slugs.sort();
    Ok(slugs)
}

/// A filesystem- and shell-safe name from the first words of an idea.
///
/// ASCII only: the slug is typed on a phone keyboard and passed through Kumo as an argument, and a
/// diacritic there is one more way for it not to match. An idea with no ASCII words at all still
/// gets a usable name.
pub fn slugify(idea: &str) -> String {
    let words: Vec<String> = idea
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .take(6)
        .collect();

    let mut slug = String::new();
    for word in words {
        if slug.len() + word.len() + 1 > 40 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }

    if slug.is_empty() {
        format!("shape-{}", chrono::Local::now().format("%Y%m%d"))
    } else {
        slug
    }
}

/// The shaper's answer, split out of its envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// What the person reads.
    pub message: String,
    /// The new PRD, or `None` when the shaper left it unchanged.
    pub prd: Option<String>,
    /// The new ticket list, or `None` when unchanged.
    pub tickets: Option<String>,
}

/// Find the envelope in a harness's output.
///
/// The *last* envelope wins. A streaming harness's captured output holds its prose as it went and
/// then the final result, so the finished answer appears after any draft of it; and a shaper that
/// quotes the format back while thinking must not have that quotation parsed as its answer.
/// Missing `END` is a rejection rather than a guess: it is what a reply cut off by a timeout looks
/// like, and writing a half-received ticket list over a whole one destroys work.
pub fn parse_reply(output: &str) -> Result<Reply> {
    let Some(start) = output.rfind(REPLY_MARKER) else {
        bail!(
            "the shaper's answer has no `{REPLY_MARKER}` envelope, so nothing was saved\n\
             the session is unchanged — send the message again"
        );
    };
    let envelope = &output[start + REPLY_MARKER.len()..];

    let Some(end) = envelope.find(END_MARKER) else {
        bail!(
            "the shaper's answer stops before `{END_MARKER}` — it was probably cut off\n\
             nothing was saved; send the message again"
        );
    };
    let envelope = &envelope[..end];

    let (message, rest) = envelope
        .split_once(PRD_MARKER)
        .with_context(|| format!("the shaper's answer has no `{PRD_MARKER}` section"))?;
    let (prd, tickets) = rest
        .split_once(TICKETS_MARKER)
        .with_context(|| format!("the shaper's answer has no `{TICKETS_MARKER}` section"))?;

    let message = message.trim();
    if message.is_empty() {
        bail!("the shaper's answer has an empty `{REPLY_MARKER}` section");
    }

    Ok(Reply {
        message: message.to_string(),
        prd: changed(prd),
        tickets: changed(tickets),
    })
}

/// A section's new content, or `None` when it says `UNCHANGED` or nothing at all. An empty section
/// is read as unchanged rather than as "delete everything": losing a PRD to a lazy reply is the
/// worse mistake.
fn changed(section: &str) -> Option<String> {
    let text = section.trim();
    (!text.is_empty() && text != UNCHANGED).then(|| text.to_string())
}

/// The notes named in `shape.context`, rendered for the prompt.
///
/// A missing entry is reported in the rendered text instead of failing the turn: the conversation
/// is still worth having, and the shaper telling the person "your HRIS notes were not found" is
/// how they learn the path is wrong.
pub fn load_context(root: &Path, config: &ShapeConfig) -> String {
    let mut out = String::new();
    for entry in &config.context {
        let path = root.join(entry);
        let file = if path.is_dir() {
            path.join("README.md")
        } else {
            path.clone()
        };

        match std::fs::read_to_string(&file) {
            Ok(text) => {
                let text = match text.char_indices().nth(MAX_CONTEXT_CHARS) {
                    None => text,
                    Some((cut, _)) => format!(
                        "{}\n\n[cut at {MAX_CONTEXT_CHARS} characters]",
                        &text[..cut]
                    ),
                };
                out.push_str(&format!("\n## {}\n\n{}\n", entry, text.trim()));
            }
            Err(_) => out.push_str(&format!(
                "\n## {}\n\n[not found at {} — tell the user this context entry is wrong]\n",
                entry,
                file.display()
            )),
        }
    }
    out
}

/// One ticket in `TICKETS.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// `T1`, `T2`, … as the shaper numbered it.
    pub id: String,
    pub title: String,
    pub ready: bool,
    /// The whole section below the heading, which becomes the task file.
    pub body: String,
}

impl Ticket {
    /// Why this ticket must not be handed to a run, if it must not.
    ///
    /// A ticket is only as good as its definition of done: that is the part the planner turns into
    /// acceptance criteria and the reviewer judges against. "Spec harus lengkap" is enforced here
    /// rather than left to discipline.
    pub fn not_runnable(&self) -> Option<String> {
        if !self.ready {
            return Some("still a draft".to_string());
        }
        let done = section(&self.body, "definition of done");
        let has_item = done.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("- ") || line.starts_with("* ")
        });
        if !has_item {
            return Some("marked ready but has no `### Definition of done` list".to_string());
        }
        None
    }
}

/// Split `TICKETS.md` into tickets. A ticket is a `## T<n>: <title>` heading and everything up to
/// the next ticket heading; `Status: ready` anywhere in it marks it ready.
pub fn parse_tickets(text: &str) -> Vec<Ticket> {
    let mut tickets = Vec::new();
    let mut current: Option<(String, String, Vec<&str>)> = None;

    for line in text.lines() {
        if let Some((id, title)) = ticket_heading(line) {
            if let Some((id, title, lines)) = current.take() {
                tickets.push(finish(id, title, &lines));
            }
            current = Some((id, title, Vec::new()));
        } else if let Some((_, _, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    if let Some((id, title, lines)) = current {
        tickets.push(finish(id, title, &lines));
    }
    tickets
}

fn ticket_heading(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("## ")?.trim();
    let (id, title) = rest.split_once(':')?;
    let id = id.trim();
    let numbered =
        id.len() > 1 && id.starts_with(['T', 't']) && id[1..].chars().all(|c| c.is_ascii_digit());
    numbered.then(|| (id.to_ascii_uppercase(), title.trim().to_string()))
}

fn finish(id: String, title: String, lines: &[&str]) -> Ticket {
    let body = lines.join("\n").trim().to_string();
    let ready = body.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.strip_prefix("status:")
            .is_some_and(|value| value.trim().starts_with("ready"))
    });
    Ticket {
        id,
        title,
        ready,
        body,
    }
}

/// The text under a `###` heading, up to the next heading of any depth.
fn section(body: &str, name: &str) -> String {
    let mut collected = Vec::new();
    let mut inside = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            if inside {
                break;
            }
            inside = trimmed
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case(name);
            continue;
        }
        if inside {
            collected.push(line);
        }
    }
    collected.join("\n")
}

/// The task file for one ticket.
///
/// The PRD travels with it. A ticket is written to be read beside the PRD, and the run that
/// receives this file sees nothing else from the session — `.kage/` is never committed, so an
/// isolated worktree cannot reach the session directory at all.
pub fn task_file(session: &Session, ticket: &Ticket, prd: &str) -> String {
    let mut out = format!("# {}: {}\n\n{}\n", ticket.id, ticket.title, ticket.body);
    if !prd.trim().is_empty() {
        out.push_str(&format!(
            "\n---\n\n# Product context\n\nFrom shaping session `{}`. Implement only the ticket \
             above; the rest of this PRD is context, not scope.\n\n{}\n",
            session.slug,
            prd.trim()
        ));
    }
    out
}

/// What exporting one ticket did.
pub enum Exported {
    Written(PathBuf),
    Skipped { id: String, reason: String },
}

/// Write every runnable ticket to `.kage/tickets/`, and say why each other one was left out.
pub fn export(project: &Project, session: &Session) -> Result<Vec<Exported>> {
    let tickets = parse_tickets(&session.read_tickets());
    if tickets.is_empty() {
        bail!(
            "session `{}` has no tickets yet — keep shaping until TICKETS.md has `## T1: …` \
             sections",
            session.slug
        );
    }

    let dir = tickets_dir(project);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let prd = session.read_prd();

    let mut results = Vec::new();
    for ticket in &tickets {
        if let Some(reason) = ticket.not_runnable() {
            results.push(Exported::Skipped {
                id: ticket.id.clone(),
                reason,
            });
            continue;
        }
        let path = dir.join(format!(
            "{}-{}.md",
            session.slug,
            ticket.id.to_ascii_lowercase()
        ));
        std::fs::write(&path, task_file(session, ticket, &prd))
            .with_context(|| format!("cannot write {}", path.display()))?;
        results.push(Exported::Written(path));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(reply: &str, prd: &str, tickets: &str) -> String {
        format!(
            "{REPLY_MARKER}\n{reply}\n{PRD_MARKER}\n{prd}\n{TICKETS_MARKER}\n{tickets}\n{END_MARKER}\n"
        )
    }

    #[test]
    fn a_complete_envelope_yields_the_message_and_both_artifacts() {
        let reply = parse_reply(&envelope(
            "Siapa usernya?",
            "# PRD\nGoal",
            "## T1: A\nStatus: draft",
        ))
        .unwrap();

        assert_eq!(reply.message, "Siapa usernya?");
        assert_eq!(reply.prd.as_deref(), Some("# PRD\nGoal"));
        assert_eq!(reply.tickets.as_deref(), Some("## T1: A\nStatus: draft"));
    }

    #[test]
    fn unchanged_or_empty_sections_leave_the_artifact_alone() {
        // An empty section read as "delete" would let one lazy reply wipe a PRD built over a dozen
        // turns.
        let reply = parse_reply(&envelope("ok", UNCHANGED, "")).unwrap();

        assert_eq!(reply.prd, None);
        assert_eq!(reply.tickets, None);
    }

    #[test]
    fn the_last_envelope_wins_over_drafts_and_quotations() {
        // A streaming harness's output holds its prose as it went, then the final result; the
        // finished answer is the last one.
        let output = format!(
            "thinking: I will answer as\n{}\n{}",
            envelope("draft", "old", UNCHANGED),
            envelope("final", "new", UNCHANGED)
        );

        let reply = parse_reply(&output).unwrap();

        assert_eq!(reply.message, "final");
        assert_eq!(reply.prd.as_deref(), Some("new"));
    }

    #[test]
    fn a_reply_cut_off_before_its_end_marker_is_rejected() {
        // What a timeout looks like. Saving a half-received ticket list over a whole one destroys
        // work, so nothing is saved.
        let cut = format!("{REPLY_MARKER}\nhi\n{PRD_MARKER}\n# PRD\n{TICKETS_MARKER}\n## T1: half");

        let error = parse_reply(&cut).unwrap_err().to_string();

        assert!(error.contains("cut off"), "{error}");
        assert!(error.contains("nothing was saved"), "{error}");
    }

    #[test]
    fn prose_with_no_envelope_is_rejected_with_a_remedy() {
        let error = parse_reply("Sure! Here's my plan...")
            .unwrap_err()
            .to_string();

        assert!(error.contains("send the message again"), "{error}");
    }

    #[test]
    fn a_slug_is_short_ascii_and_never_empty() {
        assert_eq!(
            slugify("Bikin fitur cuti di HRIS, approval 2 level!"),
            "bikin-fitur-cuti-di-hris-approval"
        );
        assert!(slugify(&"panjang ".repeat(30)).len() <= 40);
        assert!(slugify("🚀🚀🚀").starts_with("shape-"));
    }

    const TICKETS: &str = "# Tickets\n\n\
        ## T1: Leave request form\n\
        Status: ready\n\
        ### Task\nAdd the form.\n\
        ### Definition of done\n- form saves a request\n- `npm test` passes\n\n\
        ## T2: Approval chain\n\
        Status: draft\n\
        ### Task\nTwo levels.\n\n\
        ## T3: Email on approval\n\
        Status: ready\n\
        ### Task\nSend mail.\n";

    #[test]
    fn tickets_are_split_on_their_headings_with_status_read() {
        let tickets = parse_tickets(TICKETS);

        assert_eq!(tickets.len(), 3);
        assert_eq!(tickets[0].id, "T1");
        assert_eq!(tickets[0].title, "Leave request form");
        assert!(tickets[0].ready);
        assert!(!tickets[1].ready);
        assert!(tickets[0].body.contains("`npm test` passes"));
        assert!(
            !tickets[0].body.contains("Approval chain"),
            "a ticket ends at the next one"
        );
    }

    #[test]
    fn only_a_ready_ticket_with_a_definition_of_done_is_runnable() {
        let tickets = parse_tickets(TICKETS);

        assert_eq!(tickets[0].not_runnable(), None);
        assert_eq!(tickets[1].not_runnable().as_deref(), Some("still a draft"));
        let reason = tickets[2].not_runnable().unwrap();
        assert!(reason.contains("Definition of done"), "{reason}");
    }

    #[test]
    fn a_task_file_carries_the_prd_as_context_not_scope() {
        let session = Session {
            slug: "cuti".to_string(),
            dir: PathBuf::from("."),
        };
        let ticket = &parse_tickets(TICKETS)[0];

        let text = task_file(&session, ticket, "# PRD\nLeave management.");

        assert!(text.starts_with("# T1: Leave request form"), "{text}");
        assert!(text.contains("Leave management."), "{text}");
        assert!(text.contains("context, not scope"), "{text}");
    }

    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kage-shape-{label}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn context_inlines_a_directory_readme_and_flags_a_missing_entry() {
        let root = temp_root("context");
        std::fs::create_dir_all(root.join("notes/HRIS")).unwrap();
        std::fs::write(root.join("notes/HRIS/README.md"), "HRIS index").unwrap();
        std::fs::create_dir_all(root.join("notes/Secrets")).unwrap();
        std::fs::write(root.join("notes/Secrets/token.md"), "do-not-send").unwrap();
        let config = ShapeConfig {
            context: vec!["notes/HRIS".to_string(), "notes/Nope".to_string()],
            ..ShapeConfig::default()
        };

        let text = load_context(&root, &config);

        assert!(text.contains("HRIS index"), "{text}");
        assert!(
            text.contains("notes/Nope") && text.contains("not found"),
            "{text}"
        );
        assert!(
            !text.contains("do-not-send"),
            "only named entries are read: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recorded_turn_updates_artifacts_and_the_conversation() {
        let root = temp_root("record");
        let session = Session {
            slug: "cuti".to_string(),
            dir: root.clone(),
        };

        session
            .record(
                "Bikin fitur cuti",
                &Reply {
                    message: "Berapa level approval?".to_string(),
                    prd: Some("# PRD v1".to_string()),
                    tickets: None,
                },
            )
            .unwrap();
        session
            .record(
                "Dua level",
                &Reply {
                    message: "Oke".to_string(),
                    prd: None,
                    tickets: Some("## T1: Form".to_string()),
                },
            )
            .unwrap();

        assert_eq!(
            session.read_prd().trim(),
            "# PRD v1",
            "an unchanged PRD survives"
        );
        assert_eq!(session.read_tickets().trim(), "## T1: Form");
        let turns = session.turns().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].user, "Dua level");
        let conversation = std::fs::read_to_string(session.conversation()).unwrap();
        assert!(
            conversation.contains("Berapa level approval?"),
            "{conversation}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
