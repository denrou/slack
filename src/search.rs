//! Slack's message search, `search.messages`: what a query finds in the
//! conversations you can see, newest first, one page. The query is
//! Slack's own, operators included (`in:#dev from:@jane before:2026-10-01`).

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::history::{Message, message_from};
use crate::names::Names;
use crate::target::parse_permalink;
use crate::view;

const METHOD: &str = "search.messages";
/// Slack's cap on a page.
pub const MAX_COUNT: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct Found {
    /// Every match Slack counts, beyond the page too.
    pub total: u64,
    pub hits: Vec<Hit>,
}

/// One message the query found, where it was posted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub channel: String,
    /// Its `thread_ts` comes from the permalink: a search match carries
    /// none of its own.
    #[serde(flatten)]
    pub message: Message,
    pub permalink: String,
}

impl Hit {
    /// The channel message to read it around: the thread's root for a
    /// reply, which the channel's history holds where the reply is not.
    pub fn anchor(&self) -> &str {
        self.message
            .thread_ts
            .as_deref()
            .unwrap_or(&self.message.ts)
    }
}

pub fn fetch(client: &Client, query: &str, count: usize) -> Result<Found, api::Error> {
    let count = count.clamp(1, MAX_COUNT).to_string();
    let body = client.call(
        METHOD,
        &[
            ("query", query),
            ("count", &count),
            ("sort", "timestamp"),
            ("sort_dir", "desc"),
        ],
    )?;
    parse(&body)
}

/// Name what the hits show: their conversations, the person of a direct
/// message, the authors, the people mentioned and the channels linked.
pub fn name(client: &Client, names: &mut Names, hits: &[Hit]) -> Result<(), api::Error> {
    let mut people = BTreeSet::new();
    let mut places: BTreeSet<String> = hits.iter().map(|hit| hit.channel.clone()).collect();
    for hit in hits {
        view::collect_names(&mut people, &mut places, &hit.message);
    }
    names.resolve_channels(client, places)?;
    people.extend(
        hits.iter()
            .filter_map(|hit| names.channel(&hit.channel)?.user.clone()),
    );
    names.resolve_users(client, people)
}

pub fn parse(body: &Value) -> Result<Found, api::Error> {
    let block = body.get("messages").ok_or_else(|| api::Error::Shape {
        method: METHOD.to_string(),
        reason: "no `messages` block".to_string(),
    })?;
    let total = block.get("total").and_then(Value::as_u64).unwrap_or(0);
    let matches = block.get("matches").and_then(Value::as_array);
    let hits: Vec<Hit> = matches
        .map(|matches| matches.iter().filter_map(hit_from).collect())
        .unwrap_or_default();
    // Slack found something and none of it reads: a shape that moved,
    // which "nothing matches" would hide.
    let broken = match matches {
        None if total > 0 => Some(format!("{total} matches and no `matches` list")),
        Some(matches) if hits.is_empty() && !matches.is_empty() => {
            Some(format!("{} matches, none readable", matches.len()))
        }
        _ => None,
    };
    if let Some(reason) = broken {
        return Err(api::Error::Shape {
            method: METHOD.to_string(),
            reason,
        });
    }
    Ok(Found { total, hits })
}

fn hit_from(raw: &Value) -> Option<Hit> {
    let channel = raw.pointer("/channel/id").and_then(Value::as_str)?;
    let permalink = raw.get("permalink").and_then(Value::as_str)?;
    let mut message = message_from(raw)?;
    message.thread_ts = parse_permalink(permalink).and_then(|link| link.thread_ts);
    Some(Hit {
        channel: channel.to_string(),
        message,
        permalink: permalink.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_match_keeps_its_channel_its_permalink_and_its_thread() {
        let body = json!({
            "ok": true,
            "messages": {
                "total": 1801,
                "matches": [
                    {"type": "message", "ts": "1700000030.000200", "user": "U1", "text": "a reply about deploy",
                     "channel": {"id": "C0A754E2V", "name": "dev"},
                     "permalink": "https://acme.slack.com/archives/C0A754E2V/p1700000030000200?thread_ts=1700000010.000100&cid=C0A754E2V"},
                    {"type": "im", "ts": "1700000020.000100", "user": "U2", "text": "deploy?",
                     "channel": {"id": "D06JANE12", "name": "U2", "is_im": true},
                     "permalink": "https://acme.slack.com/archives/D06JANE12/p1700000020000100"},
                    {"type": "message", "ts": "1700000015.000100", "text": "no channel",
                     "permalink": "https://acme.slack.com/archives/C0A754E2V/p1700000015000100"},
                ]
            }
        });
        let found = parse(&body).unwrap();
        assert_eq!(found.total, 1801);
        assert_eq!(found.hits.len(), 2, "a match without a channel is dropped");
        let reply = &found.hits[0];
        assert_eq!(reply.channel, "C0A754E2V");
        assert!(reply.message.is_reply());
        assert_eq!(reply.anchor(), "1700000010.000100");
        let dm = &found.hits[1];
        assert_eq!(
            (dm.channel.as_str(), dm.anchor()),
            ("D06JANE12", "1700000020.000100")
        );
        assert!(dm.message.thread_ts.is_none());
    }

    #[test]
    fn no_match_is_an_empty_page_and_no_block_a_shape_error() {
        let found = parse(&json!({"ok": true, "messages": {"total": 0}})).unwrap();
        assert!(found.hits.is_empty());
        assert!(matches!(
            parse(&json!({"ok": true})),
            Err(api::Error::Shape { .. })
        ));
    }

    #[test]
    fn matches_that_none_reads_are_a_shape_error() {
        let moved = json!({"ok": true, "messages": {"total": 3, "matches": [
            {"ts": "1.0", "text": "a", "channel": "C1", "permalink": "https://acme.slack.com/archives/C1/p1"}
        ]}});
        assert!(matches!(parse(&moved), Err(api::Error::Shape { .. })));
        let gone = json!({"ok": true, "messages": {"total": 3}});
        assert!(matches!(parse(&gone), Err(api::Error::Shape { .. })));
    }
}
