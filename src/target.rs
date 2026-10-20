//! What a conversation argument refers to: `#channel`, `@person`, a bare
//! conversation id, or a Slack permalink.

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use crate::api::Client;
use crate::history::is_ts;
use crate::lastview;
use crate::names::Names;

pub struct Target {
    pub channel: String,
    /// Set when the argument was a permalink to a message: the thread to
    /// answer in.
    pub thread_ts: Option<String>,
}

pub struct Permalink {
    pub channel: String,
    pub ts: String,
    /// The thread a reply belongs to, from the `thread_ts` query parameter.
    pub thread_ts: Option<String>,
}

impl Permalink {
    /// The thread this message belongs to, or starts.
    pub fn thread(&self) -> String {
        self.thread_ts.clone().unwrap_or_else(|| self.ts.clone())
    }
}

pub fn resolve(client: &Client, names: &mut Names, team_id: &str, spec: &str) -> Result<Target> {
    if !spec.is_empty() && spec.chars().all(|c| c.is_ascii_digit()) {
        // A thread number from the last `slack unreads` output.
        let number: usize = spec
            .parse()
            .map_err(|_| anyhow!("`{spec}` is not a thread number"))?;
        let thread = lastview::thread_ref(team_id, number)?.ok_or_else(|| {
            anyhow!("the last view numbered no thread {number}; run `slack unreads` first")
        })?;
        return Ok(Target {
            channel: thread.channel,
            thread_ts: Some(thread.ts),
        });
    }
    if let Some(link) = parse_permalink(spec) {
        let thread_ts = link.thread();
        return Ok(Target {
            channel: link.channel,
            thread_ts: Some(thread_ts),
        });
    }
    if is_conversation_id(spec) {
        return Ok(Target {
            channel: spec.to_string(),
            thread_ts: None,
        });
    }
    if let Some(name) = spec.strip_prefix('#') {
        let channel = names
            .channel_id_by_name(client, name)?
            .ok_or_else(|| anyhow!("no channel named #{name} that you can see"))?;
        return Ok(Target {
            channel,
            thread_ts: None,
        });
    }
    if let Some(name) = spec.strip_prefix('@') {
        let user = names
            .user_id_by_name(client, name)?
            .ok_or_else(|| anyhow!("no one called @{name} in the workspace"))?;
        return Ok(Target {
            channel: open_dm(client, &user)?,
            thread_ts: None,
        });
    }
    bail!("`{spec}` is not a #channel, an @person, a conversation id or a Slack permalink")
}

/// The direct message with a person, opened if it did not exist yet.
pub fn open_dm(client: &Client, user: &str) -> Result<String> {
    let opened = client.call("conversations.open", &[("users", user)])?;
    let channel = opened
        .pointer("/channel/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("conversations.open answered without a channel id"))?;
    Ok(channel.to_string())
}

/// `C…`, `D…` or `G…` followed by uppercase letters and digits.
pub fn is_conversation_id(spec: &str) -> bool {
    let mut chars = spec.chars();
    matches!(chars.next(), Some('C' | 'D' | 'G'))
        && spec.len() >= 9
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `https://<team>.slack.com/archives/<channel>/p<16 digits>`, optionally
/// with `?thread_ts=<ts>&cid=<channel>` for a reply.
pub fn parse_permalink(spec: &str) -> Option<Permalink> {
    let (path, query) = spec.split_once('?').unwrap_or((spec, ""));
    let rest = path.strip_prefix("https://")?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let host = parts.next()?.to_ascii_lowercase();
    if !(host == "slack.com" || host.ends_with(".slack.com")) {
        return None;
    }
    if parts.next()? != "archives" {
        return None;
    }
    let channel = parts.next()?;
    let digits = parts.next()?.strip_prefix('p')?;
    if parts.next().is_some() || digits.len() != 16 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !is_conversation_id(channel) {
        return None;
    }
    let thread_ts = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("thread_ts="))
        .filter(|ts| is_ts(ts))
        .map(str::to_string);
    Some(Permalink {
        channel: channel.to_string(),
        ts: format!("{}.{}", &digits[..10], &digits[10..]),
        thread_ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_ids_are_recognised() {
        assert!(is_conversation_id("C0A754E2V"));
        assert!(is_conversation_id("D06JANE12"));
        assert!(!is_conversation_id("c0a754e2v"));
        assert!(!is_conversation_id("U0A754E2V"));
        assert!(!is_conversation_id("C1"));
    }

    #[test]
    fn permalinks_give_the_channel_the_message_and_its_thread() {
        let link =
            parse_permalink("https://centreonteam.slack.com/archives/C0A754E2V/p1760880000000100")
                .unwrap();
        assert_eq!(
            (link.channel.as_str(), link.ts.as_str()),
            ("C0A754E2V", "1760880000.000100")
        );
        assert_eq!(link.thread_ts, None);
        assert_eq!(link.thread(), "1760880000.000100");

        let reply = parse_permalink(
            "https://centreonteam.slack.com/archives/C0A754E2V/p1760880100000200?thread_ts=1760880000.000100&cid=C0A754E2V",
        )
        .unwrap();
        assert_eq!(reply.thread(), "1760880000.000100");

        assert!(
            parse_permalink("https://evil.example/archives/C0A754E2V/p1760880000000100").is_none()
        );
        assert!(parse_permalink("https://x.slack.com/archives/C0A754E2V/p123").is_none());
        assert!(parse_permalink("https://x.slack.com/archives/C0A754E2V").is_none());
        assert!(parse_permalink("#general").is_none());
    }
}
