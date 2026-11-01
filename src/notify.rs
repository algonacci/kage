//! Telling someone a run stopped.
//!
//! A run worth leaving alone is a run nobody is watching, and the closing summary it prints lands
//! in a terminal no one is looking at. One POST at the end turns "check back every so often" into
//! "be told". The message is plain text a human reads on a phone; the structured fields beside it
//! are for a receiver that wants to route or filter.
//!
//! A failed notification never touches the run's outcome or exit code. The run is already over and
//! its state is on disk; a webhook that is down is worth a warning, not a red exit on a green run.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Notify, NotifyEvent};
use crate::state::{Phase, RunState};

/// Comfortably under the 4096 characters Telegram accepts, the tightest receiver in common use.
/// A message cut short still says which run and what happened; one rejected outright says nothing.
const MAX_MESSAGE_CHARS: usize = 3500;

/// The longest error excerpt a message carries. A failed run's error can quote a whole validation
/// log, and the full text is one `kage status` away.
const MAX_ERROR_CHARS: usize = 1500;

/// The event a terminal phase announces. Non-terminal phases announce nothing: the run is still
/// going, and a message would only teach the reader to ignore them.
pub fn event_for(phase: Phase) -> Option<NotifyEvent> {
    match phase {
        Phase::Completed => Some(NotifyEvent::Completed),
        Phase::Failed => Some(NotifyEvent::Failed),
        Phase::Blocked => Some(NotifyEvent::Blocked),
        _ => None,
    }
}

/// The human-readable message.
///
/// The project is named because one machine drives runs in several repositories, and a run id alone
/// does not say which. `deferred` is relayed on completion because a planner that split the task
/// left work nobody has started yet — the one thing a finished run still wants from its owner.
pub fn message(project: &str, state: &RunState, deferred: Option<&str>) -> String {
    let mut lines = Vec::new();

    let headline = match (state.phase, state.resume_from) {
        (Phase::Failed | Phase::Blocked, Some(during)) => {
            format!(
                "kage · {project} · {} {} (during {during})",
                state.id, state.phase
            )
        }
        _ => format!("kage · {project} · {} {}", state.id, state.phase),
    };
    lines.push(headline);
    lines.push(format!("Task: {}", truncate(first_line(&state.task), 200)));

    if let Some(error) = &state.error {
        lines.push(String::new());
        lines.push(truncate(error.trim(), MAX_ERROR_CHARS));
    }

    if let Some(commit) = &state.commit {
        lines.push(String::new());
        lines.push(commit.describe());
    }

    if state.phase == Phase::Completed
        && let Some(deferred) = deferred
    {
        lines.push(String::new());
        lines.push("Deferred by the planner — each is a later `kage run`:".to_string());
        lines.push(deferred.trim().to_string());
    }

    lines.push(String::new());
    lines.push(format!("Details: kage status {}", state.id));

    truncate(&lines.join("\n"), MAX_MESSAGE_CHARS)
}

/// The message for a run that stopped on an error instead of reaching a terminal phase.
///
/// A harness that cannot be spawned, or a git call that dies mid-phase, surfaces as an `Err` with no
/// final state behind it. Left unannounced, exactly the run nobody is watching ends in silence.
pub fn error_message(project: &str, run_id: Option<&str>, error: &anyhow::Error) -> String {
    let headline = match run_id {
        Some(id) => format!("kage · {project} · {id} stopped on an error"),
        None => format!("kage · {project} · run stopped on an error before it started"),
    };
    let mut lines = vec![
        headline,
        String::new(),
        truncate(&format!("{error:#}"), MAX_ERROR_CHARS),
    ];
    if let Some(id) = run_id {
        lines.push(String::new());
        lines.push(format!("Details: kage status {id}"));
    }

    truncate(&lines.join("\n"), MAX_MESSAGE_CHARS)
}

/// What a body says about the run, separate from the message so an error with no final state can
/// fill it too.
pub struct RunFacts<'a> {
    pub run_id: Option<&'a str>,
    pub phase: Option<&'a str>,
    pub branch: Option<&'a str>,
}

impl<'a> RunFacts<'a> {
    pub fn of(state: &'a RunState) -> Self {
        Self {
            run_id: Some(&state.id),
            phase: Some(state.phase.as_str()),
            branch: state
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
        }
    }
}

/// The JSON body. `text` is what a chat relay forwards; the rest lets a receiver act on the event
/// without parsing prose.
pub fn body(
    notify: &Notify,
    event: NotifyEvent,
    project: &str,
    facts: &RunFacts,
    text: &str,
) -> Value {
    let mut body = serde_json::Map::new();
    for (key, value) in &notify.fields {
        body.insert(key.clone(), value.clone());
    }

    let ours = json!({
        "text": text,
        "event": event,
        "project": project,
        "run_id": facts.run_id,
        "phase": facts.phase,
        "branch": facts.branch,
    });
    if let Value::Object(ours) = ours {
        body.extend(ours);
    }

    Value::Object(body)
}

/// POST one body, bounded by the configured timeout end to end.
///
/// The secret is passed in rather than read here so tests never touch the process environment.
pub async fn post(notify: &Notify, secret: Option<&str>, body: &Value) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(notify.timeout_secs))
        .build()
        .context("cannot build an HTTP client")?;

    let mut request = client.post(notify.url.trim()).json(body);
    if let (Some(header), Some(secret)) = (&notify.secret_header, secret) {
        request = request.header(header.as_str(), secret);
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("could not reach {}", notify.url))?;

    let status = response.status();
    if !status.is_success() {
        // The body is the receiver's own explanation and the fastest route to a fix; it is capped
        // because a misrouted URL can answer with an entire HTML page.
        let detail = response.text().await.unwrap_or_default();
        bail!(
            "{} answered {status}: {}",
            notify.url,
            truncate(detail.trim(), 300)
        );
    }

    Ok(())
}

/// Announce a finished run, if the config asks for it. Prints what happened and never fails.
pub async fn announce(
    notify: Option<&Notify>,
    project: &str,
    state: &RunState,
    deferred: Option<&str>,
) {
    let Some(event) = event_for(state.phase) else {
        return;
    };
    let text = message(project, state, deferred);
    deliver(notify, event, project, &RunFacts::of(state), &text).await;
}

/// Announce a run that ended on an error. Counted as `failed`: to the person reading it on a phone
/// the difference is in the text, not in whether they wanted to hear about it.
pub async fn announce_error(
    notify: Option<&Notify>,
    project: &str,
    run_id: Option<&str>,
    error: &anyhow::Error,
) {
    let text = error_message(project, run_id, error);
    let facts = RunFacts {
        run_id,
        phase: None,
        branch: None,
    };
    deliver(notify, NotifyEvent::Failed, project, &facts, &text).await;
}

async fn deliver(
    notify: Option<&Notify>,
    event: NotifyEvent,
    project: &str,
    facts: &RunFacts<'_>,
    text: &str,
) {
    let Some(notify) = notify else {
        return;
    };
    if !notify.on.contains(&event) {
        return;
    }

    let secret = match &notify.secret_env {
        None => None,
        Some(name) => match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Some(value),
            // Sending anyway would only earn a 401 and hide the real cause behind it.
            _ => {
                println!("notify: skipped — ${name} is not set, so the webhook would refuse it");
                return;
            }
        },
    };

    let body = body(notify, event, project, facts, text);

    match post(notify, secret.as_deref(), &body).await {
        Ok(()) => println!("notify: sent ({event:?})"),
        Err(error) => println!("notify: failed — {error:#}"),
    }
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// Cut on a character boundary — a byte slice through a multi-byte character panics, and task
/// text here is routinely Indonesian with an emoji or two.
fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::state::Commitment;

    fn state(phase: Phase) -> RunState {
        let mut state = RunState::new(
            "run_20261101_001".to_string(),
            "Add a webhook\nwith a long second line".to_string(),
            PathBuf::from("."),
            3,
        );
        state.phase = phase;
        state
    }

    fn notify(url: &str) -> Notify {
        serde_yaml_ng::from_str(&format!("url: '{url}'")).unwrap()
    }

    #[test]
    fn only_a_terminal_phase_announces_anything() {
        assert_eq!(event_for(Phase::Completed), Some(NotifyEvent::Completed));
        assert_eq!(event_for(Phase::Failed), Some(NotifyEvent::Failed));
        assert_eq!(event_for(Phase::Blocked), Some(NotifyEvent::Blocked));
        for phase in [
            Phase::Created,
            Phase::Planning,
            Phase::Executing,
            Phase::Reviewing,
        ] {
            assert_eq!(event_for(phase), None, "{phase} is not an ending");
        }
    }

    #[test]
    fn a_failure_message_names_the_project_the_phase_and_the_cause() {
        let mut state = state(Phase::Failed);
        state.resume_from = Some(Phase::Testing);
        state.error = Some("validation failed: cargo test exited 101".to_string());

        let text = message("kage", &state, None);

        assert!(
            text.starts_with("kage · kage · run_20261101_001 failed (during testing)"),
            "{text}"
        );
        assert!(
            text.contains("Task: Add a webhook\n"),
            "only the first line of the task: {text}"
        );
        assert!(!text.contains("long second line"), "{text}");
        assert!(text.contains("cargo test exited 101"), "{text}");
        assert!(
            text.ends_with("Details: kage status run_20261101_001"),
            "{text}"
        );
    }

    #[test]
    fn a_completed_run_relays_the_planners_deferred_work_and_where_the_commit_went() {
        let mut state = state(Phase::Completed);
        state.commit = Some(Commitment::Committed {
            sha: "abcdef1234567890".to_string(),
            branch: "kage/run_20261101_001".to_string(),
            files_changed: 3,
            created: true,
        });

        let text = message("kage", &state, Some("- add queue\n- add Linear sync"));

        assert!(text.contains("kage/run_20261101_001"), "{text}");
        assert!(text.contains("- add Linear sync"), "{text}");
    }

    #[test]
    fn deferred_work_is_not_relayed_for_a_run_that_did_not_finish() {
        // A failed run's deferred list describes work after a piece that never landed; listing it
        // as "start these next" would send the reader to build on nothing.
        let text = message("kage", &state(Phase::Failed), Some("- add queue"));

        assert!(!text.contains("add queue"), "{text}");
    }

    #[test]
    fn a_huge_error_is_cut_to_fit_a_chat_message_without_splitting_a_character() {
        let mut state = state(Phase::Failed);
        state.error = Some("gagal 🚧 ".repeat(2000));

        let text = message("kage", &state, None);

        assert!(
            text.chars().count() <= MAX_MESSAGE_CHARS + 1,
            "{}",
            text.chars().count()
        );
        assert!(text.contains('…'));
    }

    #[test]
    fn a_run_that_died_on_an_error_still_says_which_run_and_why() {
        let error = anyhow::anyhow!("`opencode` not found").context("executor could not start");

        let text = error_message("kage", Some("run_20261101_001"), &error);
        assert!(
            text.contains("run_20261101_001 stopped on an error"),
            "{text}"
        );
        assert!(
            text.contains("executor could not start: `opencode` not found"),
            "{text}"
        );
        assert!(
            text.ends_with("Details: kage status run_20261101_001"),
            "{text}"
        );

        // Before an id exists there is no run to point `kage status` at.
        let text = error_message("kage", None, &error);
        assert!(text.contains("before it started"), "{text}");
        assert!(!text.contains("kage status"), "{text}");
    }

    #[test]
    fn kages_own_keys_win_over_configured_fields() {
        let mut notify = notify("https://example.test/notify");
        notify.fields = BTreeMap::from([
            ("chat_id".to_string(), json!("12345")),
            ("parse_mode".to_string(), Value::Null),
            ("text".to_string(), json!("stray")),
        ]);
        let state = state(Phase::Blocked);

        let body = body(
            &notify,
            NotifyEvent::Blocked,
            "kage",
            &RunFacts::of(&state),
            "the message",
        );

        assert_eq!(
            body["text"], "the message",
            "a configured field must not replace the message"
        );
        assert_eq!(body["chat_id"], "12345");
        assert!(
            body.get("parse_mode").is_some_and(Value::is_null),
            "an explicit null is kept"
        );
        assert_eq!(body["event"], "blocked");
        assert_eq!(body["run_id"], "run_20261101_001");
        assert_eq!(body["project"], "kage");
    }

    /// Accept one request, answer with `status`, and hand back the raw request text.
    async fn one_shot_server(status: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/notify", listener.local_addr().unwrap());

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buffer = [0u8; 4096];
            // Read until the declared body has fully arrived, so the assertion sees all of it.
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if raw.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            let reply =
                format!("HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno");
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&raw).into_owned()
        });

        (url, handle)
    }

    #[tokio::test]
    async fn the_secret_travels_in_the_configured_header_with_the_json_body() {
        let (url, server) = one_shot_server("200 OK").await;
        let mut notify = notify(&url);
        notify.secret_env = Some("UNUSED_IN_THIS_TEST".to_string());
        notify.secret_header = Some("X-Notify-Secret".to_string());

        post(&notify, Some("s3cret"), &json!({"text": "hello"}))
            .await
            .unwrap();

        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /notify"), "{request}");
        assert!(request.contains("x-notify-secret: s3cret"), "{request}");
        assert!(
            request.contains("content-type: application/json"),
            "{request}"
        );
        assert!(request.contains(r#"{"text":"hello"}"#), "{request}");
    }

    #[tokio::test]
    async fn a_refusing_receiver_is_an_error_that_quotes_its_answer() {
        let (url, server) = one_shot_server("401 Unauthorized").await;

        let error = post(&notify(&url), None, &json!({"text": "hello"}))
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();

        assert!(error.contains("401"), "{error}");
        assert!(
            error.contains("no"),
            "the receiver's own reason is quoted: {error}"
        );
    }

    #[tokio::test]
    async fn a_receiver_that_never_answers_is_abandoned_at_the_timeout() {
        // Accepts the connection and then says nothing, the way a wedged proxy does. Without the
        // client timeout this would hold the finished run's exit hostage indefinitely.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/notify", listener.local_addr().unwrap());
        let _server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let mut notify = notify(&url);
        notify.timeout_secs = 1;

        let started = std::time::Instant::now();
        let result = post(&notify, None, &json!({"text": "hello"})).await;

        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }
}
