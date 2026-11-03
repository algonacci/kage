//! Recognising a harness that stopped because its plan's usage limit ran out.
//!
//! A subscription harness refuses work for hours once its window is spent, and says until when:
//! claude prints `You've hit your 5-hour limit · resets 1:10am (Asia/Jakarta)`. To a run that
//! looks like any other non-zero exit, so an overnight queue died at 16:46 and sat idle until a
//! person woke up and typed `kage resume` — when all it needed was to wait. Reading the reset time
//! is a string match on the harness's own words, which is exactly the kind of thing that drifts,
//! so anything unrecognised falls through to the ordinary failure rather than to a guess.

use chrono::{Duration, NaiveDateTime, NaiveTime};

/// How much of each stream is searched. The limit notice is the last thing a harness says; a
/// failing run whose transcript merely *contains* the words (a planner reading rate-limit code)
/// must not be mistaken for one.
const TAIL_CHARS: usize = 2000;

/// Grace after the stated reset, because the clock it was stated in rounds to the minute. None in
/// tests, so a retry can be exercised without the suite sleeping.
const AFTER_RESET: Duration = Duration::seconds(if cfg!(test) { 0 } else { 90 });

/// When the harness says its limit lifts, given the output of a run that failed and the local
/// time now. `None` when the output is not a limit notice or names no time Kage can read.
///
/// The stated time is taken as local: claude names the machine's own zone, and a harness on the
/// Mac mini that runs the queue reports the Mac mini's clock.
pub fn reset_after(stdout: &str, stderr: &str, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let text = format!("{}\n{}", tail(stdout), tail(stderr)).to_lowercase();
    if !text.contains("limit") {
        return None;
    }
    let (_, after) = ["resets", "try again"]
        .iter()
        .filter_map(|word| text.rfind(word).map(|at| (at, &text[at + word.len()..])))
        .max_by_key(|(at, _)| *at)?;
    let after = after.trim_start();

    if let Some(span) = after.strip_prefix("in ") {
        return Some(now + duration(span)? + AFTER_RESET);
    }
    let clock = after.strip_prefix("at ").unwrap_or(after);
    let time = clock_time(clock)?;
    let today = now.date().and_time(time);
    let reset = if today > now {
        today
    } else {
        today + Duration::days(1)
    };
    Some(reset + AFTER_RESET)
}

fn tail(text: &str) -> &str {
    let start = text.len().saturating_sub(TAIL_CHARS);
    let start = (start..=text.len())
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(text.len());
    &text[start..]
}

/// `1:10am`, `1am`, `1:10 pm`, `13:10`.
fn clock_time(text: &str) -> Option<NaiveTime> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(text.len());
    let (digits, rest) = text.split_at(end);
    let (hour, minute) = match digits.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (digits.parse::<u32>().ok()?, 0),
    };
    let rest = rest.trim_start();
    let hour = if rest.starts_with("am") || rest.starts_with("a.m") {
        if hour == 12 { 0 } else { hour }
    } else if rest.starts_with("pm") || rest.starts_with("p.m") {
        if hour == 12 { 12 } else { hour + 12 }
    } else if digits.contains(':') {
        hour
    } else {
        // A bare number with no clock or meridiem — "resets 5" — names nothing reliably.
        return None;
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// `2 hours 3 minutes`, `2h 3m`, `45 minutes`.
fn duration(text: &str) -> Option<Duration> {
    let mut total = Duration::zero();
    let mut found = false;
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let digits: String = word.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(amount) = digits.parse::<i64>() else {
            break;
        };
        let unit = match &word[digits.len()..] {
            "" => words.next().unwrap_or(""),
            unit => unit,
        };
        let unit = unit.trim_end_matches(['.', ',']);
        total += match unit {
            "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(amount),
            "m" | "min" | "mins" | "minute" | "minutes" => Duration::minutes(amount),
            _ => break,
        };
        found = true;
        if words.peek() == Some(&"and") {
            words.next();
        }
    }
    found.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn a_claude_limit_notice_resets_at_the_next_occurrence_of_its_clock_time() {
        // The overnight queue that died at 16:46 on the Omniflow-CRM tickets printed exactly this.
        let notice =
            "Writing the plan now.\nYou've hit your 5-hour limit · resets 1:10am (Asia/Jakarta)";

        let reset = reset_after(notice, "", at(16, 46)).unwrap();

        assert_eq!(reset, at(1, 10) + Duration::days(1) + AFTER_RESET);
        assert_eq!(
            reset_after(notice, "", at(0, 30)).unwrap(),
            at(1, 10) + AFTER_RESET,
            "a reset later today is not pushed to tomorrow"
        );
    }

    #[test]
    fn clock_times_and_spans_in_the_shapes_harnesses_print_are_read() {
        let now = at(10, 0);
        let read = |text: &str| reset_after(text, "", now).map(|reset| reset - AFTER_RESET);

        assert_eq!(read("usage limit reached, resets 3pm"), Some(at(15, 0)));
        assert_eq!(
            read("Usage limit. Try again at 12:30 PM."),
            Some(at(12, 30))
        );
        assert_eq!(read("rate limit; resets at 14:05"), Some(at(14, 5)));
        assert_eq!(
            read("You've hit your usage limit. Try again in 2 hours 3 minutes."),
            Some(at(12, 3))
        );
        assert_eq!(read("limit hit, resets in 45m"), Some(at(10, 45)));
    }

    #[test]
    fn a_failure_that_is_not_a_limit_with_a_time_is_left_to_fail() {
        let now = at(10, 0);

        assert_eq!(reset_after("error: model not found", "", now), None);
        // opencode's free tier names no reset: waiting would only burn the night.
        assert_eq!(
            reset_after(
                "Error: Free usage limit reached. subscribe to keep going",
                "",
                now
            ),
            None
        );
        assert_eq!(
            reset_after("limit resets 5", "", now),
            None,
            "a bare number is no time"
        );
        let quoted = format!("the limiter resets at 3pm\n{}", "x".repeat(TAIL_CHARS));
        assert_eq!(
            reset_after(&quoted, "", now),
            None,
            "only the tail is a notice"
        );
    }
}
