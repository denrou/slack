//! Read Slack's session credentials out of a signed-in browser.
//!
//! The web client authenticates every request with a pair: an `xoxc-…`
//! token in the request body and an `xoxd-…` value in the `d` cookie. The
//! cookie is HttpOnly, so page JavaScript cannot read it; it has to come
//! from the browser's own cookie store, which is why this drives CDP rather
//! than injecting a script.
//!
//! Two sources are combined: intercepted API requests, which prove a token
//! is live, and `localConfig_v2` in localStorage, which lists every
//! workspace the user is signed into. One sign-in enrols all of them.
//! localStorage is client internals and may change shape; a failure to read
//! it degrades to what interception found rather than failing the run.
//!
//! Nothing here prints or logs a token, a cookie, a request body or a
//! `localConfig_v2` payload.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::{Duration, Instant};

use percent_encoding::percent_decode_str;
use serde_json::{Value, json};

use super::cdp::{self, Event, Session};

/// Must be `/client`, not the bare host: a returning user who opens
/// `app.slack.com/` is redirected to the marketing site, which makes no API
/// call and defines no `localConfig_v2`, so capture would time out on a
/// perfectly valid session.
pub const SLACK_CLIENT_URL: &str = "https://app.slack.com/client";

/// Serves the app shell for every workspace, so it is never itself a
/// workspace origin.
const CLIENT_HOST: &str = "app.slack.com";

const POLL: Duration = Duration::from_millis(500);
/// Ceiling on waiting for further workspaces to finish booting.
const SETTLE: Duration = Duration::from_secs(3);
/// A single failed liveness probe is not death: the tab tears its context
/// down on every navigation, which an SSO redirect does while the user
/// types a password. Only a sustained run of failures counts.
const LIVENESS_FAILURES: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no Slack session was detected before the timeout")]
    Timeout,
    #[error(
        "the browser was closed before the sign-in completed; run the command again and leave \
         the window open"
    )]
    BrowserClosed,
    #[error("a signed-in session was found, but its `d` cookie was not readable; try again")]
    NoCookie,
    #[error(transparent)]
    Cdp(cdp::Error),
}

impl From<cdp::Error> for Error {
    fn from(err: cdp::Error) -> Error {
        match err {
            cdp::Error::Closed => Error::BrowserClosed,
            other => Error::Cdp(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedWorkspace {
    /// The workspace origin, `https://team.slack.com`.
    pub url: String,
    pub xoxc: String,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
}

pub struct Capture {
    pub xoxd: String,
    pub workspaces: Vec<CapturedWorkspace>,
}

pub struct Options {
    pub timeout: Duration,
    pub headless: bool,
    /// Tokens Slack already refused. Both sources skip them, so a profile
    /// still holding an expired session keeps waiting for a fresh sign-in
    /// instead of returning the same dead token again.
    pub rejected: HashSet<String>,
}

/// Drive an attached session until the tokens are captured.
pub fn capture(
    session: &mut Session,
    options: &Options,
    progress: &mut dyn FnMut(&str),
) -> Result<Capture, Error> {
    session.call("Network.enable", json!({}))?;
    session.call("Runtime.enable", json!({}))?;
    session.call("Page.navigate", json!({ "url": SLACK_CLIENT_URL }))?;
    progress(if options.headless {
        "Loading Slack with the saved browser session…"
    } else {
        "Waiting for the Slack sign-in in the browser window…"
    });

    // Poll both sources: interception sees only the attached tab and only
    // while Slack happens to be calling, so a valid session could time out
    // on it alone. Either source is enough to finish.
    let deadline = Instant::now() + options.timeout;
    let mut intercepted = BTreeMap::new();
    let mut from_config;
    let mut probe_failures = 0;
    loop {
        record_intercepted(session.take_events()?, &options.rejected, &mut intercepted);
        from_config = local_config_workspaces(session, &options.rejected)?;
        if !from_config.is_empty() || !intercepted.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
        let probe = json!({ "expression": "1", "returnByValue": true });
        if session.call("Runtime.evaluate", probe).is_ok() {
            probe_failures = 0;
        } else {
            probe_failures += 1;
            if probe_failures >= LIVENESS_FAILURES {
                return Err(Error::BrowserClosed);
            }
        }
        thread::sleep(POLL);
    }

    // A workspace beyond the first surfaces a beat later as the client boots
    // each one: re-read until the count stops growing. Two stable reads end
    // it, so the single-workspace case costs one poll, not the whole budget.
    let settle_deadline = (Instant::now() + SETTLE).min(deadline);
    let mut stable_reads = 0;
    while Instant::now() < settle_deadline && stable_reads < 2 {
        thread::sleep(POLL);
        record_intercepted(session.take_events()?, &options.rejected, &mut intercepted);
        let again = local_config_workspaces(session, &options.rejected)?;
        if again.len() > from_config.len() {
            from_config = again;
            stable_reads = 0;
        } else {
            stable_reads += 1;
        }
    }

    progress("Session detected, reading the tokens…");
    let xoxd = session_cookie(session)?.ok_or(Error::NoCookie)?;
    let workspaces = merge(intercepted.into_values().collect(), from_config);
    Ok(Capture { xoxd, workspaces })
}

/// Remember the first `xoxc` seen on each workspace's API traffic.
fn record_intercepted(
    events: Vec<Event>,
    rejected: &HashSet<String>,
    intercepted: &mut BTreeMap<String, CapturedWorkspace>,
) {
    for event in events {
        if event.method != "Network.requestWillBeSent" {
            continue;
        }
        let Some(request) = event.params.get("request") else {
            continue;
        };
        let (Some(url), Some(body)) = (
            request.get("url").and_then(Value::as_str),
            request.get("postData").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(origin) = origin_from_api_url(url) else {
            continue;
        };
        if intercepted.contains_key(&origin) {
            continue;
        }
        if let Some(xoxc) = xoxc_from_body(body).filter(|token| !rejected.contains(token)) {
            intercepted.insert(
                origin.clone(),
                CapturedWorkspace {
                    url: origin,
                    xoxc,
                    team_id: None,
                    team_name: None,
                },
            );
        }
    }
}

/// The signed-in workspaces per Slack's localStorage. A failed read is an
/// empty list: undocumented state, and the tab may be mid-navigation.
fn local_config_workspaces(
    session: &mut Session,
    rejected: &HashSet<String>,
) -> Result<Vec<CapturedWorkspace>, Error> {
    let expression = json!({
        "expression": "localStorage.getItem('localConfig_v2')",
        "returnByValue": true,
    });
    let raw = match session.call("Runtime.evaluate", expression) {
        Ok(result) => result
            .pointer("/result/value")
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(cdp::Error::Closed) => return Err(Error::BrowserClosed),
        Err(_) => None,
    };
    Ok(raw
        .map(|raw| workspaces_from_local_config(&raw))
        .unwrap_or_default()
        .into_iter()
        .filter(|workspace| !rejected.contains(&workspace.xoxc))
        .collect())
}

/// The `d` cookie from the browser's whole cookie store. `Storage.getCookies`
/// is browser-wide; `Network.getCookies` answers for the current URL only,
/// which is nothing while sign-in parks the tab on an SSO origin. It stays
/// as a fallback for older protocol builds.
fn session_cookie(session: &mut Session) -> Result<Option<String>, Error> {
    for method in ["Storage.getCookies", "Network.getCookies"] {
        match session.call(method, json!({})) {
            Ok(result) => {
                let cookies = result
                    .get("cookies")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(xoxd) = xoxd_from_cookies(&cookies) {
                    return Ok(Some(xoxd));
                }
            }
            Err(cdp::Error::Closed) => return Err(Error::BrowserClosed),
            Err(_) => continue,
        }
    }
    Ok(None)
}

// The pure extractors below carry the parsing rules; the tests pin every
// encoding Slack is known to use.

/// Gate every URL that will be paired with the session cookie. Workspace
/// URLs are read out of the page's own localStorage, and the URL becomes
/// the host the cookie is sent to: without this, a non-Slack page could
/// name any origin and receive the live session credential.
pub fn is_slack_workspace_url(candidate: &str) -> bool {
    split_https(candidate).is_some_and(|(host, _)| is_workspace_host(&host))
}

/// The workspace origin of a Slack API URL, or None when it is not one.
pub fn origin_from_api_url(url: &str) -> Option<String> {
    let (host, path) = split_https(url)?;
    (path.starts_with("/api/") && is_workspace_host(&host)).then(|| format!("https://{host}"))
}

/// Pull an `xoxc-…` token out of a request body. Slack uses three encodings
/// depending on the endpoint, all of them in normal client traffic; missing
/// one means missing workspaces.
pub fn xoxc_from_body(body: &str) -> Option<String> {
    if !body.contains("xoxc-") {
        return None;
    }
    // multipart/form-data: the value follows its part header.
    if let Some(at) = body.find("name=\"token\"")
        && let Some(token) = token_after(body, at)
    {
        return Some(token);
    }
    // application/json
    for (at, _) in body.match_indices("\"token\"") {
        let rest = body[at + "\"token\"".len()..].trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        if let Some(token) = rest
            .trim_start()
            .strip_prefix('"')
            .and_then(|rest| token_at(rest, 0))
        {
            return Some(token);
        }
    }
    // application/x-www-form-urlencoded
    for (at, _) in body.match_indices("token=xoxc-") {
        if at == 0 || body.as_bytes()[at - 1] == b'&' {
            return token_at(body, at + "token=".len());
        }
    }
    None
}

/// Find the `d` cookie and decode it. Matched on domain rather than URL, so
/// an enterprise-grid host (`foo.enterprise.slack.com`) is covered without
/// enumerating hostnames. The stored value is percent-encoded.
pub fn xoxd_from_cookies(cookies: &[Value]) -> Option<String> {
    cookies.iter().find_map(|cookie| {
        if cookie.get("name")?.as_str()? != "d" {
            return None;
        }
        let domain = cookie.get("domain")?.as_str()?;
        if !is_slack_host(&domain.trim_start_matches('.').to_ascii_lowercase()) {
            return None;
        }
        let value = cookie.get("value")?.as_str()?;
        let decoded = percent_decode_str(value).decode_utf8().ok()?;
        decoded.starts_with("xoxd-").then(|| decoded.into_owned())
    })
}

/// Every signed-in workspace in a `localConfig_v2` payload. Defensive
/// throughout: one malformed team drops that team, not its siblings, and a
/// shape change costs the extra workspaces, never the run.
pub fn workspaces_from_local_config(raw: &str) -> Vec<CapturedWorkspace> {
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(teams) = parsed.get("teams").and_then(Value::as_object) else {
        return Vec::new();
    };
    teams.values().filter_map(workspace_from_team).collect()
}

/// Merge the two sources, keyed on origin. An intercepted token wins: it
/// was seen authenticating a live request, whereas a localStorage entry can
/// be a leftover from a workspace the user has since left.
pub fn merge(
    intercepted: Vec<CapturedWorkspace>,
    from_config: Vec<CapturedWorkspace>,
) -> Vec<CapturedWorkspace> {
    let mut by_url: BTreeMap<String, CapturedWorkspace> = from_config
        .into_iter()
        .map(|workspace| (workspace.url.clone(), workspace))
        .collect();
    for workspace in intercepted {
        match by_url.get_mut(&workspace.url) {
            Some(existing) => {
                existing.xoxc = workspace.xoxc;
                if workspace.team_id.is_some() {
                    existing.team_id = workspace.team_id;
                }
                if workspace.team_name.is_some() {
                    existing.team_name = workspace.team_name;
                }
            }
            None => {
                by_url.insert(workspace.url.clone(), workspace);
            }
        }
    }
    by_url.into_values().collect()
}

fn workspace_from_team(team: &Value) -> Option<CapturedWorkspace> {
    let token = team.get("token")?.as_str()?;
    if !token.starts_with("xoxc-") {
        return None;
    }
    let string = |name: &str| {
        team.get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    // `url` when present, else built from `domain`; an unparseable `url`
    // yields nothing, it does not fall back to `domain`.
    let url = match string("url") {
        Some(url) => {
            let (host, _) = split_https(url)?;
            format!("https://{host}")
        }
        None => {
            let domain = string("domain")?;
            if !domain
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return None;
            }
            format!("https://{domain}.slack.com")
        }
    };
    // The gate: this URL is about to be paired with the live session cookie.
    if !is_slack_workspace_url(&url) {
        return None;
    }
    Some(CapturedWorkspace {
        url,
        xoxc: token.to_string(),
        team_id: string("id").map(str::to_string),
        team_name: string("name").map(str::to_string),
    })
}

/// `(host, path)` of an https URL with a plain host, lowercased, or None.
fn split_https(url: &str) -> Option<(String, &str)> {
    let scheme = url.get(..8)?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return None;
    }
    let rest = &url[8..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, path) = rest.split_at(host_end);
    if host.is_empty() || host.contains(['@', ':']) {
        return None;
    }
    Some((host.to_ascii_lowercase(), path))
}

/// Suffix match on a dot boundary: a plain `contains` would accept
/// `notslack.com.evil.net`.
fn is_slack_host(host: &str) -> bool {
    host == "slack.com" || host.ends_with(".slack.com")
}

fn is_workspace_host(host: &str) -> bool {
    host != CLIENT_HOST && is_slack_host(host)
}

/// The token starting at `start`, which must point at `xoxc-`.
fn token_at(body: &str, start: usize) -> Option<String> {
    let rest = body.get(start..)?;
    if !rest.starts_with("xoxc-") {
        return None;
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn token_after(body: &str, from: usize) -> Option<String> {
    let at = body.get(from..)?.find("xoxc-")?;
    token_at(body, from + at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "xoxc-1234567890-1234567890123-abcdefABCDEF";

    #[test]
    fn multipart_bodies_carry_the_token_after_the_part_header() {
        let body = format!(
            "------WebKitFormBoundary\r\nContent-Disposition: form-data; name=\"token\"\r\n\r\n{TOKEN}\r\n------WebKitFormBoundary--"
        );
        assert_eq!(xoxc_from_body(&body).as_deref(), Some(TOKEN));
    }

    #[test]
    fn json_bodies_carry_the_token_under_the_token_key() {
        let body = format!(r#"{{"channel":"C1","token": "{TOKEN}","limit":10}}"#);
        assert_eq!(xoxc_from_body(&body).as_deref(), Some(TOKEN));
    }

    #[test]
    fn urlencoded_bodies_carry_the_token_as_a_pair() {
        assert_eq!(
            xoxc_from_body(&format!("token={TOKEN}&x=1")).as_deref(),
            Some(TOKEN)
        );
        assert_eq!(
            xoxc_from_body(&format!("x=1&token={TOKEN}")).as_deref(),
            Some(TOKEN)
        );
        assert_eq!(xoxc_from_body(&format!("nottoken={TOKEN}")), None);
    }

    #[test]
    fn other_token_kinds_are_not_xoxc() {
        assert_eq!(xoxc_from_body("token=xoxb-123"), None);
        assert_eq!(xoxc_from_body(""), None);
    }

    fn cookie(name: &str, domain: &str, value: &str) -> Value {
        json!({ "name": name, "domain": domain, "value": value })
    }

    #[test]
    fn the_d_cookie_is_found_on_a_slack_domain_and_decoded() {
        let cookies = [
            cookie("b", ".slack.com", "xoxd-nope"),
            cookie("d", "notslack.com.evil.net", "xoxd-evil"),
            cookie("d", ".slack.com", "xoxd-ab%2Fcd%3D"),
        ];
        assert_eq!(xoxd_from_cookies(&cookies).as_deref(), Some("xoxd-ab/cd="));
    }

    #[test]
    fn enterprise_hosts_and_missing_values_are_handled() {
        let cookies = [cookie("d", "foo.enterprise.slack.com", "xoxd-ent")];
        assert_eq!(xoxd_from_cookies(&cookies).as_deref(), Some("xoxd-ent"));
        let cookies = [
            cookie("d", ".slack.com", "not-a-session"),
            json!({ "name": "d" }),
        ];
        assert_eq!(xoxd_from_cookies(&cookies), None);
    }

    #[test]
    fn local_config_lists_every_signed_in_workspace() {
        let raw = json!({ "teams": {
            "T1": { "id": "T1", "name": "One", "url": "https://one.slack.com/", "token": "xoxc-one" },
            "T2": { "id": "T2", "name": "Two", "domain": "two", "token": "xoxc-two" },
            "T3": { "id": "T3", "name": "Bot", "url": "https://three.slack.com", "token": "xoxb-three" },
            "T4": { "id": "T4", "name": "Evil", "url": "https://evil.example/", "token": "xoxc-four" },
            "T5": { "id": "T5", "name": "Shell", "url": "https://app.slack.com", "token": "xoxc-five" },
            "T6": "garbage"
        }})
        .to_string();
        let found = workspaces_from_local_config(&raw);
        let urls: Vec<&str> = found.iter().map(|w| w.url.as_str()).collect();
        assert_eq!(urls, ["https://one.slack.com", "https://two.slack.com"]);
        assert_eq!(found[0].team_name.as_deref(), Some("One"));
        assert!(workspaces_from_local_config("not json").is_empty());
        assert!(workspaces_from_local_config(r#"{"teams": []}"#).is_empty());
    }

    #[test]
    fn only_https_slack_workspace_origins_pass_the_gate() {
        assert!(is_slack_workspace_url("https://foo.slack.com"));
        assert!(is_slack_workspace_url("https://slack.com/x"));
        assert!(is_slack_workspace_url("HTTPS://Foo.Enterprise.Slack.com/"));
        assert!(!is_slack_workspace_url("http://foo.slack.com"));
        assert!(!is_slack_workspace_url("https://app.slack.com"));
        assert!(!is_slack_workspace_url("https://notslack.com.evil.net"));
        assert!(!is_slack_workspace_url("https://user@foo.slack.com"));
        assert!(!is_slack_workspace_url("https://foo.slack.com:8443"));
        assert!(!is_slack_workspace_url("nonsense"));
    }

    #[test]
    fn api_urls_give_their_workspace_origin() {
        assert_eq!(
            origin_from_api_url("https://foo.slack.com/api/client.counts?x=1").as_deref(),
            Some("https://foo.slack.com")
        );
        assert_eq!(
            origin_from_api_url("https://foo.enterprise.slack.com/api/x").as_deref(),
            Some("https://foo.enterprise.slack.com")
        );
        assert_eq!(origin_from_api_url("https://app.slack.com/api/x"), None);
        assert_eq!(origin_from_api_url("https://foo.slack.com/client"), None);
    }

    #[test]
    fn intercepted_tokens_win_and_config_metadata_is_kept() {
        let config = vec![CapturedWorkspace {
            url: "https://one.slack.com".into(),
            xoxc: "xoxc-stale".into(),
            team_id: Some("T1".into()),
            team_name: Some("One".into()),
        }];
        let live = vec![
            CapturedWorkspace {
                url: "https://one.slack.com".into(),
                xoxc: "xoxc-live".into(),
                team_id: None,
                team_name: None,
            },
            CapturedWorkspace {
                url: "https://two.slack.com".into(),
                xoxc: "xoxc-two".into(),
                team_id: None,
                team_name: None,
            },
        ];
        let merged = merge(live, config);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].xoxc, "xoxc-live");
        assert_eq!(merged[0].team_id.as_deref(), Some("T1"));
        assert_eq!(merged[1].url, "https://two.slack.com");
    }
}
