//! Who a person is to you, for `slack about`: what you noted about them
//! in `~/.config/slack/people.toml` (yours, never sent anywhere), and how
//! much the two of you exchanged in your direct message. The exchange is
//! read through Slack's search, `in:@handle`, which counts a DM closed in
//! the sidebar too and gives the total beyond its page in one call; the
//! profile and the presence are `presence.rs`'s.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::{Timestamp, ToSpan, Zoned, civil::Date};
use serde::{Deserialize, Serialize};

use crate::api::{self, Client};
use crate::history::Message;
use crate::presence::Described;
use crate::search::{self, Found};
use crate::{paths, view};

/// The most people one run describes: each costs a search, which Slack
/// allows about twenty times a minute.
pub const MAX: usize = 5;
/// How far back the direct message is counted: the history Slack's free
/// plan keeps.
const DAYS: i64 = 90;
/// The recent part, where talking every day shows.
const RECENT_DAYS: i64 = 30;
/// The newest messages kept, for the tone and the language.
const LATEST: usize = 5;

/// Where the person sits from you, which changes how a message to them
/// is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Relation {
    Manager,
    SkipManager,
    Exec,
    Report,
    Peer,
    External,
}

impl Relation {
    pub fn word(self) -> &'static str {
        match self {
            Relation::Manager => "manager",
            Relation::SkipManager => "skip-manager",
            Relation::Exec => "exec",
            Relation::Report => "report",
            Relation::Peer => "peer",
            Relation::External => "external",
        }
    }
}

/// One person's entry in `people.toml`. Unknown keys are refused, so a
/// typo does not quietly drop a note.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub relation: Option<Relation>,
    pub note: Option<String>,
}

/// `people.toml`: a table per person, keyed by handle (`[jdoe]`, or
/// `["@jdoe"]`) or by user id (`[U0123ABCD]`, which survives a change of
/// handle).
#[derive(Debug, Default)]
pub struct Notes(BTreeMap<String, Note>);

impl Notes {
    pub fn path() -> Result<PathBuf> {
        Ok(paths::config_dir()?.join("people.toml"))
    }

    /// The file, or no notes when there is none. A file that does not
    /// read fails the run: a draft written without the note it holds is
    /// what the file is there to prevent.
    pub fn load(path: &Path) -> Result<Notes> {
        match fs::read_to_string(path) {
            Ok(text) => {
                Notes::parse(&text).with_context(|| format!("{} does not read", path.display()))
            }
            Err(err) if paths::is_not_found(&err) => Ok(Notes::default()),
            Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Notes, toml::de::Error> {
        let raw: BTreeMap<String, Note> = toml::from_str(text)?;
        Ok(Notes(
            raw.into_iter()
                .map(|(key, note)| (key.trim_start_matches('@').to_lowercase(), note))
                .collect(),
        ))
    }

    /// The entry under their id, else under their handle.
    pub fn of(&self, user: &str, handle: &str) -> Option<&Note> {
        self.0
            .get(&user.to_lowercase())
            .or_else(|| self.0.get(&handle.to_lowercase()))
    }
}

/// How much you exchanged in the direct message.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Dm {
    /// Every message of the last 90 days, as Slack counts them.
    #[serde(rename = "messages_90d")]
    pub messages: u64,
    /// The messages of the last 30 days, and the days they fall on (your
    /// clock).
    #[serde(rename = "messages_30d")]
    pub recent_messages: usize,
    #[serde(rename = "days_30d")]
    pub recent_days: usize,
    /// Slack's page stopped short of 30 days back, so the two counts
    /// above are lower bounds.
    pub at_least: bool,
    /// The newest message.
    pub last: Option<String>,
    /// The newest few, oldest first.
    pub latest: Vec<Message>,
    /// The DM's id, to render `latest`.
    #[serde(skip)]
    pub channel: Option<String>,
}

impl Dm {
    pub fn describe(&self, now: &Zoned) -> String {
        if self.messages == 0 {
            return format!("no direct message in the last {DAYS} days");
        }
        let plural = |n: u64, word: &str| {
            if n == 1 {
                format!("1 {word}")
            } else {
                format!("{n} {word}s")
            }
        };
        let plus = if self.at_least { "+" } else { "" };
        let recent = match self.recent_messages {
            0 => format!("none in the last {RECENT_DAYS}"),
            n => {
                let days = self.recent_days;
                let s = if days == 1 && !self.at_least { "" } else { "s" };
                format!("{n}{plus} in the last {RECENT_DAYS}, on {days}{plus} day{s}")
            }
        };
        let mut line = format!(
            "{} in {DAYS} days; {recent}",
            plural(self.messages, "message")
        );
        if let Some(last) = &self.last {
            let when = view::format_ts(last, now);
            // A time alone is today's: said, since "last 11:37" reads oddly.
            let today = if when.contains(' ') { "" } else { "today " };
            line.push_str(&format!("; last {today}{when}"));
        }
        line
    }
}

/// The search for your DM with them over the last 90 days. `after:` is
/// exclusive, hence the day more.
pub fn query(handle: &str, today: Date) -> String {
    let after = today.saturating_sub((DAYS + 1).days());
    format!("in:@{handle} after:{after}")
}

pub fn dm(client: &Client, handle: &str, now: &Zoned) -> Result<Dm, api::Error> {
    let found = search::fetch(client, &query(handle, now.date()), search::MAX_COUNT)?;
    Ok(summarise(&found, now))
}

/// The counts out of one page of matches, newest first.
pub fn summarise(found: &Found, now: &Zoned) -> Dm {
    let since = now.timestamp().as_second() - RECENT_DAYS * 86_400;
    let seconds = |message: &Message| -> Option<i64> { message.ts.split('.').next()?.parse().ok() };
    let recent: Vec<i64> = found
        .hits
        .iter()
        .filter_map(|hit| seconds(&hit.message))
        .filter(|at| *at >= since)
        .collect();
    let days: BTreeSet<Date> = recent
        .iter()
        .filter_map(|at| Timestamp::from_second(*at).ok())
        .map(|stamp| stamp.to_zoned(now.time_zone().clone()).date())
        .collect();
    let cut = found.total > found.hits.len() as u64;
    let reached = found
        .hits
        .last()
        .and_then(|hit| seconds(&hit.message))
        .is_some_and(|at| at < since);
    Dm {
        messages: found.total.max(found.hits.len() as u64),
        recent_messages: recent.len(),
        recent_days: days.len(),
        at_least: cut && !reached,
        last: found.hits.first().map(|hit| hit.message.ts.clone()),
        latest: found
            .hits
            .iter()
            .take(LATEST)
            .rev()
            .map(|hit| hit.message.clone())
            .collect(),
        channel: found.hits.first().map(|hit| hit.channel.clone()),
    }
}

/// One person as `slack about --json` prints them: `slack who`'s fields,
/// the title, the note and the DM.
#[derive(Serialize)]
pub struct Card<'a> {
    #[serde(flatten)]
    pub described: Described<'a>,
    pub title: Option<&'a str>,
    pub relation: Option<Relation>,
    pub note: Option<&'a str>,
    pub dm: &'a Dm,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn now() -> Zoned {
        "2026-11-18T15:00:00+01:00[Europe/Paris]".parse().unwrap()
    }

    /// A page of matches, newest first, `days_ago` each.
    fn found(total: u64, days_ago: &[i64]) -> Found {
        let now = now().timestamp().as_second();
        let matches: Vec<_> = days_ago
            .iter()
            .enumerate()
            .map(|(i, days)| {
                let ts = format!("{}.{:06}", now - days * 86_400 - i as i64, i);
                json!({
                    "channel": {"id": "D1"},
                    "permalink": format!("https://x.slack.com/archives/D1/p{}", ts.replace('.', "")),
                    "ts": ts,
                    "user": "U1",
                    "text": format!("message {i}"),
                })
            })
            .collect();
        search::parse(&json!({"messages": {"total": total, "matches": matches}})).unwrap()
    }

    #[test]
    fn the_query_reaches_ninety_days_back_and_takes_the_handle() {
        let today: Date = "2026-11-18".parse().unwrap();
        assert_eq!(query("jdoe", today), "in:@jdoe after:2026-08-19");
    }

    #[test]
    fn a_complete_page_counts_exactly() {
        let dm = summarise(&found(5, &[0, 0, 1, 12, 45]), &now());
        assert_eq!(dm.messages, 5);
        assert_eq!(dm.recent_messages, 4);
        assert_eq!(dm.recent_days, 3, "two messages today count as one day");
        assert!(!dm.at_least);
        assert_eq!(dm.channel.as_deref(), Some("D1"));
        let texts: Vec<&str> = dm.latest.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "message 4",
                "message 3",
                "message 2",
                "message 1",
                "message 0"
            ],
            "the newest five, oldest first"
        );
        assert_eq!(dm.last.as_ref(), Some(&dm.latest[4].ts));
        let line = dm.describe(&now());
        assert!(
            line.starts_with("5 messages in 90 days; 4 in the last 30, on 3 days; last today "),
            "{line}"
        );
    }

    #[test]
    fn a_page_cut_inside_the_month_gives_lower_bounds() {
        let dm = summarise(&found(250, &[0, 1, 2]), &now());
        assert!(dm.at_least);
        assert_eq!(dm.messages, 250);
        let line = dm.describe(&now());
        assert!(
            line.starts_with("250 messages in 90 days; 3+ in the last 30, on 3+ days"),
            "{line}"
        );
    }

    #[test]
    fn a_page_cut_past_the_month_counts_the_month_exactly() {
        let dm = summarise(&found(250, &[0, 1, 40]), &now());
        assert!(!dm.at_least);
        assert_eq!(dm.recent_messages, 2);
    }

    #[test]
    fn nothing_found_says_so() {
        let dm = summarise(&found(0, &[]), &now());
        assert_eq!(dm, Dm::default());
        assert_eq!(dm.describe(&now()), "no direct message in the last 90 days");
    }

    #[test]
    fn a_quiet_month_says_none() {
        let dm = summarise(&found(1, &[50]), &now());
        let line = dm.describe(&now());
        assert!(
            line.starts_with("1 message in 90 days; none in the last 30; last "),
            "{line}"
        );
    }

    #[test]
    fn notes_are_found_by_id_or_handle_whatever_the_case() {
        let notes = Notes::parse(
            r#"
            [jdoe]
            relation = "manager"
            note = "One summary rather than several messages."

            ["@Bob"]
            relation = "external"

            [U0123]
            relation = "skip-manager"
            "#,
        )
        .unwrap();
        let jdoe = notes.of("U9", "jdoe").unwrap();
        assert_eq!(jdoe.relation, Some(Relation::Manager));
        assert_eq!(
            jdoe.note.as_deref(),
            Some("One summary rather than several messages.")
        );
        assert_eq!(
            notes.of("U8", "bob").unwrap().relation,
            Some(Relation::External)
        );
        assert_eq!(
            notes.of("U0123", "renamed").unwrap().relation,
            Some(Relation::SkipManager),
            "the id wins over the handle"
        );
        assert!(notes.of("U7", "someone").is_none());
    }

    #[test]
    fn a_typo_in_the_notes_is_an_error() {
        assert!(Notes::parse("[jdoe]\nrelaton = \"manager\"\n").is_err());
        assert!(Notes::parse("[jdoe]\nrelation = \"boss\"\n").is_err());
    }

    #[test]
    fn a_missing_file_is_no_notes() {
        let notes = Notes::load(Path::new("/nonexistent/people.toml")).unwrap();
        assert!(notes.of("U1", "jdoe").is_none());
    }
}
