//! The one transport to Slack: a form POST to `<workspace url>/api/<method>`
//! authenticated the way the web client does it, with the `xoxc` token in
//! the body and the `xoxd` session cookie. Commands never see the headers.

use std::fs::File;
use std::io::ErrorKind;
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
/// A file's bytes take longer than an API call: ten minutes, and the time
/// its length takes at 100 kB a second, so a large file on a slow line
/// still ends and a stalled one does not hang for good.
fn upload_timeout(length: u64) -> Duration {
    Duration::from_secs(600 + length / 100_000)
}
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
    /// Never the URL: it is the leave to write the file.
    #[error("the upload failed: {reason}")]
    Upload { reason: String },
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

/// Cloned for the socket's own thread (`rtm::listen`).
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

    /// Sends a file's bytes to the URL `files.getUploadURLExternal` named
    /// for it. Deliberate: neither the token nor the cookie goes with
    /// them. The URL is the leave to write that one file, it is on
    /// another host than the workspace's, and Slack's own example sends
    /// the bytes and a content type, nothing else. No redirect is
    /// followed either: the bytes go to the host checked here or nowhere.
    pub fn upload(&self, url: &str, file: &File) -> Result<(), Error> {
        let upload_error = |reason: String| Error::Upload { reason };
        if !capture::is_slack_upload_url(url) {
            return Err(upload_error("an upload URL outside slack.com".to_string()));
        }
        let length = file
            .metadata()
            .map_err(|err| upload_error(format!("could not read the file: {err}")))?
            .len();
        let response = self
            .agent
            .post(url)
            .config()
            .timeout_global(Some(upload_timeout(length)))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build()
            .header("User-Agent", USER_AGENT)
            .content_type("application/octet-stream")
            .send(file)
            .map_err(|err| upload_error(upload_reason(&err)))?;
        match response.status().as_u16() {
            200 => Ok(()),
            status => Err(upload_error(format!("HTTP {status}"))),
        }
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

/// Why the bytes did not go, without the URL ureq may quote.
fn upload_reason(err: &ureq::Error) -> String {
    match err {
        ureq::Error::Io(err) => err.to_string(),
        ureq::Error::Timeout(_) => "it timed out".to_string(),
        ureq::Error::HostNotFound => "the host was not found".to_string(),
        ureq::Error::ConnectionFailed => "could not connect".to_string(),
        _ => "the request could not be sent".to_string(),
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
    fn an_upload_error_never_quotes_the_url() {
        let err = ureq::Error::BadUri("https://files.slack.com/upload/v1/secret".to_string());
        assert_eq!(upload_reason(&err), "the request could not be sent");
    }

    #[test]
    fn a_larger_file_gets_more_time() {
        assert_eq!(upload_timeout(500_000), Duration::from_secs(605));
        assert_eq!(upload_timeout(1 << 30), Duration::from_secs(11_337));
    }

    #[test]
    fn an_upload_goes_to_slack_or_nowhere() {
        let client = Client::new(
            "https://t.slack.com",
            &Credentials {
                xoxc: "xoxc-test",
                xoxd: "xoxd-test",
            },
        );
        // Refused at the gate, before any connection.
        let file = File::open(file!()).unwrap();
        let err = client
            .upload("https://files.slack.com.evil.net/upload/v1/x", &file)
            .unwrap_err();
        assert!(matches!(err, Error::Upload { reason } if reason.contains("outside slack.com")));
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
