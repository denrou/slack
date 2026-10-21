//! The messages of one thread: `conversations.replies`, root first, every
//! reply after it, oldest first.

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::history::{Message, compare_ts, is_usable_cursor, message_from};

const METHOD: &str = "conversations.replies";
const PAGE: usize = 200;
/// A thousand replies is as far as anyone reads in a terminal.
const MAX_PAGES: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct Replies {
    pub root: Message,
    pub replies: Vec<Message>,
    /// The page cap left replies out.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
    /// Your read cursor in the thread, which Slack puts on the root of a
    /// thread you follow.
    #[serde(skip)]
    pub last_read: Option<String>,
}

pub fn fetch(client: &Client, channel: &str, thread_ts: &str) -> Result<Replies, api::Error> {
    let limit = PAGE.to_string();
    let mut messages: Vec<Message> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut has_more = false;
    let mut last_read = None;
    for _ in 0..MAX_PAGES {
        let mut params = vec![("channel", channel), ("ts", thread_ts), ("limit", &limit)];
        if let Some(cursor) = &cursor {
            params.push(("cursor", cursor));
        }
        let body = client.call(METHOD, &params)?;
        let raw = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| shape("no `messages` array"))?;
        if last_read.is_none() {
            last_read = root_last_read(raw, thread_ts);
        }
        messages.extend(raw.iter().filter_map(message_from));
        cursor = body
            .pointer("/response_metadata/next_cursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_string);
        has_more = cursor.is_some();
        if cursor.is_none() {
            break;
        }
    }
    messages.sort_by(|a, b| compare_ts(&a.ts, &b.ts));
    // A page may repeat the root; a message is listed once.
    messages.dedup_by(|a, b| a.ts == b.ts);
    let root_at = messages
        .iter()
        .position(|message| message.ts == thread_ts)
        .ok_or_else(|| shape("the thread's root message is not in the answer"))?;
    let root = messages.remove(root_at);
    Ok(Replies {
        root,
        replies: messages,
        has_more,
        last_read,
    })
}

fn root_last_read(raw: &[Value], thread_ts: &str) -> Option<String> {
    raw.iter()
        .find(|message| message.get("ts").and_then(Value::as_str) == Some(thread_ts))?
        .get("last_read")?
        .as_str()
        .filter(|cursor| is_usable_cursor(cursor))
        .map(str::to_string)
}

fn shape(reason: &str) -> api::Error {
    api::Error::Shape {
        method: METHOD.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_read_cursor_comes_from_the_root_alone() {
        let raw = json!([
            {"ts": "1.000100", "thread_ts": "1.000100", "last_read": "1.000300"},
            {"ts": "1.000200", "thread_ts": "1.000100", "last_read": "1.000900"}
        ]);
        let raw = raw.as_array().unwrap();
        assert_eq!(root_last_read(raw, "1.000100").as_deref(), Some("1.000300"));
        assert_eq!(root_last_read(&raw[1..], "1.000100"), None);
        let never = json!([{"ts": "1.000100", "last_read": "0000000000.000000"}]);
        assert_eq!(root_last_read(never.as_array().unwrap(), "1.000100"), None);
    }
}
