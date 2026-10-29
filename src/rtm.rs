//! Slack's real-time socket (`rtm.connect`), which `slack watch` listens
//! to so it hears of a message as it is posted, whether or not it was
//! read since. A thread of its own holds the socket, keeps it alive and
//! reconnects; `Posted::parse` turns a frame into a message, and is pure.
//! Brought back from v0.18.1, where the screen used it, trimmed to new
//! messages.

use std::io::ErrorKind;
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
/// Failures to open in a row before the listener is told: one is a blip.
const FAILURES_TOLD: u32 = 3;

/// What the socket tells the listener.
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    /// Slack said hello. `again` when it is not the first connection:
    /// whatever came while it was down was missed.
    Up {
        again: bool,
    },
    /// The connection dropped; it is being opened again.
    Down,
    /// It failed to open `FAILURES_TOLD` times in a row, for this reason;
    /// the tries go on. Once per run of failures.
    Off(String),
    /// Slack refused the session: no try will do better, the thread ends.
    Dead(String),
    Message(Posted),
}

/// A new message from a person: no edit, no deletion, no bot, no join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: String,
    /// The poster's own workspace, which Slack gives for a shared
    /// channel's guests (`user_team`, else `team`).
    pub team: Option<String>,
    pub text: String,
}

/// The subtypes that are a person writing; the others are edits,
/// deletions, joins, topics, bots.
const WRITTEN: &[&str] = &["thread_broadcast", "file_share", "me_message"];

impl Posted {
    /// The root of the thread it is a reply in, even one also sent to the
    /// channel.
    pub fn reply_to(&self) -> Option<&str> {
        self.thread_ts.as_deref().filter(|root| *root != self.ts)
    }

    pub fn parse(frame: &Value) -> Option<Posted> {
        let text = |key: &str| frame.get(key).and_then(Value::as_str).map(str::to_string);
        if frame.get("type")?.as_str()? != "message"
            || frame.get("bot_id").is_some_and(|bot| !bot.is_null())
            || frame.get("hidden").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        if let Some(subtype) = frame.get("subtype").and_then(Value::as_str)
            && !WRITTEN.contains(&subtype)
        {
            return None;
        }
        Some(Posted {
            channel: text("channel")?,
            ts: text("ts")?,
            thread_ts: text("thread_ts"),
            user: text("user")?,
            team: text("user_team").or_else(|| text("team")),
            text: text("text").unwrap_or_default(),
        })
    }
}

/// Listen on Slack's socket from a thread of its own, for as long as the
/// receiver is held.
pub fn listen(client: Client) -> Receiver<Signal> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || keep_listening(&client, &sender));
    receiver
}

fn keep_listening(client: &Client, sender: &Sender<Signal>) {
    let mut again = false;
    let mut wait = FIRST_WAIT;
    let mut failures = 0;
    loop {
        let began = Instant::now();
        match client.socket() {
            Ok(socket) => {
                failures = 0;
                if listen_on(socket, sender, again) == Ended::Closed
                    || sender.send(Signal::Down).is_err()
                {
                    return;
                }
                again = true;
            }
            Err(err @ api::Error::Auth { .. }) => {
                let _ = sender.send(Signal::Dead(err.to_string()));
                return;
            }
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
    /// The listener is gone: stop.
    Closed,
}

fn listen_on(mut socket: Socket, sender: &Sender<Signal>, again: bool) -> Ended {
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
            Some("hello") => Signal::Up { again },
            // Slack is about to close it, and says so.
            Some("goodbye") => return Ended::Dropped,
            _ => match Posted::parse(&frame) {
                Some(posted) => Signal::Message(posted),
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

    #[test]
    fn a_message_is_read_with_its_thread_and_team() {
        let frame = json!({"type": "message", "channel": "D1", "user": "U1", "text": "hi", "ts": "2.0", "team": "T1"});
        assert_eq!(
            Posted::parse(&frame),
            Some(Posted {
                channel: "D1".to_string(),
                ts: "2.0".to_string(),
                thread_ts: None,
                user: "U1".to_string(),
                team: Some("T1".to_string()),
                text: "hi".to_string(),
            })
        );

        let reply = json!({"type": "message", "channel": "C1", "user": "U1", "ts": "3.0", "thread_ts": "2.0", "team": "T1", "user_team": "T9"});
        let reply = Posted::parse(&reply).expect("a reply is a message");
        assert_eq!(reply.reply_to(), Some("2.0"));
        assert_eq!(reply.team.as_deref(), Some("T9"), "the poster's own team");

        let broadcast = json!({"type": "message", "subtype": "thread_broadcast", "channel": "C1", "user": "U1", "ts": "4.0", "thread_ts": "2.0"});
        let broadcast = Posted::parse(&broadcast).expect("a broadcast is a message");
        assert_eq!(broadcast.reply_to(), Some("2.0"), "in the thread too");

        let root = json!({"type": "message", "channel": "C1", "user": "U1", "ts": "2.0", "thread_ts": "2.0"});
        assert_eq!(Posted::parse(&root).unwrap().reply_to(), None);

        let file = json!({"type": "message", "subtype": "file_share", "channel": "D1", "user": "U1", "ts": "5.0"});
        assert!(Posted::parse(&file).is_some(), "a file with no text");
    }

    #[test]
    fn edits_deletions_bots_and_the_rest_are_not_messages() {
        for frame in [
            json!({"type": "message", "subtype": "message_changed", "channel": "C1", "ts": "9.0",
                   "message": {"user": "U1", "text": "fixed", "ts": "3.0"}}),
            json!({"type": "message", "subtype": "message_deleted", "channel": "C1", "ts": "9.0", "deleted_ts": "3.0"}),
            json!({"type": "message", "subtype": "message_replied", "channel": "C1", "ts": "9.0"}),
            json!({"type": "message", "subtype": "channel_join", "channel": "C1", "user": "U1", "ts": "9.0"}),
            json!({"type": "message", "subtype": "bot_message", "channel": "C1", "bot_id": "B1", "ts": "9.0"}),
            json!({"type": "message", "channel": "D1", "user": "U1", "bot_id": "B1", "ts": "9.0", "text": "an app as a user"}),
            json!({"type": "message", "channel": "D1", "ts": "9.0", "text": "no one"}),
            json!({"type": "message", "channel": "D1", "user": "U1", "ts": "9.0", "hidden": true}),
            json!({"type": "user_typing", "channel": "D1", "user": "U2"}),
            json!({"type": "presence_change", "user": "U1"}),
            json!({"reply_to": 1, "type": "pong"}),
        ] {
            assert_eq!(Posted::parse(&frame), None, "{frame}");
        }
    }
}
