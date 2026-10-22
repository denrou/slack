//! Slack's real-time socket (`rtm.connect`), which the screen listens to
//! so it hears of a message as it is posted instead of polling for it. A
//! detached thread holds the socket, keeps it alive and reconnects;
//! `Event::parse` turns a frame into what the screen acts on, and is
//! pure. What an event means for the screen is decided there, not here.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

use crate::api::{self, Client, Socket};

/// How long a read waits, so the loop gets to send its pings.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Slack's own pings come minutes apart (143 seconds in the probe of
/// 2026-10-21): ours tell a dead connection sooner.
const PING_EVERY: Duration = Duration::from_secs(30);
/// Nothing heard for this long, not even a pong: the connection is dead.
const SILENCE: Duration = Duration::from_secs(60);
const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(60);
/// A connection that held this long was a good one: the next failure
/// starts the backoff over.
const STEADY: Duration = Duration::from_secs(60);
/// Failures to open in a row before the screen is told: one is a blip.
const FAILURES_TOLD: u32 = 3;

/// What the socket tells the screen.
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    /// Slack said hello. `again` when it is not the first connection:
    /// whatever came while it was down was missed.
    Up {
        again: bool,
    },
    /// The connection dropped; it is being opened again.
    Down,
    /// It failed to open `FAILURES_TOLD` times in a row, for this reason:
    /// the poll covers meanwhile, and the tries go on. Once per run of
    /// failures.
    Off(String),
    Event(Event),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Message(Posted),
    /// A read cursor or a preference moved, here or in another client:
    /// the counts say what.
    Moved,
    Reaction {
        channel: String,
        ts: String,
        user: String,
        name: String,
        add: bool,
    },
    /// Someone is typing; Slack says it again every few seconds while
    /// they do.
    Typing {
        channel: String,
        thread_ts: Option<String>,
        user: String,
    },
}

/// A message posted, edited or deleted.
#[derive(Debug, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    /// The message's own: for an edit or a deletion, the one changed.
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    /// An edit or a deletion rather than a new message.
    pub changed: bool,
}

impl Posted {
    /// The root of the thread it is a reply in, even one also sent to the
    /// channel, whose part the counts see.
    pub fn reply_to(&self) -> Option<&str> {
        self.thread_ts.as_deref().filter(|root| *root != self.ts)
    }
}

impl Event {
    /// The events the screen acts on; the rest (presence, profiles,
    /// `search_recents`…) are dropped.
    pub fn parse(frame: &Value) -> Option<Event> {
        let text =
            |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        match frame.get("type")?.as_str()? {
            "message" => {
                let channel = text(frame, "channel")?;
                let subtype = frame.get("subtype").and_then(Value::as_str);
                let posted = match subtype {
                    Some("message_changed") => {
                        let message = frame.get("message")?;
                        // A link's preview, or a bot's attachments: a full
                        // reload each would be too many, the next shows it.
                        let same_text = frame
                            .get("previous_message")
                            .and_then(|previous| previous.get("text"))
                            .is_some_and(|before| Some(before) == message.get("text"));
                        if same_text {
                            return None;
                        }
                        Posted {
                            channel,
                            ts: text(message, "ts")?,
                            thread_ts: text(message, "thread_ts"),
                            user: text(message, "user"),
                            changed: true,
                        }
                    }
                    Some("message_deleted") => {
                        let previous = frame.get("previous_message");
                        Posted {
                            channel,
                            ts: text(frame, "deleted_ts")?,
                            thread_ts: previous.and_then(|message| text(message, "thread_ts")),
                            user: previous.and_then(|message| text(message, "user")),
                            changed: true,
                        }
                    }
                    // The root's new reply count; the reply comes on its own.
                    Some("message_replied") => return None,
                    _ => Posted {
                        channel,
                        ts: text(frame, "ts")?,
                        thread_ts: text(frame, "thread_ts"),
                        user: text(frame, "user"),
                        changed: false,
                    },
                };
                Some(Event::Message(posted))
            }
            "channel_marked" | "group_marked" | "im_marked" | "mpim_marked" | "thread_marked"
            | "pref_change" => Some(Event::Moved),
            kind @ ("reaction_added" | "reaction_removed") => {
                let item = frame.get("item")?;
                Some(Event::Reaction {
                    channel: text(item, "channel")?,
                    ts: text(item, "ts")?,
                    user: text(frame, "user")?,
                    name: text(frame, "reaction")?,
                    add: kind == "reaction_added",
                })
            }
            "user_typing" => Some(Event::Typing {
                channel: text(frame, "channel")?,
                thread_ts: text(frame, "thread_ts"),
                user: text(frame, "user")?,
            }),
            _ => None,
        }
    }
}

/// Listen on Slack's socket from a thread of its own, for as long as the
/// screen holds the receiver. `live` says whether the socket is up, for
/// the poll to slow down meanwhile. Detached, as the poll is.
pub fn listen(client: Client, live: Arc<AtomicBool>) -> Receiver<Signal> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || keep_listening(&client, &live, &sender));
    receiver
}

fn keep_listening(client: &Client, live: &AtomicBool, sender: &Sender<Signal>) {
    let mut again = false;
    let mut wait = FIRST_WAIT;
    let mut failures = 0;
    loop {
        let began = Instant::now();
        match client.socket() {
            Ok(socket) => {
                failures = 0;
                let ended = listen_on(socket, live, sender, again);
                live.store(false, Ordering::Relaxed);
                if ended == Ended::Closed || sender.send(Signal::Down).is_err() {
                    return;
                }
                again = true;
            }
            // The poll reports a dead session; another try would not help.
            Err(api::Error::Auth { .. }) => return,
            // The poll covers until the socket is back.
            Err(err) => {
                failures += 1;
                if failures == FAILURES_TOLD && sender.send(Signal::Off(err.to_string())).is_err() {
                    return;
                }
            }
        }
        if began.elapsed() >= STEADY {
            wait = FIRST_WAIT;
        }
        thread::sleep(wait);
        wait = (wait * 2).min(MAX_WAIT);
    }
}

#[derive(PartialEq, Eq)]
enum Ended {
    /// The connection is gone: open another.
    Dropped,
    /// The screen is gone: stop.
    Closed,
}

fn listen_on(mut socket: Socket, live: &AtomicBool, sender: &Sender<Signal>, again: bool) -> Ended {
    if set_read_timeout(&socket).is_err() {
        return Ended::Dropped;
    }
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    let mut id = 0u64;
    loop {
        if heard.elapsed() >= SILENCE {
            return Ended::Dropped;
        }
        if pinged.elapsed() >= PING_EVERY {
            id += 1;
            let ping = json!({"id": id, "type": "ping"}).to_string();
            if socket.send(Message::text(ping)).is_err() {
                return Ended::Dropped;
            }
            pinged = Instant::now();
        }
        let text = match socket.read() {
            Ok(Message::Text(text)) => text,
            // Slack's pings are answered by tungstenite itself.
            Ok(Message::Close(_)) => return Ended::Dropped,
            Ok(_) => {
                heard = Instant::now();
                continue;
            }
            Err(tungstenite::Error::Io(err))
                if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(_) => return Ended::Dropped,
        };
        heard = Instant::now();
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let signal = match frame.get("type").and_then(Value::as_str) {
            Some("hello") => {
                live.store(true, Ordering::Relaxed);
                Signal::Up { again }
            }
            // Slack is about to close it, and says so.
            Some("goodbye") => return Ended::Dropped,
            _ => match Event::parse(&frame) {
                Some(event) => Signal::Event(event),
                None => continue,
            },
        };
        if sender.send(signal).is_err() {
            return Ended::Closed;
        }
    }
}

fn set_read_timeout(socket: &Socket) -> std::io::Result<()> {
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(READ_TIMEOUT)),
        MaybeTlsStream::Rustls(stream) => stream.get_ref().set_read_timeout(Some(READ_TIMEOUT)),
        _ => Err(ErrorKind::Unsupported.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted(channel: &str, ts: &str, thread_ts: Option<&str>, user: &str) -> Posted {
        Posted {
            channel: channel.to_string(),
            ts: ts.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            user: Some(user.to_string()),
            changed: false,
        }
    }

    #[test]
    fn a_message_is_read_with_its_thread() {
        let frame =
            json!({"type": "message", "channel": "C1", "user": "U1", "text": "hi", "ts": "2.0"});
        assert_eq!(
            Event::parse(&frame),
            Some(Event::Message(posted("C1", "2.0", None, "U1")))
        );

        let reply = json!({"type": "message", "channel": "C1", "user": "U1", "ts": "3.0", "thread_ts": "2.0"});
        let Some(Event::Message(reply)) = Event::parse(&reply) else {
            panic!("a reply is a message");
        };
        assert_eq!(reply.reply_to(), Some("2.0"));

        let broadcast = json!({"type": "message", "subtype": "thread_broadcast", "channel": "C1", "user": "U1", "ts": "4.0", "thread_ts": "2.0"});
        let Some(Event::Message(broadcast)) = Event::parse(&broadcast) else {
            panic!("a broadcast is a message");
        };
        assert_eq!(broadcast.reply_to(), Some("2.0"), "in the thread too");

        let root = json!({"type": "message", "channel": "C1", "user": "U1", "ts": "2.0", "thread_ts": "2.0"});
        let Some(Event::Message(root)) = Event::parse(&root) else {
            panic!("a root is a message");
        };
        assert_eq!(root.reply_to(), None);
    }

    #[test]
    fn an_edit_or_a_deletion_names_the_message_it_changed() {
        let edit = json!({
            "type": "message", "subtype": "message_changed", "channel": "C1", "ts": "9.0",
            "message": {"user": "U1", "text": "fixed", "ts": "3.0", "thread_ts": "2.0"}
        });
        let mut expected = posted("C1", "3.0", Some("2.0"), "U1");
        expected.changed = true;
        assert_eq!(Event::parse(&edit), Some(Event::Message(expected)));

        let unfurl = json!({
            "type": "message", "subtype": "message_changed", "channel": "C1", "ts": "9.0",
            "message": {"user": "U1", "text": "see https://x.y", "ts": "3.0", "attachments": [{}]},
            "previous_message": {"user": "U1", "text": "see https://x.y", "ts": "3.0"}
        });
        assert_eq!(Event::parse(&unfurl), None, "the same text: a preview");

        let deletion = json!({
            "type": "message", "subtype": "message_deleted", "channel": "C1", "ts": "9.0",
            "deleted_ts": "3.0", "previous_message": {"user": "U1", "ts": "3.0"}
        });
        let mut expected = posted("C1", "3.0", None, "U1");
        expected.changed = true;
        assert_eq!(Event::parse(&deletion), Some(Event::Message(expected)));

        let replied =
            json!({"type": "message", "subtype": "message_replied", "channel": "C1", "ts": "9.0"});
        assert_eq!(Event::parse(&replied), None);
    }

    #[test]
    fn cursors_reactions_typing_and_the_rest() {
        for kind in [
            "channel_marked",
            "im_marked",
            "mpim_marked",
            "group_marked",
            "thread_marked",
        ] {
            let frame = json!({"type": kind, "channel": "D1", "ts": "1.0"});
            assert_eq!(Event::parse(&frame), Some(Event::Moved), "{kind}");
        }
        let frame = json!({
            "type": "reaction_removed", "user": "U2", "reaction": "eyes",
            "item": {"type": "message", "channel": "C1", "ts": "2.0"}, "event_ts": "5.0"
        });
        assert_eq!(
            Event::parse(&frame),
            Some(Event::Reaction {
                channel: "C1".to_string(),
                ts: "2.0".to_string(),
                user: "U2".to_string(),
                name: "eyes".to_string(),
                add: false,
            })
        );
        let frame = json!({"type": "user_typing", "channel": "D1", "user": "U2"});
        assert_eq!(
            Event::parse(&frame),
            Some(Event::Typing {
                channel: "D1".to_string(),
                thread_ts: None,
                user: "U2".to_string(),
            })
        );
        // A reaction on a file has no channel: nothing on the screen shows it.
        let file = json!({"type": "reaction_added", "user": "U2", "reaction": "eyes", "item": {"type": "file", "file": "F1"}});
        assert_eq!(Event::parse(&file), None);
        for frame in [
            json!({"type": "search_recents"}),
            json!({"type": "presence_change", "user": "U1"}),
            json!({"reply_to": 1, "type": "pong"}),
            json!({"ok": true}),
        ] {
            assert_eq!(Event::parse(&frame), None, "{frame}");
        }
    }
}
