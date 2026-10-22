//! A conversation's messages from `conversations.history`: the unread
//! part from its read cursor, or the recent history without one. Slack
//! answers newest first, so with a limit the page holds the most recent
//! messages; `has_more` says older ones exist.

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::emoji;

pub const DEFAULT_LIMIT: usize = 20;
const METHOD: &str = "conversations.history";

/// Joins and leaves are not something to read.
const NOISE: &[&str] = &["channel_join", "channel_leave", "group_join", "group_leave"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The bot's name when a bot posted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub reply_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
}

/// One emoji on a message, with who put it there.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Reaction {
    /// Slack's emoji name, `+1` or `+1::skin-tone-2`.
    pub name: String,
    pub count: u64,
    /// Slack lists the first of them only when there are many.
    pub users: Vec<String>,
}

impl Reaction {
    pub fn by(&self, user_id: &str) -> bool {
        self.users.iter().any(|user| user == user_id)
    }
}

impl Message {
    /// A reply posted in a thread (its own ts differs from the thread's).
    pub fn is_reply(&self) -> bool {
        self.thread_ts
            .as_deref()
            .is_some_and(|thread| thread != self.ts)
    }

    pub fn mentions(&self, user_id: &str) -> bool {
        self.text.contains(&format!("<@{user_id}>")) || self.text.contains(&format!("<@{user_id}|"))
    }

    /// Put or take back one person's reaction, as Slack now has it: the
    /// screen updates the message instead of reading it again.
    pub fn react(&mut self, name: &str, user_id: &str, add: bool) {
        let at = self
            .reactions
            .iter()
            .position(|r| emoji::same(&r.name, name));
        match (at, add) {
            (Some(at), true) => {
                let reaction = &mut self.reactions[at];
                if !reaction.by(user_id) {
                    reaction.count += 1;
                    reaction.users.push(user_id.to_string());
                }
            }
            (None, true) => self.reactions.push(Reaction {
                name: name.to_string(),
                count: 1,
                users: vec![user_id.to_string()],
            }),
            (Some(at), false) => {
                let reaction = &mut self.reactions[at];
                if reaction.by(user_id) {
                    reaction.users.retain(|user| user != user_id);
                    reaction.count = reaction.count.saturating_sub(1);
                }
                if reaction.count == 0 {
                    self.reactions.remove(at);
                }
            }
            (None, false) => {}
        }
    }
}

/// A page of a conversation: its unread part, or its recent history.
#[derive(Debug, Clone, Serialize)]
pub struct Unread {
    /// Oldest first.
    pub messages: Vec<Message>,
    /// Older messages exist beyond the page.
    pub has_more: bool,
}

pub fn fetch(
    client: &Client,
    channel: &str,
    last_read: Option<&str>,
    limit: usize,
) -> Result<Unread, api::Error> {
    let limit_text = limit.to_string();
    let mut params = vec![("channel", channel), ("limit", limit_text.as_str())];
    if let Some(cursor) = last_read.filter(|cursor| is_usable_cursor(cursor)) {
        params.push(("oldest", cursor));
    }
    parse(&client.call(METHOD, &params)?, last_read)
}

/// The messages around one, `side` on each side: the page up to it, and
/// the page after it, since with `oldest` alone Slack answers the
/// messages right after it rather than the newest (seen live on
/// 2026-10-21). `has_more` says older ones exist.
pub fn around(client: &Client, channel: &str, ts: &str, side: usize) -> Result<Unread, api::Error> {
    let before = (side + 1).to_string();
    let after = side.to_string();
    let (older, newer) = std::thread::scope(|scope| {
        let newer = scope.spawn(|| {
            let params = [("channel", channel), ("oldest", ts), ("limit", &after)];
            client.call(METHOD, &params)
        });
        let params = [
            ("channel", channel),
            ("latest", ts),
            ("inclusive", "true"),
            ("limit", &before),
        ];
        let older = client.call(METHOD, &params);
        // A panic in the other call is a bug, not a condition to handle.
        (older, newer.join().expect("the history thread panicked"))
    });
    let mut page = parse(&older?, None)?;
    let newer = parse(&newer?, Some(ts))?;
    page.messages.extend(newer.messages);
    page.messages.sort_by(|a, b| compare_ts(&a.ts, &b.ts));
    page.messages.dedup_by(|a, b| a.ts == b.ts);
    Ok(page)
}

/// Whether a read cursor can be sent as `oldest`: a conversation never
/// read reports `0000000000.000000`, which Slack rejects.
pub fn is_usable_cursor(cursor: &str) -> bool {
    is_ts(cursor) && cursor.contains(|c: char| ('1'..='9').contains(&c))
}

pub fn is_ts(value: &str) -> bool {
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, "0"));
    !seconds.is_empty()
        && seconds.chars().all(|c| c.is_ascii_digit())
        && !fraction.is_empty()
        && fraction.chars().all(|c| c.is_ascii_digit())
}

/// Order two timestamps as numbers: `999.5` is older than `1000.25`,
/// which a comparison of the strings gets wrong.
pub fn compare_ts(a: &str, b: &str) -> std::cmp::Ordering {
    let value = |ts: &str| ts.parse::<f64>().unwrap_or(0.0);
    value(a).total_cmp(&value(b))
}

pub fn parse(body: &Value, last_read: Option<&str>) -> Result<Unread, api::Error> {
    let raw = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| api::Error::Shape {
            method: METHOD.to_string(),
            reason: "no `messages` array".to_string(),
        })?;
    let cursor = last_read.filter(|cursor| is_usable_cursor(cursor));
    let mut messages: Vec<Message> = raw
        .iter()
        .filter_map(message_from)
        .filter(|message| {
            !message
                .subtype
                .as_deref()
                .is_some_and(|s| NOISE.contains(&s))
        })
        .filter(|message| cursor.is_none_or(|cursor| compare_ts(&message.ts, cursor).is_gt()))
        .collect();
    messages.sort_by(|a, b| compare_ts(&a.ts, &b.ts));
    Ok(Unread {
        messages,
        has_more: body
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub fn message_from(raw: &Value) -> Option<Message> {
    let string = |name: &str| raw.get(name).and_then(Value::as_str).map(str::to_string);
    let ts = string("ts").filter(|ts| is_ts(ts))?;
    let titles = |name: &str, keys: &[&str]| -> Vec<String> {
        raw.get(name)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        keys.iter()
                            .find_map(|key| item.get(key).and_then(Value::as_str))
                            .filter(|title| !title.is_empty())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let text = string("text").unwrap_or_default();
    let attachments = titles("attachments", &["title"])
        .into_iter()
        .filter(|title| *title != text)
        .collect();
    Some(Message {
        ts,
        user: string("user"),
        bot: raw
            .pointer("/bot_profile/name")
            .or_else(|| raw.get("username"))
            .and_then(Value::as_str)
            .map(str::to_string),
        text,
        subtype: string("subtype"),
        thread_ts: string("thread_ts"),
        reply_count: raw.get("reply_count").and_then(Value::as_u64).unwrap_or(0),
        files: titles("files", &["title", "name"]),
        attachments,
        reactions: raw
            .get("reactions")
            .and_then(Value::as_array)
            .map(|reactions| reactions.iter().filter_map(reaction_from).collect())
            .unwrap_or_default(),
    })
}

fn reaction_from(raw: &Value) -> Option<Reaction> {
    let users: Vec<String> = raw
        .get("users")
        .and_then(Value::as_array)
        .map(|users| {
            users
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(Reaction {
        name: raw.get("name").and_then(Value::as_str)?.to_string(),
        count: raw
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(users.len() as u64),
        users,
    })
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn body() -> Value {
        json!({
            "ok": true,
            "has_more": true,
            "messages": [
                {"ts": "1700000030.000100", "user": "U1", "text": "third", "thread_ts": "1700000030.000100", "reply_count": 2,
                 "reactions": [{"name": "+1", "users": ["U2", "U7"], "count": 2}, {"name": "eyes", "users": ["U2"]}, {"users": ["U3"]}]},
                {"ts": "1700000020.000100", "subtype": "channel_join", "user": "U2", "text": "joined"},
                {"ts": "1700000010.000100", "bot_id": "B1", "bot_profile": {"name": "deploybot"}, "text": "first",
                 "files": [{"name": "log.txt", "title": "Deploy log"}],
                 "attachments": [{"title": "preview"}, {"fallback": "ignored"}, {"title": "first"}]},
                {"ts": "1699999999.000100", "user": "U1", "text": "already read"},
                {"no_ts": true}
            ]
        })
    }

    #[test]
    fn unread_messages_come_oldest_first_without_noise() {
        let unread = parse(&body(), Some("1700000000.000000")).unwrap();
        let texts: Vec<&str> = unread.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["first", "third"]);
        assert!(unread.has_more);
        let bot = &unread.messages[0];
        assert_eq!(bot.bot.as_deref(), Some("deploybot"));
        assert_eq!(bot.files, ["Deploy log"]);
        assert_eq!(bot.attachments, ["preview"]);
        assert_eq!(unread.messages[1].reply_count, 2);
        assert!(!unread.messages[1].is_reply());
        let reactions = &unread.messages[1].reactions;
        assert_eq!(reactions.len(), 2, "a reaction without a name is dropped");
        assert_eq!((reactions[0].name.as_str(), reactions[0].count), ("+1", 2));
        assert!(reactions[0].by("U7"));
        assert_eq!(reactions[1].count, 1, "the count falls back to the users");
    }

    #[test]
    fn a_reaction_is_put_and_taken_back() {
        let mut message = body()["messages"][0].clone();
        message["reactions"] = json!([{"name": "+1", "users": ["U2"], "count": 1}]);
        let mut message = message_from(&message).unwrap();
        message.react("+1", "U7", true);
        message.react("+1", "U7", true);
        assert_eq!(
            message.reactions[0].count, 2,
            "a second add changes nothing"
        );
        message.react("tada", "U7", true);
        assert_eq!(message.reactions[1].name, "tada");
        message.react("tada", "U7", false);
        message.react("+1", "U2", false);
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(message.reactions[0].users, ["U7"]);
        message.react("eyes", "U7", false);
        assert_eq!(message.reactions.len(), 1);
    }

    #[test]
    fn an_unusable_cursor_keeps_the_whole_page() {
        let unread = parse(&body(), Some("0000000000.000000")).unwrap();
        assert_eq!(unread.messages.len(), 3);
        assert!(!is_usable_cursor("0000000000.000000"));
        assert!(is_usable_cursor("1700000000.000100"));
        assert!(!is_usable_cursor("abc"));
    }

    #[test]
    fn a_page_without_messages_is_a_shape_error() {
        assert!(matches!(
            parse(&json!({"ok": true}), None),
            Err(api::Error::Shape { .. })
        ));
    }

    #[test]
    fn mentions_and_replies_are_recognised() {
        let message = message_from(
            &json!({"ts": "1.000001", "thread_ts": "1.000000", "text": "<@U7|me> hi"}),
        )
        .unwrap();
        assert!(message.is_reply());
        assert!(message.mentions("U7"));
        assert!(!message.mentions("U8"));
    }
}
