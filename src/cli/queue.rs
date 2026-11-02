//! `kage queue` — run the exported tickets one after another.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{Config, NotifyEvent, Project};
use crate::engine::workflow::Options;
use crate::finish::{self, HandOver};
use crate::notify;
use crate::queue::{self, Ledger, QueuedTicket, Standing};
use crate::shape;
use crate::state::{Phase, RunState, store};

/// Per-invocation choices for the queue.
#[derive(Debug, Default)]
pub struct QueueOptions {
    /// Show where every ticket stands and run nothing.
    pub list: bool,
    /// Where a ticket with nothing to stack on branches from, instead of `git.base`.
    pub base: Option<String>,
    pub skip_gate_check: bool,
}

pub async fn run(cwd: &Path, options: QueueOptions) -> Result<()> {
    let project = Project::discover(cwd)?;
    let config = project.load_config()?;

    let dir = shape::tickets_dir(&project);
    let tickets = queue::discover(&dir)?;
    if tickets.is_empty() {
        println!(
            "No tickets in {}.\nExport some with:  kage shape export <slug>",
            dir.display()
        );
        return Ok(());
    }

    let mut ledger = Ledger::load(&project)?;
    let load = |id: &str| store::load(&project, id).ok();

    if options.list {
        for ticket in &tickets {
            println!(
                "{}",
                describe(ticket, &queue::standing(&ledger, &ticket.name, load))
            );
        }
        return Ok(());
    }

    let mut ran = 0usize;
    for index in 0..tickets.len() {
        let ticket = &tickets[index];
        match queue::standing(&ledger, &ticket.name, load) {
            Standing::Done { .. } => continue,
            Standing::Unfinished { run_id, phase } => {
                return stop(
                    &project, &config, &tickets, &ledger, ran, ticket, &run_id, phase,
                )
                .await;
            }
            Standing::Pending => {}
        }

        let base = queue::stack_base(&tickets, index, |earlier| {
            queue::standing(&ledger, &earlier.name, load)
        })
        .or_else(|| options.base.clone());
        let task = super::resolve_task(None, Some(ticket.path.clone()))?;

        println!("\n{}", "=".repeat(60));
        println!(
            "Queue {}/{}: {}{}",
            index + 1,
            tickets.len(),
            ticket.name,
            base.as_deref()
                .map(|base| format!("  (on {base})"))
                .unwrap_or_default()
        );

        let before = store::list_run_ids(&project).unwrap_or_default();
        let run_options = Options {
            base,
            skip_gate_check: options.skip_gate_check,
            ..Options::default()
        };

        match super::start(&project, &config, &task, &run_options).await {
            Ok(state) => {
                ledger.record(&ticket.name, &state.id);
                ledger.save(&project)?;
                let hand_over = match hand_over_for(&project, &state) {
                    Place::Tip(hand_over) => hand_over,
                    Place::NotQueued | Place::Stacked => None,
                };
                super::conclude(&project, &config, &state, hand_over.as_ref()).await;
                if state.phase != Phase::Completed {
                    return stop(
                        &project,
                        &config,
                        &tickets,
                        &ledger,
                        ran,
                        ticket,
                        &state.id,
                        state.phase,
                    )
                    .await;
                }
                ran += 1;
            }
            Err(error) => {
                // A run that died on an error returns no state, but it may already own a run
                // directory and a worktree. Linking it keeps the next queue from starting the
                // ticket over beside it; the queue is the only thing starting runs here, so one
                // new id is this ticket's.
                let after = store::list_run_ids(&project).unwrap_or_default();
                let fresh: Vec<_> = after.iter().filter(|id| !before.contains(id)).collect();
                if let [run_id] = fresh.as_slice() {
                    ledger.record(&ticket.name, run_id);
                    ledger.save(&project)?;
                }
                let text = format!(
                    "{}\nstopped at {} on an error: {error:#}",
                    headline(&project, ran),
                    ticket.name
                );
                notify::announce_queue(
                    config.notify.as_ref(),
                    &super::project_name(&project),
                    NotifyEvent::Failed,
                    &text,
                )
                .await;
                return Err(error.context(format!("the queue stopped at {}", ticket.name)));
            }
        }
    }

    let mut text = format!("{}\nevery ticket is done", headline(&project, ran));
    let merge = merge_lines(&tickets, &ledger, load);
    if !merge.is_empty() {
        text.push_str("\n\n");
        text.push_str(&merge);
    }
    println!("\n{}\n{text}", "=".repeat(60));
    if ran > 0 {
        notify::announce_queue(
            config.notify.as_ref(),
            &super::project_name(&project),
            NotifyEvent::Completed,
            &text,
        )
        .await;
    }
    Ok(())
}

/// Halt at a ticket whose run did not complete.
///
/// Later tickets are not started: in the same session they were written against this one's work,
/// and running another session's while this one waits would leave the owner two half-finished
/// features to untangle instead of one stopped run to resume.
#[allow(clippy::too_many_arguments)]
async fn stop(
    project: &Project,
    config: &Config,
    tickets: &[QueuedTicket],
    ledger: &Ledger,
    ran: usize,
    ticket: &QueuedTicket,
    run_id: &str,
    phase: Phase,
) -> Result<()> {
    let load = |id: &str| store::load(project, id).ok();
    let waiting = tickets
        .iter()
        .filter(|other| queue::standing(ledger, &other.name, load) == Standing::Pending)
        .count();
    let text = format!(
        "{}\nstopped at {}: run {run_id} is {phase}, {waiting} ticket(s) not started\n\
         continue with:  kage resume {run_id}   then   kage queue",
        headline(project, ran),
        ticket.name
    );
    println!("\n{}\n{text}", "=".repeat(60));

    let event = match phase {
        Phase::Blocked => NotifyEvent::Blocked,
        _ => NotifyEvent::Failed,
    };
    notify::announce_queue(
        config.notify.as_ref(),
        &super::project_name(project),
        event,
        &text,
    )
    .await;

    bail!("the queue stopped at {}", ticket.name)
}

/// Where a finished run sits in the queue, which decides whether its branch is handed over.
pub(crate) enum Place {
    /// Not a queued ticket's run: hand it over on its own.
    NotQueued,
    /// A later ticket in its session stacks on this branch, so handing it over now would open a
    /// pull request the next ticket immediately outdates.
    Stacked,
    /// The session's last ticket: its branch holds the whole session. `None` when there is
    /// nothing committed to hand over.
    Tip(Option<HandOver>),
}

/// Find the run's ticket through the ledger and decide its [`Place`].
///
/// Shared by the queue and `kage resume`, because the run that completes a session is as often a
/// stopped ticket resumed by hand as one the queue finished.
pub(crate) fn hand_over_for(project: &Project, state: &RunState) -> Place {
    let (Ok(ledger), Ok(tickets)) = (
        Ledger::load(project),
        queue::discover(&shape::tickets_dir(project)),
    ) else {
        return Place::NotQueued;
    };
    let Some(ticket) = tickets
        .iter()
        .find(|ticket| ledger.latest(&ticket.name) == Some(state.id.as_str()))
    else {
        return Place::NotQueued;
    };

    let siblings: Vec<&QueuedTicket> = tickets
        .iter()
        .filter(|other| other.group == ticket.group)
        .collect();
    if siblings.iter().any(|other| other.number > ticket.number) {
        return Place::Stacked;
    }

    let load = |id: &str| store::load(project, id).ok();
    let done: Vec<(String, RunState)> = siblings
        .iter()
        .filter_map(
            |sibling| match queue::standing(&ledger, &sibling.name, load) {
                Standing::Done { run_id, .. } => {
                    load(&run_id).map(|state| (finish::title_of(&ticket_title(sibling)), state))
                }
                _ => None,
            },
        )
        .collect();
    Place::Tip(finish::for_session(
        project,
        session_title(project, &ticket.group),
        &done,
    ))
}

/// The ticket's own first line — `T2: Approval flow` — read from its task file.
fn ticket_title(ticket: &QueuedTicket) -> String {
    std::fs::read_to_string(&ticket.path).unwrap_or_else(|_| ticket.name.clone())
}

/// The PRD's title when the session still exists, the session's slug otherwise.
fn session_title(project: &Project, group: &str) -> String {
    std::fs::read_to_string(shape::sessions_dir(project).join(group).join("PRD.md"))
        .ok()
        .and_then(|prd| {
            prd.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("# ")
                    .map(str::trim)
                    .map(str::to_string)
            })
        })
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| group.to_string())
}

fn headline(project: &Project, ran: usize) -> String {
    format!(
        "kage · {} · queue: {ran} ticket(s) completed this time",
        super::project_name(project)
    )
}

/// The branch to merge for each session: the last completed ticket's, which holds every ticket
/// stacked under it.
fn merge_lines(
    tickets: &[QueuedTicket],
    ledger: &Ledger,
    load: impl Fn(&str) -> Option<RunState> + Copy,
) -> String {
    let mut tips: BTreeMap<&str, String> = BTreeMap::new();
    for ticket in tickets {
        if let Standing::Done {
            branch: Some(branch),
            ..
        } = queue::standing(ledger, &ticket.name, load)
        {
            tips.insert(&ticket.group, branch);
        }
    }
    tips.iter()
        .map(|(group, branch)| format!("merge {group}:  {branch}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe(ticket: &QueuedTicket, standing: &Standing) -> String {
    match standing {
        Standing::Pending => format!("pending     {}", ticket.name),
        Standing::Done { run_id, branch } => format!(
            "done        {}  ({run_id}{})",
            ticket.name,
            branch
                .as_deref()
                .map(|branch| format!(", {branch}"))
                .unwrap_or_default()
        ),
        Standing::Unfinished { run_id, phase } => format!(
            "{:<11} {}  ({run_id} — kage resume {run_id})",
            phase.as_str(),
            ticket.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Commitment;

    fn committed(project: &Project, id: &str) -> RunState {
        let mut state = RunState::new(
            id.to_string(),
            "task".to_string(),
            std::path::PathBuf::from("."),
            3,
        );
        state.phase = Phase::Completed;
        state.commit = Some(Commitment::Committed {
            sha: "abc".to_string(),
            branch: format!("kage/{id}"),
            files_changed: 1,
            created: true,
        });
        store::save(project, &state).unwrap();
        state
    }

    #[test]
    fn only_the_last_ticket_of_a_session_is_handed_over_and_it_carries_the_session() {
        // A pull request per ticket would be outdated by the next ticket stacking on its branch;
        // the session's last branch holds everything, so that is the one handed over.
        let root = std::env::temp_dir().join(format!("kage-queue-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();
        let project = Project::discover(&root).unwrap();

        let tickets = shape::tickets_dir(&project);
        std::fs::create_dir_all(&tickets).unwrap();
        std::fs::write(tickets.join("leave-t1.md"), "# T1: Table\n").unwrap();
        std::fs::write(tickets.join("leave-t2.md"), "# T2: Approval\n").unwrap();
        let session = shape::sessions_dir(&project).join("leave");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("PRD.md"), "# Leave requests\n\nbody").unwrap();

        let first = committed(&project, "run_1");
        let second = committed(&project, "run_2");
        let unrelated = committed(&project, "run_9");
        let mut ledger = Ledger::default();
        ledger.record("leave-t1.md", "run_1");
        ledger.record("leave-t2.md", "run_2");
        ledger.save(&project).unwrap();

        assert!(matches!(hand_over_for(&project, &first), Place::Stacked));
        assert!(matches!(
            hand_over_for(&project, &unrelated),
            Place::NotQueued
        ));
        let Place::Tip(Some(hand_over)) = hand_over_for(&project, &second) else {
            panic!("the session's last ticket must be handed over");
        };
        assert_eq!(hand_over.branch, "kage/run_2");
        assert_eq!(hand_over.title, "Leave requests");
        assert!(hand_over.summary.contains("## T1: Table"));
        assert!(hand_over.summary.contains("## T2: Approval"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
