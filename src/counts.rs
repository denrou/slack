//! `client.counts`: the unread state of every conversation in one call,
//! plus a `threads` block for the Threads view. It drives both the count
//! and the view, and it is one of the web client's own endpoints, so its
//! shape is read defensively: a shape change must surface as an error,
//! never as "all caught up".

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::names::Names;
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
    /// A private channel: someone chose to put you in it.
    pub private: bool,
    /// Set to "All new messages" in Slack.
    pub notify_all: bool,
}

impl Conversation {
    pub fn is_unread(&self) -> bool {
        self.has_unreads || self.mention_count > 0
    }

    /// Someone wrote to you (a DM or group DM with unreads), named you in
    /// a channel, or wrote in a channel you follow: a private one, where
    /// someone chose to put you, or one set to "All new messages" in
    /// Slack. Public channel chatter does not count, nor does anything
    /// you muted, except a mention, which the app badges too.
    pub fn needs_attention(&self) -> bool {
        match self.kind {
            Kind::Channel => {
                self.mention_count > 0
                    || (self.is_unread() && !self.muted && (self.private || self.notify_all))
            }
            Kind::Im | Kind::Mpim => self.is_unread() && !self.muted,
        }
    }

    /// A followed channel with something new but no mention: shown by
    /// default, unlike the other channels.
    pub fn is_followed_channel(&self) -> bool {
        self.kind == Kind::Channel && self.mention_count == 0 && self.needs_attention()
    }

    /// Unread the way the app badges it: a muted conversation only counts
    /// for its mentions.
    pub fn counts_as_unread(&self) -> bool {
        self.is_unread() && (!self.muted || self.needs_attention())
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
        })
    }

    /// Flag the conversations the user muted or follows in Slack.
    pub fn apply_prefs(&mut self, prefs: &Prefs) {
        for conversation in &mut self.conversations {
            conversation.muted = prefs.muted.contains(&conversation.id);
            conversation.notify_all = prefs.notify_all.contains(&conversation.id);
        }
    }

    /// Flag the private channels; `conversations.info` is what knows.
    pub fn apply_privacy(&mut self, is_private: impl Fn(&str) -> bool) {
        for conversation in &mut self.conversations {
            conversation.private =
                conversation.kind == Kind::Channel && is_private(&conversation.id);
        }
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

    pub fn attention(&self) -> impl Iterator<Item = &Conversation> {
        self.conversations.iter().filter(|c| c.needs_attention())
    }

    /// A thread mention counts even if Slack left `has_unreads` false, the
    /// same rule as for conversations.
    pub fn threads_unread(&self) -> bool {
        self.threads
            .is_some_and(|t| t.has_unreads || t.mention_count > 0)
    }

    /// Conversations that need you, plus one for the Threads view.
    pub fn attention_count(&self) -> usize {
        self.attention().count() + usize::from(self.threads_unread())
    }

    /// Every conversation with unreads, plus one for the Threads view.
    pub fn unread_count(&self) -> usize {
        self.unread().count() + usize::from(self.threads_unread())
    }
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
    counts.apply_prefs(&prefs?);
    Ok(counts)
}

/// `fetch`, plus the privacy of the unread channels, which only
/// `conversations.info` knows; the names cache keeps that to one call per
/// channel ever.
pub fn load(client: &Client, names: &mut Names) -> Result<Counts, api::Error> {
    let mut counts = fetch(client)?;
    let unread: Vec<String> = counts
        .conversations
        .iter()
        .filter(|c| c.kind == Kind::Channel && c.is_unread())
        .map(|c| c.id.clone())
        .collect();
    names.resolve_channels(client, unread)?;
    counts.apply_privacy(|id| names.channel(id).is_some_and(|c| c.is_private));
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
        private: false,
        notify_all: false,
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

    fn prefs(muted: &[&str], notify_all: &[&str]) -> Prefs {
        Prefs {
            muted: muted.iter().map(|s| s.to_string()).collect(),
            notify_all: notify_all.iter().map(|s| s.to_string()).collect(),
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
    fn attention_is_dms_mentions_and_the_threads_view() {
        let counts = fixture();
        assert_eq!(ids(counts.attention()), ["C02ALERTS", "D06JANE"]);
        assert!(counts.threads_unread());
        assert_eq!(counts.attention_count(), 3);
    }

    #[test]
    fn unread_adds_the_channels_without_a_mention() {
        let counts = fixture();
        assert_eq!(ids(counts.unread()), ["C02ALERTS", "C03RANDOM", "D06JANE"]);
        assert_eq!(counts.unread_count(), 4);
    }

    #[test]
    fn followed_channels_need_attention_without_a_mention() {
        let mut counts = fixture();
        assert!(!counts.conversations.iter().any(|c| c.is_followed_channel()));

        counts.apply_prefs(&prefs(&[], &["C03RANDOM"]));
        let followed = ids(counts
            .conversations
            .iter()
            .filter(|c| c.is_followed_channel()));
        assert_eq!(followed, ["C03RANDOM"]);
        assert_eq!(counts.attention_count(), 4);

        // Privacy is the other way in; muting wins over both, and a channel
        // with a mention is never "followed", it is a mention.
        counts.apply_prefs(&prefs(&[], &[]));
        counts.apply_privacy(|id| id == "C03RANDOM" || id == "C02ALERTS" || id == "D06JANE");
        let followed = ids(counts
            .conversations
            .iter()
            .filter(|c| c.is_followed_channel()));
        assert_eq!(followed, ["C03RANDOM"]);
        assert!(
            !counts
                .conversations
                .iter()
                .find(|c| c.id == "D06JANE")
                .unwrap()
                .private
        );
        counts.apply_prefs(&prefs(&["C03RANDOM"], &["C03RANDOM"]));
        assert!(!counts.conversations.iter().any(|c| c.is_followed_channel()));
        assert_eq!(counts.attention_count(), 3);
    }

    #[test]
    fn muted_conversations_count_for_their_mentions_only() {
        let mut counts = fixture();
        counts.apply_prefs(&prefs(&["C03RANDOM", "D06JANE"], &[]));
        assert_eq!(
            ids(counts.unread()),
            ["C02ALERTS"],
            "a muted channel without a mention and a muted DM drop out"
        );
        assert_eq!(counts.muted_unread_count(), 2);
        assert_eq!(counts.attention_count(), 2);

        counts.apply_prefs(&prefs(&["C02ALERTS"], &[]));
        assert!(
            counts.attention().any(|c| c.id == "C02ALERTS"),
            "a mention in a muted channel still counts"
        );
        assert_eq!(counts.muted_unread_count(), 0);
    }

    #[test]
    fn a_mention_counts_without_has_unreads() {
        let body =
            json!({ "channels": [{ "id": "C1", "has_unreads": false, "mention_count": 2 }] });
        let counts = Counts::parse(&body).unwrap();
        assert_eq!(counts.attention_count(), 1);
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
        assert_eq!(counts.attention_count(), 1);
    }
}
