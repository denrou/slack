//! A minimal Chrome DevTools Protocol client over one blocking WebSocket:
//! numbered commands with their replies, and the events that arrive in
//! between kept for the caller. Hand-rolled rather than a CDP crate, as in
//! slackcli: the job is to open a page and read two values.

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::error::ProtocolError;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one socket read waits before reporting "nothing yet".
const READ_SLICE: Duration = Duration::from_millis(100);
/// Events kept between two drains; Slack's client is chatty while booting.
const EVENT_BUFFER: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the browser connection closed")]
    Closed,
    #[error("CDP {method} failed: {message}")]
    Command { method: String, message: String },
    #[error("CDP {method} got no reply within {}s", .timeout.as_secs())]
    Timeout { method: String, timeout: Duration },
    #[error("browser connection error: {0}")]
    Socket(#[from] tungstenite::Error),
}

pub struct Event {
    pub method: String,
    pub params: Value,
}

pub struct Session {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: VecDeque<Event>,
}

impl Session {
    pub fn connect(url: &str) -> Result<Session, Error> {
        let (mut ws, _response) = tungstenite::connect(url)?;
        if let MaybeTlsStream::Plain(stream) = ws.get_mut() {
            stream
                .set_read_timeout(Some(READ_SLICE))
                .map_err(tungstenite::Error::Io)?;
        }
        Ok(Session {
            ws,
            next_id: 1,
            events: VecDeque::new(),
        })
    }

    /// Send a command and wait for its reply, keeping the events seen meanwhile.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_within(method, params, COMMAND_TIMEOUT)
    }

    /// Every event received so far, including what is readable right now.
    pub fn take_events(&mut self) -> Result<Vec<Event>, Error> {
        while let Some(message) = self.read_once()? {
            self.keep(message);
        }
        Ok(self.events.drain(..).collect())
    }

    /// Ask the browser to quit itself: only it can reap its helpers and
    /// release the profile locks. Best effort; the launcher escalates.
    pub fn close_browser(mut self) {
        let _ = self.call_within("Browser.close", json!({}), Duration::from_secs(5));
        let _ = self.ws.close(None);
    }

    fn call_within(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws.send(Message::Text(frame.into()))?;

        let deadline = Instant::now() + timeout;
        loop {
            let Some(message) = self.read_until(deadline)? else {
                return Err(Error::Timeout {
                    method: method.to_string(),
                    timeout,
                });
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                self.keep(message);
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                return Err(Error::Command {
                    method: method.to_string(),
                    message: text,
                });
            }
            return Ok(message.get("result").cloned().unwrap_or_else(|| json!({})));
        }
    }

    fn keep(&mut self, message: Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return; // a reply nobody waits for
        };
        if self.events.len() >= EVENT_BUFFER {
            self.events.pop_front();
        }
        self.events.push_back(Event {
            method: method.to_string(),
            params: message.get("params").cloned().unwrap_or_else(|| json!({})),
        });
    }

    fn read_until(&mut self, deadline: Instant) -> Result<Option<Value>, Error> {
        loop {
            if let Some(message) = self.read_once()? {
                return Ok(Some(message));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    /// One parsed text frame, or None when nothing arrived within READ_SLICE.
    fn read_once(&mut self) -> Result<Option<Value>, Error> {
        loop {
            match self.ws.read() {
                Ok(Message::Text(text)) => match serde_json::from_str::<Value>(text.as_str()) {
                    Ok(value) => return Ok(Some(value)),
                    Err(_) => continue, // not worth tearing the session down
                },
                Ok(Message::Close(_)) => return Err(Error::Closed),
                Ok(_) => continue, // pings are answered by tungstenite itself
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Err(Error::Closed);
                }
                Err(tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => {
                    return Err(Error::Closed);
                }
                Err(err) => return Err(Error::Socket(err)),
            }
        }
    }
}
