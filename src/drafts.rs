//! The drafts saved from here, beside what you sent. `send --draft` and
//! `send --at` append each one to `drafts.jsonl`; `slack drafts` reads them
//! back and finds, in the same place, the message of yours that followed,
//! so the voice the drafts are written in can be checked against yours
//! (the slack skill's voice review). Reading the pairs back only reads
//! Slack.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{self, Client};
use crate::history::{compare_ts, message_from};
use crate::mrkdwn;
use crate::names::Names;
use crate::style::Style;
use crate::{emoji, par, paths, target, view};

const LOG: &str = "drafts.jsonl";
/// Deliberate: a draft saved on a Friday evening may be sent on Monday
/// morning; past three days, a message there answers something else.
const WINDOW: i64 = 3 * 24 * 3600;
/// Slack's largest page; three days of one place rarely fill it.
const PAGE: &str = "999";

/// One draft as saved, a line of the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// When it was saved, in Unix seconds.
    pub saved: i64,
    pub team: String,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    /// As it went to Slack, mentions as `<@U…>`.
    pub text: String,
    /// The id the draft was created with, which a message sent from it may keep.
    pub client_msg_id: String,
    /// When Slack is to post it, for `send --at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<i64>,
    /// The Claude Code session that wrote it: its transcript holds the revisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// The Claude Code session running this command, if any.
pub fn session() -> Option<String> {
    std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty())
}

fn path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join(LOG))
}

/// Appends the draft to the log. Best effort: the draft is in Slack
/// already, and an error here would read as "not saved", which a retry
/// would turn into a second draft.
pub fn record(record: &Record) {
    let write = || -> Result<()> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        paths::append_private(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))
    };
    if let Err(err) = write() {
        crate::warn::warn(format!(
            "the draft is saved, but not in the draft log: {err:#}"
        ));
    }
}

/// The drafts of this workspace saved since `since` (Unix seconds),
/// oldest first. No log is no draft; a line that does not read is skipped
/// with a warning.
fn load(team: &str, since: i64) -> Result<Vec<Record>> {
    let path = path()?;
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if paths::is_not_found(&err) => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut records = Vec::new();
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(&line) {
            Ok(record) if record.team == team && record.saved >= since => records.push(record),
            Ok(_) => {}
            Err(err) => crate::warn::warn(format!(
                "{} line {}: skipped, {err}",
                path.display(),
                number + 1
            )),
        }
    }
    records.sort_by_key(|record| record.saved);
    Ok(records)
}

/// What became of a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Sent as written.
    Unchanged,
    /// Sent with changes.
    Edited,
    /// No message of yours there in the window.
    NotSent,
    /// Nothing of yours there yet, and the window is still open.
    Pending,
    /// The place could not be read, or not far enough back to tell.
    Unknown,
}

/// How the message was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    /// It carries the draft's `client_msg_id`.
    Id,
    /// Your next message in that place.
    Next,
}

/// A draft and what you sent, as `slack drafts` prints it.
#[derive(Debug, Serialize)]
pub struct Pair {
    #[serde(skip)]
    pub saved_at: i64,
    /// Your clock, ISO 8601.
    pub saved: String,
    pub place: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Mentions spelled out as names, as `slack show` prints a message.
    pub draft: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_by: Option<Match>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
}

/// Where a draft's message is looked for, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    /// The save, or the time Slack is to post it for `send --at`.
    from: i64,
    /// The end, now at most.
    until: i64,
    /// The end is still to come: nothing there yet is not "not sent".
    open: bool,
}

/// A message of yours in the draft's place, after it was saved.
#[derive(Debug, Clone, PartialEq)]
struct Mine {
    ts: String,
    text: String,
    client_msg_id: Option<String>,
}

/// The drafts of the last `days` days, each with what you sent after it.
/// A session Slack refuses fails the run; a place that cannot be read is
/// `unknown`, with a warning, so one archived channel does not hide the rest.
pub fn pairs(
    client: &Client,
    names: &mut Names,
    team: &str,
    me: &str,
    url: &str,
    days: u64,
) -> Result<Vec<Pair>> {
    let now = Zoned::now().timestamp().as_second();
    let since = now - i64::try_from(days).unwrap_or(i64::MAX / 2) * 24 * 3600;
    let records = load(team, since)?;
    let windows: Vec<(&Record, Window)> = records
        .iter()
        .enumerate()
        .map(|(at, record)| (record, window(&records, at, now)))
        .collect();
    let found = par::map(windows.clone(), |(record, window)| {
        fetch_mine(client, me, record, window)
    });

    let mut found_pairs = Vec::with_capacity(records.len());
    for ((record, window), found) in windows.into_iter().zip(found) {
        let (outcome, matched) = match found {
            Ok((mine, cut)) => decide(record, &mine, cut, window.open),
            Err(err @ api::Error::Auth { .. }) => return Err(err.into()),
            Err(err) => {
                crate::warn::warn(format!("could not read {}: {err}", record.channel));
                (Outcome::Unknown, None)
            }
        };
        found_pairs.push((record, outcome, matched));
    }

    let mut people = BTreeSet::new();
    let mut places: BTreeSet<String> = records.iter().map(|r| r.channel.clone()).collect();
    for (record, _, matched) in &found_pairs {
        for text in std::iter::once(&record.text).chain(matched.as_ref().map(|(_, m)| &m.text)) {
            people.extend(view::mentioned_users(text));
            places.extend(view::mentioned_channels(text));
        }
    }
    // Best effort, as for every name: an id prints when a lookup fails.
    let _ = names.resolve_channels(client, places);
    people.extend(
        records
            .iter()
            .filter_map(|r| names.dm_user(&r.channel).map(str::to_string)),
    );
    let _ = names.resolve_users(client, people);

    let render = |text: &str| plain(text, names);
    Ok(found_pairs
        .into_iter()
        .map(|(record, outcome, matched)| {
            let (matched_by, mine) = matched.unzip();
            let label = names.channel_label(&record.channel);
            Pair {
                saved_at: record.saved,
                saved: local(record.saved),
                place: if record.thread_ts.is_some() {
                    format!("a thread in {label}")
                } else {
                    label
                },
                channel: record.channel.clone(),
                thread_ts: record.thread_ts.clone(),
                scheduled: record.scheduled.map(local),
                session: record.session.clone(),
                draft: render(&record.text),
                outcome,
                matched_by,
                sent: mine.as_ref().map(|m| render(&m.text)),
                sent_at: mine
                    .as_ref()
                    .and_then(|m| m.ts.split('.').next()?.parse().ok())
                    .map(local),
                permalink: mine.as_ref().map(|m| {
                    target::permalink(url, &record.channel, &m.ts, record.thread_ts.as_deref())
                }),
            }
        })
        .collect())
}

/// A draft's window. It opens at the save, or at the time Slack posts a
/// scheduled one, so a message written meanwhile is not taken for it. It
/// closes at the next draft saved in the same place after that, else three
/// days on, and never past now.
fn window(records: &[Record], at: usize, now: i64) -> Window {
    let record = &records[at];
    let from = record.saved.max(record.scheduled.unwrap_or(0));
    let next = records[at + 1..]
        .iter()
        .find(|other| {
            other.channel == record.channel
                && other.thread_ts == record.thread_ts
                && other.saved > from
        })
        .map(|other| other.saved);
    let end = next.unwrap_or(i64::MAX).min(from + WINDOW);
    Window {
        from,
        until: end.min(now),
        open: end > now,
    }
}

/// Your messages in the draft's place in its window, oldest first, and
/// whether the page may miss the first of them.
fn fetch_mine(
    client: &Client,
    me: &str,
    record: &Record,
    window: Window,
) -> Result<(Vec<Mine>, bool), api::Error> {
    let oldest = format!("{}.000000", window.from);
    let latest = format!("{}.999999", window.until);
    let mut params = vec![
        ("channel", record.channel.as_str()),
        ("oldest", oldest.as_str()),
        ("latest", latest.as_str()),
        ("limit", PAGE),
    ];
    let method = match &record.thread_ts {
        Some(ts) => {
            params.push(("ts", ts));
            "conversations.replies"
        }
        None => "conversations.history",
    };
    parse_mine(
        &client.call(method, &params)?,
        me,
        record.thread_ts.as_deref(),
        method,
    )
}

fn parse_mine(
    body: &Value,
    me: &str,
    thread_ts: Option<&str>,
    method: &str,
) -> Result<(Vec<Mine>, bool), api::Error> {
    let raw = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| api::Error::Shape {
            method: method.to_string(),
            reason: "no `messages` array".to_string(),
        })?;
    let mut mine: Vec<Mine> = raw
        .iter()
        .filter_map(|raw| {
            let message = message_from(raw)?;
            let yours = message.user.as_deref() == Some(me)
                && match message.subtype.as_deref() {
                    None => true,
                    // A thread reply sent to the channel too is still a reply.
                    Some("thread_broadcast") => thread_ts.is_some(),
                    Some(_) => false,
                };
            let in_place = match thread_ts {
                // The thread's replies, not its root.
                Some(thread) => message.ts != thread,
                // The conversation itself, not a reply sent to it as well.
                None => !message.is_reply(),
            };
            (yours && in_place).then(|| Mine {
                client_msg_id: raw
                    .get("client_msg_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ts: message.ts,
                text: message.text,
            })
        })
        .collect();
    mine.sort_by(|a, b| compare_ts(&a.ts, &b.ts));
    // A history page holds the newest messages, so a cut one misses those
    // right after the draft; a thread's replies come oldest first, and a
    // cut there only loses the later ones.
    let cut = thread_ts.is_none()
        && body
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    Ok((mine, cut))
}

/// The message sent from the draft, and how it compares. The draft's own
/// id wins; otherwise your first message after it, unless the page was
/// `cut` before it. Nothing yet in a window still `open` is pending.
fn decide(
    record: &Record,
    mine: &[Mine],
    cut: bool,
    open: bool,
) -> (Outcome, Option<(Match, Mine)>) {
    let by_id = mine
        .iter()
        .find(|m| m.client_msg_id.as_deref() == Some(record.client_msg_id.as_str()));
    let (how, message) = match (by_id, mine.first()) {
        (Some(message), _) => (Match::Id, message),
        (None, _) if cut => return (Outcome::Unknown, None),
        (None, Some(message)) => (Match::Next, message),
        (None, None) if open => return (Outcome::Pending, None),
        (None, None) => return (Outcome::NotSent, None),
    };
    let outcome = if same(&record.text, &message.text) {
        Outcome::Unchanged
    } else {
        Outcome::Edited
    };
    (outcome, Some((how, message.clone())))
}

/// Whether two texts read the same once Slack's encoding is undone: its
/// entities, `<https://…>` around a bare link, an emoji as `:name:` or as
/// its character, the spacing at the ends.
fn same(draft: &str, sent: &str) -> bool {
    let read = |text: &str| {
        glyphs(&mrkdwn::render(text, &|_| None, &|_| None, &Style::plain()))
            .trim()
            .to_string()
    };
    read(draft) == read(sent)
}

/// The `:name:` codes of standard emoji, skin tone included, as their
/// characters; anything else, a custom emoji or `12:30:00`, as it is.
fn glyphs(text: &str) -> String {
    const TONE: &str = "::skin-tone-";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(':') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let mut end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || "_+-'".contains(c)))
            .unwrap_or(after.len());
        if end > 0 && after[end..].starts_with(TONE) {
            let tone = &after[end + TONE.len()..];
            let digits = tone
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(tone.len());
            if digits > 0 {
                end += TONE.len() + digits;
            }
        }
        let glyph = (end > 0 && after[end..].starts_with(':'))
            .then(|| emoji::glyph(&after[..end]))
            .flatten();
        match glyph {
            Some(glyph) => {
                out.push_str(glyph);
                rest = &after[end + 1..];
            }
            None => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn plain(text: &str, names: &Names) -> String {
    mrkdwn::render(
        text,
        &|id| names.user_name(id),
        &|id| names.channel_name(id),
        &Style::plain(),
    )
    .trim_end()
    .to_string()
}

fn local(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .map(|at| {
            at.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        })
        .unwrap_or_else(|_| seconds.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn draft(saved: i64, channel: &str, thread_ts: Option<&str>) -> Record {
        Record {
            saved,
            team: "T1".to_string(),
            channel: channel.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            text: "Hello <@U2>, the fix is in &amp; https://x.io".to_string(),
            client_msg_id: "id-1".to_string(),
            scheduled: None,
            session: None,
        }
    }

    fn mine(ts: &str, text: &str, id: Option<&str>) -> Mine {
        Mine {
            ts: ts.to_string(),
            text: text.to_string(),
            client_msg_id: id.map(str::to_string),
        }
    }

    #[test]
    fn a_record_reads_back_without_its_optional_fields() {
        let record = draft(1_700_000_000, "C1", None);
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains("thread_ts") && !line.contains("session"));
        assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), record);
    }

    #[test]
    fn a_window_ends_at_the_next_draft_there_three_days_on_or_now() {
        let records = vec![
            draft(1_000, "C1", None),
            draft(2_000, "C1", Some("5.0")),
            draft(3_000, "C1", None),
            draft(4_000, "C2", None),
        ];
        let now = 1_000_000;
        // The thread is another place; the next draft in C1 itself closes it.
        let closed = Window {
            from: 1_000,
            until: 3_000,
            open: false,
        };
        assert_eq!(window(&records, 0, now), closed);
        assert_eq!(window(&records, 1, now).until, 2_000 + WINDOW);
        // Now comes before the end: what is not there yet may still come.
        let open = Window {
            from: 4_000,
            until: 10_000,
            open: true,
        };
        assert_eq!(window(&records, 3, 10_000), open);
    }

    #[test]
    fn a_scheduled_draft_is_looked_for_from_its_time() {
        let mut scheduled = draft(1_000, "C1", None);
        scheduled.scheduled = Some(500_000);
        // A draft saved there before Slack posts it does not close its window.
        let records = vec![
            scheduled,
            draft(2_000, "C1", None),
            draft(600_000, "C1", None),
        ];
        let expected = Window {
            from: 500_000,
            until: 600_000,
            open: false,
        };
        assert_eq!(window(&records, 0, i64::MAX), expected);
    }

    #[test]
    fn only_your_messages_in_the_place_count() {
        let body = json!({"has_more": true, "messages": [
            {"ts": "30.0", "user": "UME", "text": "third"},
            {"ts": "20.0", "user": "UME", "text": "a reply too", "thread_ts": "10.0", "subtype": "thread_broadcast"},
            {"ts": "15.0", "user": "UME", "text": "joined", "subtype": "channel_join"},
            {"ts": "12.0", "user": "U2", "text": "someone else"},
            {"ts": "11.0", "user": "UME", "text": "first", "client_msg_id": "id-1", "thread_ts": "11.0"},
        ]});
        let (found, cut) = parse_mine(&body, "UME", None, "conversations.history").unwrap();
        assert!(cut);
        let expected = vec![
            mine("11.0", "first", Some("id-1")),
            mine("30.0", "third", None),
        ];
        assert_eq!(found, expected);

        let body = json!({"has_more": true, "messages": [
            {"ts": "10.0", "user": "UME", "text": "the root", "thread_ts": "10.0"},
            {"ts": "20.0", "user": "UME", "text": "and here", "thread_ts": "10.0", "subtype": "thread_broadcast"},
        ]});
        // Replies come oldest first: a cut page still starts after the draft.
        let (found, cut) = parse_mine(&body, "UME", Some("10.0"), "conversations.replies").unwrap();
        assert!(!cut);
        assert_eq!(found, vec![mine("20.0", "and here", None)]);
        assert!(parse_mine(&json!({}), "UME", None, "conversations.history").is_err());
    }

    #[test]
    fn the_drafts_own_id_wins_over_the_next_message() {
        let record = draft(1, "C1", None);
        let found = [
            mine("2.0", "something else first", None),
            mine("3.0", "Hello <@U2>, the fix is in", Some("id-1")),
        ];
        let (outcome, matched) = decide(&record, &found, false, false);
        assert_eq!(outcome, Outcome::Edited);
        let matched = matched.map(|(how, m)| (how, m.ts));
        assert_eq!(matched, Some((Match::Id, "3.0".to_string())));
    }

    #[test]
    fn the_next_message_stands_in_unless_the_page_was_cut() {
        let record = draft(1, "C1", None);
        // Slack's own encoding of the same text is still the same text.
        let found = [mine(
            "2.0",
            "Hello <@U2>, the fix is in &amp; <https://x.io>",
            None,
        )];
        let (outcome, matched) = decide(&record, &found, false, false);
        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(matched.map(|(how, _)| how), Some(Match::Next));
        assert_eq!(
            decide(&record, &found, true, false),
            (Outcome::Unknown, None)
        );
        assert_eq!(decide(&record, &[], false, false), (Outcome::NotSent, None));
        assert_eq!(decide(&record, &[], false, true), (Outcome::Pending, None));
    }

    #[test]
    fn an_emoji_reads_the_same_as_a_code_or_a_character() {
        assert!(same("Merci 🙂", "Merci :slightly_smiling_face:"));
        assert!(same("ok 👍🏻", "ok :+1::skin-tone-2:"));
        assert!(!same("Merci 🙂", "Merci"));
        let untouched = "at 12:30:00, :custom-party: https://x.io";
        assert_eq!(glyphs(untouched), untouched);
    }
}
