//! Followed threads with unread replies: `subscriptions.thread.getView`,
//! the endpoint behind Slack's own Threads view. Undocumented, so nothing
//! about its shape is trusted: an unexpected page is reported and the view
//! falls back to its one-line summary rather than guessing.
//!
//! Observed on a live workspace (slackcli, 2026-10-10): the view lists the
//! threads with unread replies first, then the read ones, newest activity
//! first, ten per page. The next page is asked for with `current_ts` set to
//! the last thread's `root_msg.latest_reply`, an exclusive upper bound; the
//! response's own `max_ts` is newer than every thread on the page and
//! returns the same page. A page holding a read thread is the last one
//! worth reading.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::history::{Message, compare_ts, is_usable_cursor, message_from};

const METHOD: &str = "subscriptions.thread.getView";
/// Five pages bound the cost of a long backlog; past that the list is
/// reported incomplete.
pub const MAX_PAGES: usize = 5;
pub const REPLY_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    pub channel: String,
    pub thread_ts: String,
    pub root: Message,
    /// Oldest first, capped at the reply limit.
    pub unread_replies: Vec<Message>,
    /// The cap dropped replies.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
}

pub struct ThreadList {
    pub items: Vec<Thread>,
    /// The list is incomplete: the page cap, an unreadable entry or a
    /// failing later page.
    pub has_more: bool,
}

/// One page of the view, normalised.
pub struct Page {
    pub threads: Vec<Thread>,
    /// `current_ts` for the next page, when the page gives one.
    pub next_cursor: Option<String>,
    /// No later page can hold an unread thread.
    pub done: bool,
    /// Entries that are threads with nothing unread.
    pub read: usize,
    /// Entries that could not be read as a thread at all.
    pub malformed: usize,
}

enum Entry {
    Unread(Box<Thread>),
    Read,
    Malformed,
}

/// The followed threads with unread replies, following the view's pages up
/// to `MAX_PAGES`. None when the view is unusable before anything was read,
/// so the caller falls back to the summary; after that, what was read,
/// marked incomplete. Warnings go to stderr; only a dead session is an error.
pub fn fetch(client: &Client, limit: usize) -> Result<Option<ThreadList>, api::Error> {
    let mut items: Vec<Thread> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut incomplete = false;
    for page_number in 1..=MAX_PAGES {
        let params: Vec<(&str, &str)> = cursor
            .iter()
            .map(|cursor| ("current_ts", cursor.as_str()))
            .collect();
        let body = match client.call(METHOD, &params) {
            Ok(body) => body,
            Err(err @ api::Error::Auth { .. }) => return Err(err),
            Err(err) => {
                crate::warn::warn(format!(
                    "could not read the thread view (page {page_number}): {err}"
                ));
                return Ok(partial(page_number, items));
            }
        };
        let Some(page) = parse_page(&body, limit) else {
            crate::warn::warn(format!(
                "the thread view answered in an unexpected shape (page {page_number})"
            ));
            return Ok(partial(page_number, items));
        };
        if page.malformed > 0 {
            crate::warn::warn(format!(
                "{} entries of the thread view could not be read",
                page.malformed
            ));
            incomplete = true;
            // Not one entry of the page is a thread: the shape has changed.
            if page.threads.is_empty() && page.read == 0 {
                return Ok(partial(page_number, items));
            }
        }
        for thread in page.threads {
            // Two pages can repeat a thread.
            if seen.insert((thread.channel.clone(), thread.thread_ts.clone())) {
                items.push(thread);
            }
        }
        if page.done {
            return Ok(Some(ThreadList {
                items,
                has_more: incomplete,
            }));
        }
        match page.next_cursor {
            Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
            // No cursor, or one that did not move: the next call would repeat this page.
            _ => {
                return Ok(Some(ThreadList {
                    items,
                    has_more: true,
                }));
            }
        }
    }
    Ok(Some(ThreadList {
        items,
        has_more: true,
    }))
}

/// Nothing read yet means the view is unusable; otherwise what was read,
/// marked incomplete.
fn partial(page_number: usize, items: Vec<Thread>) -> Option<ThreadList> {
    (page_number > 1).then_some(ThreadList {
        items,
        has_more: true,
    })
}

/// One page of the view as unread threads. None when the response has no
/// `threads` array; every entry that is not a thread with a root message,
/// a channel and a list of replies counts as malformed.
pub fn parse_page(body: &Value, limit: usize) -> Option<Page> {
    let raw = body.get("threads")?.as_array()?;
    let mut page = Page {
        threads: Vec::new(),
        next_cursor: None,
        done: false,
        read: 0,
        malformed: 0,
    };
    for entry in raw {
        match normalise(entry, limit) {
            Entry::Unread(thread) => page.threads.push(*thread),
            Entry::Read => page.read += 1,
            Entry::Malformed => page.malformed += 1,
        }
    }
    let last = raw.last();
    let latest_reply = last
        .and_then(|entry| entry.pointer("/root_msg/latest_reply"))
        .and_then(Value::as_str);
    let last_unread = last.and_then(|entry| {
        entry
            .get("unread_replies")?
            .as_array()?
            .last()?
            .get("ts")?
            .as_str()
    });
    page.next_cursor = [latest_reply, last_unread]
        .into_iter()
        .flatten()
        .find(|cursor| is_usable_cursor(cursor))
        .map(str::to_string);
    page.done = body.get("has_more").and_then(Value::as_bool) != Some(true) || page.read > 0;
    Some(page)
}

fn normalise(raw: &Value, limit: usize) -> Entry {
    let Some(root_raw) = raw.get("root_msg") else {
        return Entry::Malformed;
    };
    let channel = root_raw
        .get("channel")
        .and_then(Value::as_str)
        .filter(|channel| !channel.is_empty());
    let (Some(root), Some(channel)) = (message_from(root_raw), channel) else {
        return Entry::Malformed;
    };
    let replies = match raw.get("unread_replies") {
        // A read thread carries no unread_replies key.
        None | Some(Value::Null) => return Entry::Read,
        Some(Value::Array(replies)) => replies,
        Some(_) => return Entry::Malformed,
    };
    if replies.is_empty() {
        return Entry::Read;
    }
    let mut replies: Vec<Message> = replies.iter().filter_map(message_from).collect();
    if replies.is_empty() {
        return Entry::Malformed;
    }
    replies.sort_by(|a, b| compare_ts(&a.ts, &b.ts));
    let has_more = replies.len() > limit;
    replies.truncate(limit);
    Entry::Unread(Box::new(Thread {
        channel: channel.to_string(),
        thread_ts: root.thread_ts.clone().unwrap_or_else(|| root.ts.clone()),
        root,
        unread_replies: replies,
        has_more,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn unread_thread() -> Value {
        json!({
            "root_msg": {"channel": "C1", "ts": "1700000000.000100", "thread_ts": "1700000000.000100",
                         "latest_reply": "1700000050.000100", "text": "root", "user": "U1"},
            "unread_replies": [
                {"ts": "1700000050.000100", "user": "U2", "text": "second"},
                {"ts": "1700000040.000100", "user": "U3", "text": "first"}
            ]
        })
    }

    #[test]
    fn a_page_separates_unread_read_and_malformed_entries() {
        let body = json!({"ok": true, "has_more": true, "threads": [
            unread_thread(),
            {"root_msg": {"channel": "C2", "ts": "1700000010.000100", "latest_reply": "1700000020.000100", "text": "read"},
             "latest_replies": [{"ts": "1700000020.000100", "text": "x"}]},
            {"nonsense": true}
        ]});
        let page = parse_page(&body, 20).unwrap();
        assert_eq!((page.threads.len(), page.read, page.malformed), (1, 1, 1));
        let thread = &page.threads[0];
        assert_eq!(
            (thread.channel.as_str(), thread.thread_ts.as_str()),
            ("C1", "1700000000.000100")
        );
        let texts: Vec<&str> = thread
            .unread_replies
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, ["first", "second"]);
        assert!(page.done, "a read thread ends the unread section");
        assert_eq!(
            page.next_cursor, None,
            "the malformed last entry gives no cursor"
        );
    }

    #[test]
    fn a_full_page_of_unread_threads_continues_from_the_last_reply() {
        let body = json!({"has_more": true, "threads": [unread_thread()]});
        let page = parse_page(&body, 20).unwrap();
        assert!(!page.done);
        assert_eq!(page.next_cursor.as_deref(), Some("1700000050.000100"));
        let page = parse_page(&json!({"has_more": false, "threads": []}), 20).unwrap();
        assert!(page.done && page.threads.is_empty());
        assert!(parse_page(&json!({"ok": true}), 20).is_none());
    }

    #[test]
    fn the_reply_cap_marks_the_thread_incomplete() {
        let page = parse_page(&json!({"threads": [unread_thread()]}), 1).unwrap();
        let thread = &page.threads[0];
        assert_eq!(thread.unread_replies.len(), 1);
        assert_eq!(thread.unread_replies[0].text, "first");
        assert!(thread.has_more);
    }

    #[test]
    fn a_thread_without_a_channel_or_with_empty_replies_is_not_unread() {
        let page = parse_page(&json!({"threads": [
            {"root_msg": {"ts": "1.000100", "text": "no channel"}, "unread_replies": [{"ts": "2.000100"}]},
            {"root_msg": {"channel": "C1", "ts": "1.000100"}, "unread_replies": []},
            {"root_msg": {"channel": "C1", "ts": "1.000100"}, "unread_replies": "oops"}
        ]}), 20).unwrap();
        assert_eq!((page.threads.len(), page.read, page.malformed), (0, 1, 2));
    }
}
