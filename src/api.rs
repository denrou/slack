//! The one transport to Slack: a form POST to `<workspace url>/api/<method>`
//! authenticated the way the web client does it, with the `xoxc` token in
//! the body and the `xoxd` session cookie. Commands never see the headers.

use std::io::{ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use tungstenite::HandshakeError;
use tungstenite::client::IntoClientRequest;
use tungstenite::error::UrlError;
use tungstenite::http::{HeaderValue, Uri};
use tungstenite::stream::MaybeTlsStream;

use crate::auth::capture;

/// What `encodeURIComponent` leaves unescaped: the cookie is sent exactly
/// as the browser sends it.
const COOKIE_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// Error codes that mean the saved session is dead, whatever the method.
const AUTH_ERRORS: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "token_expired",
    "token_revoked",
    "account_inactive",
];

const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; slack-cli/",
    env!("CARGO_PKG_VERSION"),
    ")"
);
const TIMEOUT: Duration = Duration::from_secs(30);
/// A file can be a video: longer than a call, and the screen says it is
/// downloading meanwhile.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const RATE_LIMIT_RETRIES: u32 = 2;
/// Cap on an honoured Retry-After: a status-bar poll must not hang for minutes.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Slack refused the saved session ({code}); run `slack login`")]
    Auth { code: String },
    #[error("Slack answered {method} with {code}")]
    Slack { method: String, code: String },
    #[error("Slack answered {method} with HTTP {status}")]
    Http { method: String, status: u16 },
    #[error("could not reach Slack for {method}: {source}")]
    Transport {
        method: String,
        #[source]
        source: ureq::Error,
    },
    #[error("unexpected response to {method}: {reason}")]
    Shape { method: String, reason: String },
    /// Never the URL: it carries the session's ticket.
    #[error("could not open Slack's socket: {reason}")]
    Socket { reason: String },
    #[error("Slack did not hand over the file: {reason}")]
    File { reason: String },
}

/// Slack's real-time socket, as `Client::socket` opens it.
pub type Socket = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

/// A connection to the socket's host with timeouts, which the handshake
/// keeps: a middlebox that never answers would hold it forever otherwise.
fn open_stream(uri: &Uri) -> std::io::Result<TcpStream> {
    let host = uri
        .host()
        .ok_or_else(|| std::io::Error::from(ErrorKind::InvalidInput))?;
    let mut failed = std::io::Error::from(ErrorKind::NotFound);
    for addr in (host, uri.port_u16().unwrap_or(443)).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(TIMEOUT))?;
                stream.set_write_timeout(Some(TIMEOUT))?;
                return Ok(stream);
            }
            Err(err) => failed = err,
        }
    }
    Err(failed)
}

/// Why the socket did not open, without the URL tungstenite would quote.
fn socket_reason(err: &tungstenite::Error) -> String {
    match err {
        tungstenite::Error::Url(UrlError::UnableToConnect(_)) => "could not connect".to_string(),
        other => other.to_string(),
    }
}

pub struct Credentials<'a> {
    pub xoxc: &'a str,
    pub xoxd: &'a str,
}

/// Who a set of credentials acts as, per `auth.test`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Identity {
    pub team_id: String,
    pub team: String,
    pub user_id: String,
    pub user: String,
    pub url: String,
}

/// Cloned for the screen's poller, which runs detached (`tui::poll`).
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    base_url: String,
    xoxc: String,
    cookie: String,
}

impl Client {
    pub fn new(workspace_url: &str, credentials: &Credentials) -> Client {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build();
        Client {
            agent: ureq::Agent::new_with_config(config),
            base_url: workspace_url.trim_end_matches('/').to_string(),
            xoxc: credentials.xoxc.to_string(),
            cookie: cookie_header(credentials.xoxd),
        }
    }

    /// Call `method` with `params` and return its `ok: true` payload.
    pub fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, Error> {
        let url = format!("{}/api/{method}", self.base_url);
        let mut form = Vec::with_capacity(params.len() + 1);
        form.push(("token", self.xoxc.as_str()));
        form.extend_from_slice(params);

        let mut retries = 0;
        loop {
            let mut response = self
                .agent
                .post(&url)
                .header("Cookie", &self.cookie)
                .header("Origin", "https://app.slack.com")
                .header("User-Agent", USER_AGENT)
                .send_form(form.iter().copied())
                .map_err(|source| Error::Transport {
                    method: method.to_string(),
                    source,
                })?;

            let status = response.status().as_u16();
            if status == 429 && retries < RATE_LIMIT_RETRIES {
                retries += 1;
                std::thread::sleep(retry_after(&response));
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(Error::Http {
                    method: method.to_string(),
                    status,
                });
            }
            let body: Value = response
                .body_mut()
                .read_json()
                .map_err(|err| Error::Shape {
                    method: method.to_string(),
                    reason: format!("not JSON: {err}"),
                })?;
            return envelope(method, body);
        }
    }

    /// Slack's real-time socket for this session: `rtm.connect` names it,
    /// and it opens with the cookie, as the web client opens its own.
    pub fn socket(&self) -> Result<Socket, Error> {
        const METHOD: &str = "rtm.connect";
        let body = self.call(METHOD, &[])?;
        let url = body
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Shape {
                method: METHOD.to_string(),
                reason: "missing `url`".to_string(),
            })?;
        if !capture::is_slack_socket_url(url) {
            return Err(Error::Shape {
                method: METHOD.to_string(),
                reason: "a socket outside slack.com".to_string(),
            });
        }
        let socket_error = |reason: String| Error::Socket { reason };
        let mut request = url
            .into_client_request()
            .map_err(|err| socket_error(socket_reason(&err)))?;
        let headers = request.headers_mut();
        for (name, value) in [
            ("Cookie", self.cookie.as_str()),
            ("Origin", "https://app.slack.com"),
            ("User-Agent", USER_AGENT),
        ] {
            let value = HeaderValue::from_str(value)
                .map_err(|_| socket_error(format!("a `{name}` header that is not text")))?;
            headers.insert(name, value);
        }
        // Deliberate: not `tungstenite::connect`, which has no timeout and
        // follows redirects, the cookie with them, to a host
        // `is_slack_socket_url` never saw.
        let stream = open_stream(request.uri())
            .map_err(|err| socket_error(format!("could not connect: {err}")))?;
        let (socket, _response) =
            tungstenite::client_tls(request, stream).map_err(|err| match err {
                HandshakeError::Failure(err) => socket_error(socket_reason(&err)),
                HandshakeError::Interrupted(_) => {
                    socket_error("the handshake timed out".to_string())
                }
            })?;
        Ok(socket)
    }

    /// Write a file Slack hosts into `sink`, with the session cookie as
    /// the web client fetches it, and return its size.
    pub fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64, Error> {
        const WHAT: &str = "the file";
        let refused = |reason: &str| Error::File {
            reason: reason.to_string(),
        };
        if !capture::is_slack_workspace_url(url) {
            return Err(refused("it is not on slack.com"));
        }
        // Deliberate: no redirect is followed, since the cookie would go
        // along to a host `is_slack_workspace_url` never saw.
        let mut response = self
            .agent
            .get(url)
            .config()
            .max_redirects(0)
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .build()
            .header("Cookie", &self.cookie)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|source| Error::Transport {
                method: WHAT.to_string(),
                source,
            })?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(refused(
                "it sent the download elsewhere, which usually means the session expired (`slack whoami` checks it)",
            ));
        }
        if !(200..300).contains(&status) {
            return Err(Error::Http {
                method: WHAT.to_string(),
                status,
            });
        }
        if response.body().mime_type() == Some("text/html") {
            return Err(refused(
                "a web page came back instead, which usually means the session expired (`slack whoami` checks it)",
            ));
        }
        std::io::copy(&mut response.body_mut().as_reader(), sink)
            .map_err(|err| refused(&format!("the download broke off: {err}")))
    }

    pub fn auth_test(&self) -> Result<Identity, Error> {
        let body = self.call("auth.test", &[])?;
        let field = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| Error::Shape {
                    method: "auth.test".to_string(),
                    reason: format!("missing `{name}`"),
                })
        };
        Ok(Identity {
            team_id: field("team_id")?,
            team: field("team")?,
            user_id: field("user_id")?,
            user: field("user")?,
            url: field("url")?,
        })
    }
}

fn cookie_header(xoxd: &str) -> String {
    format!("d={}", utf8_percent_encode(xoxd, COOKIE_VALUE))
}

fn retry_after(response: &ureq::http::Response<ureq::Body>) -> Duration {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(2), Duration::from_secs)
        .min(MAX_RETRY_WAIT)
}

fn envelope(method: &str, body: Value) -> Result<Value, Error> {
    match body.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(body),
        Some(false) => {
            let code = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_string();
            if AUTH_ERRORS.contains(&code.as_str()) {
                Err(Error::Auth { code })
            } else {
                Err(Error::Slack {
                    method: method.to_string(),
                    code,
                })
            }
        }
        None => Err(Error::Shape {
            method: method.to_string(),
            reason: "no `ok` field".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn cookie_is_encoded_like_the_browser_does() {
        assert_eq!(
            cookie_header("xoxd-ab/c+d=e f"),
            "d=xoxd-ab%2Fc%2Bd%3De%20f"
        );
    }

    #[test]
    fn a_socket_error_never_quotes_the_ticket() {
        let err = tungstenite::Error::Url(UrlError::UnableToConnect(
            "wss://wss-primary.slack.com/websocket/x?ticket=secret".to_string(),
        ));
        assert_eq!(socket_reason(&err), "could not connect");
    }

    #[test]
    fn ok_payload_is_returned() {
        let body = json!({"ok": true, "user": "denis"});
        assert_eq!(envelope("auth.test", body.clone()).unwrap(), body);
    }

    #[test]
    fn auth_codes_become_auth_errors() {
        for code in AUTH_ERRORS {
            let err = envelope("auth.test", json!({"ok": false, "error": code})).unwrap_err();
            assert!(
                matches!(err, Error::Auth { code: c } if c == *code),
                "{code}"
            );
        }
    }

    #[test]
    fn other_codes_name_the_method() {
        let err = envelope(
            "chat.postMessage",
            json!({"ok": false, "error": "channel_not_found"}),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Slack { method, code }
            if method == "chat.postMessage" && code == "channel_not_found"));
    }

    #[test]
    fn missing_ok_is_a_shape_error() {
        assert!(matches!(
            envelope("client.counts", json!({"channels": []})),
            Err(Error::Shape { .. })
        ));
    }
}
