//! A conversation's messages from `conversations.history`: the unread
//! part from its read cursor, or the recent history without one. Slack
//! answers newest first, so with a limit the page holds the most recent
//! messages; `has_more` says older ones exist.

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::api::{self, Client};

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
    pub files: Vec<File>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
}

/// A file posted with a message, printed as its title alone, `--json`
/// included: the URLs are fetched with the session cookie, and are of no
/// use without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// The title Slack shows, else the file name.
    pub title: String,
}

impl File {
    fn from(raw: &Value) -> Option<File> {
        let string = |name: &str| {
            raw.get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Some(File {
            title: string("title").or_else(|| string("name"))?,
        })
    }
}

impl Serialize for File {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.title)
    }
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
        files: raw
            .get("files")
            .and_then(Value::as_array)
            .map(|files| files.iter().filter_map(File::from).collect())
            .unwrap_or_default(),
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
        let titles: Vec<&str> = bot.files.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, ["Deploy log"]);
        assert_eq!(bot.attachments, ["preview"]);
        assert_eq!(unread.messages[1].reply_count, 2);
        assert!(!unread.messages[1].is_reply());
        let reactions = &unread.messages[1].reactions;
        assert_eq!(reactions.len(), 2, "a reaction without a name is dropped");
        assert_eq!((reactions[0].name.as_str(), reactions[0].count), ("+1", 2));
        assert!(reactions[0].users.iter().any(|user| user == "U7"));
        assert_eq!(reactions[1].count, 1, "the count falls back to the users");
    }

    #[test]
    fn a_file_prints_as_its_title() {
        let message = message_from(&json!({
            "ts": "1700000010.000100",
            "text": "",
            "files": [
                {"id": "F1", "name": "shot.png", "title": "", "mimetype": "image/png",
                 "url_private": "https://files.slack.com/files-pri/T1-F1/shot.png",
                 "thumb_360": "https://files.slack.com/t/360.png",
                 "thumb_720": "https://files.slack.com/t/720.png",
                 "permalink": "https://acme.slack.com/files/U1/F1/shot.png"},
                {"id": "F2", "name": "party.gif", "mimetype": "image/gif",
                 "thumb_360": "https://files.slack.com/t/gif.png"},
                {"id": "F3", "name": "notes.pdf", "mimetype": "application/pdf"},
                {"id": "F4", "mode": "tombstone"}
            ]
        }))
        .unwrap();
        let [shot, _, _] = message.files.as_slice() else {
            panic!(
                "a file without a name or title is dropped: {:?}",
                message.files
            );
        };
        assert_eq!(
            shot.title, "shot.png",
            "an empty title falls back to the name"
        );
        assert_eq!(
            serde_json::to_value(&message).unwrap()["files"],
            json!(["shot.png", "party.gif", "notes.pdf"]),
            "no URL in the JSON"
        );
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
