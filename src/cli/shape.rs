//! `kage shape` — talk an idea into a PRD and runnable tickets.
//!
//! Every subcommand that talks is one turn, or a loop of single turns. `say` is the primitive: it
//! is what a messaging gateway calls with each message from a phone, and `chat` is nothing more
//! than `say` in a terminal loop, so the two can never behave differently.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::adapters::{self, AgentRequest, Role, preflight};
use crate::config::{Config, Project};
use crate::engine::prompts;
use crate::shape::{self, Exported, Reply, Session};

/// `kage shape new "<idea>"` — open a session and take its first turn.
pub async fn new(cwd: &Path, idea: &str, chat_after: bool) -> Result<()> {
    if idea.trim().is_empty() {
        bail!("the idea cannot be empty");
    }
    let (project, config) = load(cwd)?;
    // Before the session directory exists: a shaper that cannot be spawned must not leave an empty
    // session behind for every attempt.
    preflight::check(&config, &[Role::Shaper])?;

    let session = shape::create(&project, idea)?;
    println!("Session: {}  ({})", session.slug, session.dir.display());

    let reply = turn(&project, &config, &session, idea).await?;
    print_reply(&session, &reply);

    if chat_after {
        chat_loop(&project, &config, &session).await?;
    } else {
        println!(
            "\nContinue with:  kage shape say {} \"<message>\"   or   kage shape chat {}",
            session.slug, session.slug
        );
    }
    Ok(())
}

/// `kage shape say <slug> "<message>"` — one turn.
pub async fn say(cwd: &Path, slug: &str, message: &str) -> Result<()> {
    if message.trim().is_empty() {
        bail!("the message cannot be empty");
    }
    let (project, config) = load(cwd)?;
    let session = shape::open(&project, slug)?;
    preflight::check(&config, &[Role::Shaper])?;

    let reply = turn(&project, &config, &session, message).await?;
    print_reply(&session, &reply);
    Ok(())
}

/// `kage shape chat <slug>` — turns in a loop until `/exit` or end of input.
pub async fn chat(cwd: &Path, slug: &str) -> Result<()> {
    let (project, config) = load(cwd)?;
    let session = shape::open(&project, slug)?;
    preflight::check(&config, &[Role::Shaper])?;

    println!("Session: {}  ({})", session.slug, session.dir.display());
    chat_loop(&project, &config, &session).await
}

/// `kage shape show <slug>` — where the session stands, without spending a turn.
pub fn show(cwd: &Path, slug: &str) -> Result<()> {
    let (project, _) = load(cwd)?;
    let session = shape::open(&project, slug)?;
    print_status(&session)
}

/// `kage shape list` — every session in this project.
pub fn list(cwd: &Path) -> Result<()> {
    let (project, _) = load(cwd)?;
    let slugs = shape::list(&project)?;
    if slugs.is_empty() {
        println!("No shaping sessions yet. Start one:  kage shape new \"<idea>\"");
        return Ok(());
    }
    for slug in slugs {
        let session = shape::open(&project, &slug)?;
        let turns = session.turns().map(|turns| turns.len()).unwrap_or(0);
        println!("{slug}  · {turns} turn(s) · {}", ticket_summary(&session));
    }
    Ok(())
}

/// `kage shape attach <slug> <file>...` — copy documents into the session without spending a turn.
pub fn attach(cwd: &Path, slug: &str, files: &[PathBuf]) -> Result<()> {
    let (project, _) = load(cwd)?;
    let session = shape::open(&project, slug)?;
    for file in files {
        print_attached(&project, &session.attach(&cwd.join(file))?.path);
    }
    println!("The shaper reads them from your next message.");
    Ok(())
}

/// `kage shape export <slug>` — write each runnable ticket as a task file.
pub fn export(cwd: &Path, slug: &str) -> Result<()> {
    let (project, _) = load(cwd)?;
    let session = shape::open(&project, slug)?;
    print_export(&shape::export(&project, &session)?);
    Ok(())
}

fn load(cwd: &Path) -> Result<(Project, Config)> {
    let project = Project::discover(cwd)?;
    let config = project.load_config()?;
    Ok((project, config))
}

/// One turn: assemble the prompt from the session, ask the shaper, save what it decided.
///
/// The session is only written after the reply parses. A failed turn — the harness died, the reply
/// broke the envelope — leaves the session exactly as it was, so the remedy is always "send it
/// again" and never "repair the files first".
async fn turn(
    project: &Project,
    config: &Config,
    session: &Session,
    message: &str,
) -> Result<Reply> {
    let adapter = adapters::build(Role::Shaper, config)?;

    let turns = session.turns()?;
    let omitted = turns.len().saturating_sub(config.shape.history_turns);
    let recent: Vec<(String, String)> = turns[omitted..]
        .iter()
        .map(|turn| (turn.user.clone(), turn.reply.clone()))
        .collect();

    let context = shape::load_context(&project.root, &config.shape);
    let prd = session.read_prd();
    let tickets = session.read_tickets();
    // A file named in the message is attached before the shaper sees it: the owner should be able
    // to say "check REVIEW.pdf in my downloads" and never learn that the shaper cannot reach there.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for file in shape::mentioned_files(message, &project.root, home.as_deref()) {
        match session.attach(&file) {
            Ok(attached) if attached.copied => print_attached(project, &attached.path),
            Ok(_) => {}
            Err(error) => println!("  could not attach {}: {error:#}", file.display()),
        }
    }
    let attachments = session.attachments();

    let prompt = prompts::shaper(&prompts::ShapeTurn {
        workdir: &project.root,
        can_read_repo: adapter.writes_own_artifacts(),
        context: &context,
        prd: &prd,
        tickets: &tickets,
        recent: &recent,
        omitted_turns: omitted,
        attachments: &attachments,
        message,
    });

    let number = turns.len() + 1;
    let log_path = session.logs_dir().join(format!("turn-{number:03}.log"));
    std::fs::create_dir_all(session.logs_dir())
        .with_context(|| format!("cannot create {}", session.logs_dir().display()))?;
    println!("  {}", adapter.describe());

    let result = adapter
        .run(AgentRequest {
            prompt,
            prompt_file: session.prompts_dir().join(format!("turn-{number:03}.md")),
            // The project root, never a worktree: shaping reads the code as it stands and changes
            // nothing, so there is nothing to isolate.
            workdir: project.root.clone(),
            log_path: log_path.clone(),
            label: "shaper".to_string(),
            echo: false,
        })
        .await?;

    if result.timed_out {
        bail!(
            "the shaper ran past its {}-second budget — nothing was saved\n\
             raise `roles.shaper.timeout_secs`, or send a narrower message",
            config.roles.shaper.timeout_secs
        );
    }
    if result.stalled {
        bail!(
            "the shaper went silent and was stopped as hung — nothing was saved\n\
             its log is at {}",
            log_path.display()
        );
    }
    if !result.success() {
        let stderr = result.stderr.trim();
        bail!(
            "the shaper exited with {} — nothing was saved\n{}log: {}",
            result
                .code
                .map(|code| format!("code {code}"))
                .unwrap_or_else(|| "no exit code".to_string()),
            if stderr.is_empty() {
                String::new()
            } else {
                format!("{}\n", stderr.lines().last().unwrap_or_default())
            },
            log_path.display()
        );
    }

    let reply = shape::parse_reply(&result.stdout)
        .with_context(|| format!("the full answer is in {}", log_path.display()))?;
    session.record(message, &reply)?;
    Ok(reply)
}

async fn chat_loop(project: &Project, config: &Config, session: &Session) -> Result<()> {
    println!(
        "\nType a message and press Enter. End a line with \\ to continue it.\n\
         /attach <file>  hand the shaper a document · /show  where it stands\n\
         /export  write ready tickets · /exit  leave\n"
    );

    let stdin = std::io::stdin();
    loop {
        let Some(message) = read_message(&stdin)? else {
            println!();
            return Ok(());
        };
        match message.trim() {
            "" => continue,
            "/exit" | "/quit" => return Ok(()),
            "/show" => {
                print_status(session)?;
                continue;
            }
            "/export" => {
                match shape::export(project, session) {
                    Ok(results) => print_export(&results),
                    Err(error) => println!("{error:#}"),
                }
                continue;
            }
            _ => {}
        }
        if let Some(typed) = message.trim().strip_prefix("/attach") {
            let typed = typed.trim();
            if typed.is_empty() {
                println!(
                    "usage: /attach <file>   (dragging the file into the terminal types its path)"
                );
                continue;
            }
            let home = std::env::var_os("HOME").map(PathBuf::from);
            match session.attach(&shape::typed_path(typed, home.as_deref())) {
                Ok(attached) => {
                    print_attached(project, &attached.path);
                    println!("Now tell the shaper what to do with it.\n");
                }
                Err(error) => println!("{error:#}\n"),
            }
            continue;
        }

        // A failed turn is reported and the loop carries on: the session is unchanged, and leaving
        // the chat over one bad reply would make the person retype their context to get back.
        match turn(project, config, session, &message).await {
            Ok(reply) => print_reply(session, &reply),
            Err(error) => println!("\n{error:#}\n"),
        }
    }
}

/// One message from the terminal, joining lines that end in `\`. `None` at end of input.
fn read_message(stdin: &std::io::Stdin) -> Result<Option<String>> {
    let mut message = String::new();
    loop {
        print!("{}", if message.is_empty() { "you> " } else { "...> " });
        std::io::stdout().flush().ok();

        let mut line = String::new();
        let read = stdin
            .lock()
            .read_line(&mut line)
            .context("cannot read from the terminal")?;
        if read == 0 {
            return Ok((!message.is_empty()).then_some(message));
        }

        let line = line.trim_end_matches(['\n', '\r']);
        match line.strip_suffix('\\') {
            Some(continued) => {
                message.push_str(continued);
                message.push('\n');
            }
            None => {
                message.push_str(line);
                return Ok(Some(message));
            }
        }
    }
}

fn print_attached(project: &Project, path: &Path) {
    let shown = path.strip_prefix(&project.root).unwrap_or(path);
    println!("attached {}", shown.display());
}

fn print_reply(session: &Session, reply: &Reply) {
    println!("\n{}\n", "-".repeat(60));
    println!("{}", reply.message.trim());
    println!("\n{}", "-".repeat(60));

    let mut changed = Vec::new();
    if reply.prd.is_some() {
        changed.push("PRD updated");
    }
    if reply.tickets.is_some() {
        changed.push("tickets updated");
    }
    let changed = if changed.is_empty() {
        "no artifact changed".to_string()
    } else {
        changed.join(", ")
    };
    println!("{changed} · {}\n", ticket_summary(session));
}

fn print_status(session: &Session) -> Result<()> {
    let turns = session.turns()?;
    println!("Session {}  · {} turn(s)", session.slug, turns.len());
    println!("PRD:      {}", session.prd().display());
    println!("Tickets:  {}", session.tickets().display());

    let tickets = shape::parse_tickets(&session.read_tickets());
    if tickets.is_empty() {
        println!("\nNo tickets yet.");
        return Ok(());
    }
    println!();
    for ticket in &tickets {
        let state = match ticket.not_runnable() {
            None => "ready".to_string(),
            Some(reason) => reason,
        };
        println!("  {}  {}  — {state}", ticket.id, ticket.title);
    }
    Ok(())
}

fn print_export(results: &[Exported]) {
    let mut written = 0;
    for result in results {
        match result {
            Exported::Written(path) => {
                written += 1;
                println!("wrote   {}", path.display());
            }
            Exported::Skipped { id, reason } => println!("skipped {id}: {reason}"),
        }
    }
    if written > 0 {
        println!("\nRun one with:  kage run --task-file <path>");
    } else {
        println!("\nNothing is ready yet — keep shaping until a ticket is `Status: ready`.");
    }
}

fn ticket_summary(session: &Session) -> String {
    let tickets = shape::parse_tickets(&session.read_tickets());
    let runnable = tickets
        .iter()
        .filter(|ticket| ticket.not_runnable().is_none())
        .count();
    format!("{} ticket(s), {runnable} ready", tickets.len())
}
