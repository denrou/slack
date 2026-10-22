//! `client.counts`: the unread state of every conversation in one call,
//! plus a `threads` block for the Threads view. It drives both the count
//! and the view, and it is one of the web client's own endpoints, so its
//! shape is read defensively: a shape change must surface as an error,
//! never as "all caught up".

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::prefs::{self, Prefs};

const METHOD: &str = "client.counts";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Channel,
    /// A direct message.
    Im,
    /// A group direct message.
    Mpim,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: String,
    pub kind: Kind,
    pub has_unreads: bool,
    pub mention_count: u64,
    /// The read cursor; `conversations.history` from here gives the unread part.
    pub last_read: Option<String>,
    /// The newest message; `conversations.mark` here reads everything.
    pub latest: Option<String>,
    /// Muted in Slack: the app badges it for mentions only.
    pub muted: bool,
}

impl Conversation {
    pub fn is_unread(&self) -> bool {
        self.has_unreads || self.mention_count > 0
    }

    /// Unread the way the app's sidebar shows it: anything with something
    /// new, except a muted conversation, which only counts for a mention
    /// in a channel (a DM's `mention_count` is just its unread count).
    /// Slack's own mute list decides, nothing local.
    pub fn counts_as_unread(&self) -> bool {
        let mentioned = self.kind == Kind::Channel && self.mention_count > 0;
        self.is_unread() && (!self.muted || mentioned)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Threads {
    pub has_unreads: bool,
    pub mention_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Counts {
    pub conversations: Vec<Conversation>,
    /// None when Slack sent no usable block: "not reported" is not "none".
    pub threads: Option<Threads>,
    /// From the preferences read beside the counts, for the screen's `R`.
    #[serde(skip)]
    pub frequent_emoji: Vec<String>,
    /// The conversations visited last in the app, for the screen's recent
    /// sections.
    #[serde(skip)]
    pub recent: Vec<String>,
    /// The preferences could not be read, so nothing reads as muted.
    #[serde(skip)]
    pub prefs_missing: bool,
}

impl Counts {
    pub fn parse(body: &Value) -> Result<Counts, api::Error> {
        let mut conversations = Vec::new();
        let mut arrays = 0;
        for (key, kind) in [
            ("channels", Kind::Channel),
            ("mpims", Kind::Mpim),
            ("ims", Kind::Im),
        ] {
            let Some(entries) = body.get(key).and_then(Value::as_array) else {
                continue;
            };
            arrays += 1;
            let parsed: Vec<Conversation> = entries
                .iter()
                .filter_map(|entry| conversation(entry, kind))
                .collect();
            if parsed.is_empty() && !entries.is_empty() {
                return Err(shape(format!("no entry of `{key}` has an id")));
            }
            conversations.extend(parsed);
        }
        if arrays == 0 {
            return Err(shape("none of `channels`, `mpims`, `ims` is present"));
        }
        Ok(Counts {
            conversations,
            threads: body.get("threads").and_then(threads),
            frequent_emoji: Vec::new(),
            recent: Vec::new(),
            prefs_missing: false,
        })
    }

    /// Flag the conversations the user muted in Slack.
    pub fn apply_prefs(&mut self, prefs: &Prefs) {
        for conversation in &mut self.conversations {
            conversation.muted = prefs.muted.contains(&conversation.id);
        }
        self.frequent_emoji.clone_from(&prefs.frequent_emoji);
        self.recent.clone_from(&prefs.recent);
    }

    pub fn unread(&self) -> impl Iterator<Item = &Conversation> {
        self.conversations.iter().filter(|c| c.counts_as_unread())
    }

    /// Unread conversations the mute list keeps out of the view.
    pub fn muted_unread_count(&self) -> usize {
        self.conversations
            .iter()
            .filter(|c| c.is_unread() && !c.counts_as_unread())
            .count()
    }

    /// A thread mention counts even if Slack left `has_unreads` false, the
    /// same rule as for conversations.
    pub fn threads_unread(&self) -> bool {
        self.threads
            .is_some_and(|t| t.has_unreads || t.mention_count > 0)
    }

    /// Every unread conversation, plus one for the Threads view: the
    /// status-bar number.
    pub fn unread_count(&self) -> usize {
        self.unread().count() + usize::from(self.threads_unread())
    }

    /// The messages aimed at you: the unread messages of a DM or group
    /// message (at least one), the mentions in a channel and in followed
    /// threads. Only over `unread`, so a muted DM stays out here too.
    pub fn me_count(&self) -> u64 {
        let conversations: u64 = self
            .unread()
            .map(|c| match c.kind {
                Kind::Channel => c.mention_count,
                Kind::Im | Kind::Mpim => c.mention_count.max(1),
            })
            .sum();
        conversations + self.threads.map_or(0, |t| t.mention_count)
    }
}

/// The read cursor of every conversation, from `client.counts` alone, for
/// a mark that must only ever move forward.
pub fn cursors(client: &Client) -> Result<BTreeMap<String, String>, api::Error> {
    let counts = Counts::parse(&client.call(METHOD, &[])?)?;
    Ok(counts
        .conversations
        .into_iter()
        .filter_map(|c| Some((c.id, c.last_read?)))
        .collect())
}

/// The counts with the notification preferences applied; the two calls
/// run side by side so the status-bar path stays one round trip long.
pub fn fetch(client: &Client) -> Result<Counts, api::Error> {
    let (body, prefs) = std::thread::scope(|scope| {
        let prefs = scope.spawn(|| prefs::fetch(client));
        let body = client.call(METHOD, &[]);
        // A panic in the prefs thread is a bug, not a condition to handle.
        (body, prefs.join().expect("the prefs thread panicked"))
    });
    let mut counts = Counts::parse(&body?)?;
    match prefs? {
        Some(prefs) => counts.apply_prefs(&prefs),
        None => counts.prefs_missing = true,
    }
    Ok(counts)
}

fn conversation(entry: &Value, kind: Kind) -> Option<Conversation> {
    let string = |name: &str| entry.get(name).and_then(Value::as_str).map(str::to_string);
    Some(Conversation {
        id: string("id")?,
        kind,
        has_unreads: entry
            .get("has_unreads")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mention_count: entry
            .get("mention_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        last_read: string("last_read"),
        latest: string("latest"),
        muted: false,
    })
}

fn threads(block: &Value) -> Option<Threads> {
    block.as_object().map(|block| Threads {
        has_unreads: block
            .get("has_unreads")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mention_count: block
            .get("mention_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn shape(reason: impl Into<String>) -> api::Error {
    api::Error::Shape {
        method: METHOD.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture() -> Counts {
        let body: Value =
            serde_json::from_str(include_str!("../tests/fixtures/client_counts.json")).unwrap();
        Counts::parse(&body).unwrap()
    }

    fn prefs(muted: &[&str]) -> Prefs {
        Prefs {
            muted: muted.iter().map(|s| s.to_string()).collect(),
            ..Prefs::default()
        }
    }

    fn ids<'a>(conversations: impl Iterator<Item = &'a Conversation>) -> Vec<&'a str> {
        conversations.map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn every_array_is_read_with_its_kind() {
        let counts = fixture();
        assert_eq!(counts.conversations.len(), 7);
        let kind = |id: &str| {
            counts
                .conversations
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .kind
        };
        assert_eq!(kind("C02ALERTS"), Kind::Channel);
        assert_eq!(kind("G05GROUP"), Kind::Mpim);
        assert_eq!(kind("D06JANE"), Kind::Im);
    }

    #[test]
    fn unread_is_every_conversation_with_something_new_and_the_threads_view() {
        let counts = fixture();
        assert_eq!(ids(counts.unread()), ["C02ALERTS", "C03RANDOM", "D06JANE"]);
        assert!(counts.threads_unread());
        assert_eq!(counts.unread_count(), 4);
        assert_eq!(
            counts.me_count(),
            3,
            "the mention in C02ALERTS and the two unread messages of D06JANE"
        );
    }

    #[test]
    fn muted_conversations_count_for_their_mentions_only() {
        let mut counts = fixture();
        counts.apply_prefs(&prefs(&["C03RANDOM", "D06JANE"]));
        assert_eq!(
            ids(counts.unread()),
            ["C02ALERTS"],
            "a muted channel without a mention and a muted DM drop out"
        );
        assert_eq!(counts.muted_unread_count(), 2);
        assert_eq!(counts.unread_count(), 2);
        assert_eq!(
            counts.me_count(),
            1,
            "a muted DM is not aimed at you either"
        );

        counts.apply_prefs(&prefs(&["C02ALERTS"]));
        assert!(
            counts.unread().any(|c| c.id == "C02ALERTS"),
            "a mention in a muted channel still counts"
        );
        assert_eq!(counts.muted_unread_count(), 0);
    }

    #[test]
    fn a_mention_counts_without_has_unreads() {
        let body =
            json!({ "channels": [{ "id": "C1", "has_unreads": false, "mention_count": 2 }] });
        let counts = Counts::parse(&body).unwrap();
        assert_eq!(counts.unread_count(), 1);
        assert!(counts.threads.is_none());
    }

    #[test]
    fn a_missing_shape_is_an_error_not_silence() {
        assert!(matches!(
            Counts::parse(&json!({ "ok": true })),
            Err(api::Error::Shape { .. })
        ));
        let all_malformed = json!({ "channels": [{ "has_unreads": true }] });
        assert!(matches!(
            Counts::parse(&all_malformed),
            Err(api::Error::Shape { .. })
        ));
    }

    #[test]
    fn one_malformed_entry_drops_that_entry_only() {
        let body = json!({ "ims": [{ "id": "D1", "has_unreads": true }, { "oops": 1 }] });
        let counts = Counts::parse(&body).unwrap();
        assert_eq!(counts.conversations.len(), 1);
        assert_eq!(counts.unread_count(), 1);
    }
}
