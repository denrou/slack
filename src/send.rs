//! `slack send`: post a message, in a thread when asked, and print where
//! it landed. The `@handle` and `#channel` words the name cache knows
//! become mentions. With `--at`, Slack posts it at that time itself
//! (`chat.scheduleMessage`), so nothing has to run then and a Mac asleep
//! at that hour changes nothing.

use std::io::{self, IsTerminal, Read};

use anyhow::{Context, Result, anyhow, bail};
use jiff::Zoned;
use serde_json::Value;

use crate::api::Client;
use crate::history::is_ts;
use crate::mention;
use crate::names::Names;
use crate::presence;
use crate::target::{self, parse_permalink};

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    target_spec: &str,
    text: Option<&str>,
    thread: Option<&str>,
    at: Option<&Zoned>,
) -> Result<()> {
    let target = target::resolve(client, names, team_id, target_spec)?;
    let thread_ts = match thread {
        Some(thread) => Some(thread_ts_from(thread)?),
        None => target.thread_ts,
    };
    let text = match text {
        Some(text) => text.to_string(),
        None => {
            if io::stdin().is_terminal() {
                eprintln!("Reading the message from stdin; end with Ctrl-D.");
            }
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .context("could not read the message from stdin")?;
            text
        }
    };
    let text = text.trim_end();
    if text.trim().is_empty() {
        bail!("nothing to send: the message is empty");
    }
    let text = mention::link(text, names);

    let mut params = vec![
        ("channel", target.channel.as_str()),
        ("text", text.as_str()),
    ];
    if let Some(ts) = &thread_ts {
        params.push(("thread_ts", ts));
    }
    if let Some(at) = at {
        let post_at = at.timestamp().as_second();
        let post_at_text = post_at.to_string();
        params.push(("post_at", &post_at_text));
        let scheduled = client.call("chat.scheduleMessage", &params)?;
        let channel = scheduled
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or(&target.channel);
        let where_to = landing(client, names, channel, thread_ts.is_some())?;
        let when = presence::clock(post_at, &Zoned::now());
        println!("Scheduled for {when} in {where_to}");
        return Ok(());
    }
    let posted = client.call("chat.postMessage", &params)?;
    let ts = posted
        .get("ts")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("chat.postMessage answered without a ts"))?;
    let channel = posted
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or(&target.channel);

    let where_to = landing(client, names, channel, thread_ts.is_some())?;
    println!("Sent to {where_to}");
    // Best effort: the message is posted either way.
    if let Some(permalink) = client
        .call(
            "chat.getPermalink",
            &[("channel", channel), ("message_ts", ts)],
        )
        .ok()
        .and_then(|body| body.get("permalink")?.as_str().map(str::to_string))
    {
        println!("{permalink}");
    }
    Ok(())
}

/// Where a message goes, as printed: the conversation's name, or a
/// thread in it.
fn landing(client: &Client, names: &mut Names, channel: &str, in_thread: bool) -> Result<String> {
    names.resolve_channels(client, [channel.to_string()])?;
    if let Some(user) = names.channel(channel).and_then(|c| c.user.clone()) {
        names.resolve_users(client, [user])?;
    }
    let label = names.channel_label(channel);
    Ok(if in_thread {
        format!("a thread in {label}")
    } else {
        label
    })
}

/// The thread of `--thread`: a message ts, or the permalink of a message
/// in it.
fn thread_ts_from(value: &str) -> Result<String> {
    if let Some(link) = parse_permalink(value) {
        return Ok(link.thread());
    }
    if is_ts(value) {
        return Ok(value.to_string());
    }
    bail!("`{value}` is neither a message ts (like 1760880000.000100) nor a Slack permalink")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thread_flag_takes_a_ts_or_a_permalink() {
        assert_eq!(
            thread_ts_from("1760880000.000100").unwrap(),
            "1760880000.000100"
        );
        assert_eq!(
            thread_ts_from("https://t.slack.com/archives/C0A754E2V/p1760880000000100").unwrap(),
            "1760880000.000100"
        );
        assert!(thread_ts_from("yesterday").is_err());
    }
}
