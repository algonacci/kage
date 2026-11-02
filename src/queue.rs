//! The ticket queue: task files run one after another, unattended.
//!
//! `kage shape export` leaves runnable tickets in `.kage/tickets/`; this is what turns a folder of
//! them into a night's work instead of a person typing `kage run --task-file` after each finishes.
//! The queue never judges a ticket — every one goes through the ordinary run with its plan, gate,
//! and review. What it adds is order, a record of which run worked which ticket, and stacking.
//!
//! Stacking is the reason this is more than a shell loop. Every run branches from the base commit,
//! so `t2` of a session would never see what `t1` built — and the tickets were split precisely so
//! that later ones build on earlier ones. A ticket therefore branches from its predecessor's run
//! branch when that run completed, so the session's branches form one chain the owner merges once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Project;
use crate::state::{Phase, RunState};

/// Which run worked which ticket, kept beside the tickets themselves.
///
/// Keyed by file name, each value the runs in the order they were started. Run state already holds
/// how each one ended, so the ledger records only the link — a second copy of the outcome would
/// go stale the first time someone ran `kage resume` by hand.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub tickets: BTreeMap<String, Vec<String>>,
}

pub fn ledger_path(project: &Project) -> PathBuf {
    crate::shape::tickets_dir(project).join("queue.json")
}

impl Ledger {
    pub fn load(project: &Project) -> Result<Self> {
        let path = ledger_path(project);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("corrupt queue ledger at {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Written through a temp file and a rename, like run state: a ledger cut short by a crash
    /// would forget which run holds a ticket, and the next queue would start it again from zero.
    pub fn save(&self, project: &Project) -> Result<()> {
        let path = ledger_path(project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let temp = path.with_extension("json.tmp");
        let encoded = serde_json::to_vec_pretty(self).context("cannot encode the queue ledger")?;
        std::fs::write(&temp, encoded)
            .with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("cannot replace {}", path.display()))
    }

    pub fn record(&mut self, ticket: &str, run_id: &str) {
        self.tickets
            .entry(ticket.to_string())
            .or_default()
            .push(run_id.to_string());
    }

    pub fn latest(&self, ticket: &str) -> Option<&str> {
        self.tickets
            .get(ticket)
            .and_then(|runs| runs.last())
            .map(String::as_str)
    }
}

/// One task file in the queue, with the session it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTicket {
    pub path: PathBuf,
    /// The file name — the ledger key, and what the person reads.
    pub name: String,
    /// The shaping session it came from: `<slug>` in `<slug>-t<n>.md`. A hand-written file that
    /// does not follow the pattern is its own group of one.
    pub group: String,
    pub number: u32,
}

/// Every task file in the tickets folder, in the order they should run.
///
/// Sorted by session, then by ticket number as a number: by name alone `t10` runs before `t2`, and
/// stacking would hand `t10` a branch that lacks the work it was written against.
pub fn discover(dir: &Path) -> Result<Vec<QueuedTicket>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", dir.display())),
    };

    let mut tickets = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot read {}", dir.display()))?
            .path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let name = name.to_string();
        let stem = name.trim_end_matches(".md");
        let (group, number) = split_name(stem);
        tickets.push(QueuedTicket {
            path,
            name,
            group,
            number,
        });
    }

    tickets.sort_by(|a, b| (&a.group, a.number, &a.name).cmp(&(&b.group, b.number, &b.name)));
    Ok(tickets)
}

/// `hris-cuti-t2` → (`hris-cuti`, 2). Anything else is a group of its own, numbered 0.
fn split_name(stem: &str) -> (String, u32) {
    if let Some((group, ticket)) = stem.rsplit_once("-t")
        && !group.is_empty()
        && let Ok(number) = ticket.parse::<u32>()
    {
        return (group.to_string(), number);
    }
    (stem.to_string(), 0)
}

/// Where a ticket stands, read from the run that last worked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// No run has touched it, or the run it pointed at is gone from disk.
    Pending,
    /// Its latest run completed. `branch` is where the work landed, when the run was isolated.
    Done {
        run_id: String,
        branch: Option<String>,
    },
    /// Its latest run stopped short. The queue halts here rather than starting a second run over
    /// the first: that run's worktree holds the attempt, and `kage resume` continues it.
    Unfinished { run_id: String, phase: Phase },
}

pub fn standing(
    ledger: &Ledger,
    ticket: &str,
    load: impl Fn(&str) -> Option<RunState>,
) -> Standing {
    let Some(run_id) = ledger.latest(ticket) else {
        return Standing::Pending;
    };
    match load(run_id) {
        None => Standing::Pending,
        Some(state) if state.phase == Phase::Completed => Standing::Done {
            run_id: run_id.to_string(),
            branch: state.worktree.map(|worktree| worktree.branch),
        },
        Some(state) => Standing::Unfinished {
            run_id: run_id.to_string(),
            phase: state.phase,
        },
    }
}

/// The branch a ticket should start from: its nearest earlier sibling's, when that run completed
/// on a branch of its own. `None` means the configured base.
///
/// Only the same session stacks. Two sessions in one folder are unrelated work, and chaining them
/// would make one feature's branch carry another's half-finished commits.
pub fn stack_base(
    tickets: &[QueuedTicket],
    index: usize,
    standing_of: impl Fn(&QueuedTicket) -> Standing,
) -> Option<String> {
    let ticket = &tickets[index];
    let predecessor = tickets[..index]
        .iter()
        .rev()
        .find(|earlier| earlier.group == ticket.group && earlier.number < ticket.number)?;
    match standing_of(predecessor) {
        Standing::Done { branch, .. } => branch,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::run::Worktree;

    fn names(tickets: &[QueuedTicket]) -> Vec<&str> {
        tickets.iter().map(|ticket| ticket.name.as_str()).collect()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kage-queue-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(id: &str, phase: Phase, branch: Option<&str>) -> RunState {
        let mut state = RunState::new(id.to_string(), "task".to_string(), PathBuf::from("."), 3);
        state.phase = phase;
        state.worktree = branch.map(|branch| Worktree {
            path: PathBuf::from("."),
            branch: branch.to_string(),
        });
        state
    }

    #[test]
    fn tickets_run_by_session_then_by_number_not_by_name() {
        let dir = scratch("order");
        for name in [
            "b-t10.md",
            "b-t2.md",
            "a-t1.md",
            "notes.txt",
            "standalone.md",
            "b-t1.md",
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }

        let tickets = discover(&dir).unwrap();

        assert_eq!(
            names(&tickets),
            ["a-t1.md", "b-t1.md", "b-t2.md", "b-t10.md", "standalone.md"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_tickets_folder_is_an_empty_queue() {
        let dir = std::env::temp_dir().join("kage-queue-does-not-exist");
        assert!(discover(&dir).unwrap().is_empty());
    }

    #[test]
    fn a_slug_containing_a_t_is_still_split_at_the_ticket_number() {
        assert_eq!(
            split_name("cuti-tahunan-t3"),
            ("cuti-tahunan".to_string(), 3)
        );
        assert_eq!(split_name("tasks"), ("tasks".to_string(), 0));
        assert_eq!(split_name("-t1"), ("-t1".to_string(), 0));
    }

    #[test]
    fn a_ticket_stands_where_its_latest_run_ended() {
        let mut ledger = Ledger::default();
        assert_eq!(standing(&ledger, "a-t1.md", |_| None), Standing::Pending);

        ledger.record("a-t1.md", "run_1");
        ledger.record("a-t1.md", "run_2");
        let load = |id: &str| match id {
            "run_2" => Some(state("run_2", Phase::Failed, Some("kage/run_2"))),
            _ => Some(state(id, Phase::Completed, None)),
        };
        assert_eq!(
            standing(&ledger, "a-t1.md", load),
            Standing::Unfinished {
                run_id: "run_2".to_string(),
                phase: Phase::Failed
            }
        );

        let completed = |id: &str| Some(state(id, Phase::Completed, Some("kage/run_2")));
        assert_eq!(
            standing(&ledger, "a-t1.md", completed),
            Standing::Done {
                run_id: "run_2".to_string(),
                branch: Some("kage/run_2".to_string())
            }
        );
    }

    #[test]
    fn a_ticket_whose_run_vanished_from_disk_is_pending_again() {
        let mut ledger = Ledger::default();
        ledger.record("a-t1.md", "run_gone");
        assert_eq!(standing(&ledger, "a-t1.md", |_| None), Standing::Pending);
    }

    fn ticket(group: &str, number: u32) -> QueuedTicket {
        let name = format!("{group}-t{number}.md");
        QueuedTicket {
            path: PathBuf::from(&name),
            name,
            group: group.to_string(),
            number,
        }
    }

    #[test]
    fn a_ticket_stacks_on_its_completed_predecessor_in_the_same_session() {
        let tickets = [ticket("a", 1), ticket("a", 2), ticket("b", 1)];
        let standing_of = |queued: &QueuedTicket| match queued.name.as_str() {
            "a-t1.md" => Standing::Done {
                run_id: "run_1".to_string(),
                branch: Some("kage/run_1".to_string()),
            },
            _ => Standing::Pending,
        };

        assert_eq!(stack_base(&tickets, 0, standing_of), None);
        assert_eq!(
            stack_base(&tickets, 1, standing_of).as_deref(),
            Some("kage/run_1")
        );
        // Another session's ticket never inherits a branch it was not written against.
        assert_eq!(stack_base(&tickets, 2, standing_of), None);
    }

    #[test]
    fn a_predecessor_that_did_not_complete_gives_no_base_to_stack_on() {
        let tickets = [ticket("a", 1), ticket("a", 2)];
        let unfinished = |_: &QueuedTicket| Standing::Unfinished {
            run_id: "run_1".to_string(),
            phase: Phase::Blocked,
        };
        assert_eq!(stack_base(&tickets, 1, unfinished), None);
    }

    #[test]
    fn the_ledger_survives_a_round_trip_and_keeps_every_attempt() {
        let mut ledger = Ledger::default();
        ledger.record("a-t1.md", "run_1");
        ledger.record("a-t1.md", "run_2");

        let encoded = serde_json::to_string(&ledger).unwrap();
        let decoded: Ledger = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.tickets["a-t1.md"], ["run_1", "run_2"]);
        assert_eq!(decoded.latest("a-t1.md"), Some("run_2"));
    }
}
