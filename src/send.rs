//! `slack send`: post a message, in a thread when asked, and print where
//! it landed. The `@handle` and `#channel` words the name cache knows
//! become mentions. With `--at`, Slack posts it at that time itself, so
//! nothing has to run then and a Mac asleep at that hour changes nothing.
//! A session token is refused by `chat.scheduleMessage`
//! (`not_allowed_token_type`, 2026-11-04), so it goes the way the Slack
//! app schedules one: a draft with `date_scheduled` (`drafts.create`,
//! undocumented), which the app lists with its scheduled messages. With
//! `--draft`, the same call without the time leaves it in the app's
//! drafts, to finish and send from there.

use std::fs::File;
use std::io::{self, IsTerminal, Read};

use anyhow::{Context, Result, anyhow, bail};
use jiff::Zoned;
use serde_json::Value;

use crate::api::Client;
use crate::history::is_ts;
use crate::mention;
use crate::mrkdwn;
use crate::names::Names;
use crate::presence;
use crate::target::{self, parse_permalink};

/// What `send` does with the message.
pub enum Delivery {
    /// Post it now.
    Now,
    /// Have Slack post it then.
    At(Zoned),
    /// Leave it in the Slack app's drafts.
    Draft,
}

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    target_spec: &str,
    text: Option<&str>,
    thread: Option<&str>,
    delivery: &Delivery,
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

    match delivery {
        Delivery::Now => {}
        Delivery::At(at) => {
            let post_at = schedule(
                client,
                &target.channel,
                thread_ts.as_deref(),
                &text,
                at.timestamp().as_second(),
            )?;
            let where_to = landing(client, names, &target.channel, thread_ts.is_some())?;
            let when = presence::clock(post_at, &Zoned::now());
            println!("Scheduled for {when} in {where_to}");
            return Ok(());
        }
        Delivery::Draft => {
            create_draft(client, &target.channel, thread_ts.as_deref(), &text, None)?;
            let where_to = landing(client, names, &target.channel, thread_ts.is_some())?;
            println!("Draft saved in {where_to}, to send from the Slack app");
            return Ok(());
        }
    }
    let mut params = vec![
        ("channel", target.channel.as_str()),
        ("text", text.as_str()),
    ];
    if let Some(ts) = &thread_ts {
        params.push(("thread_ts", ts));
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

/// Hands the message to Slack as a draft it posts at `post_at`, and
/// returns the time Slack kept. A draft kept without that time is an
/// error: it would sit in the drafts and never go out.
fn schedule(
    client: &Client,
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
    post_at: i64,
) -> Result<i64> {
    let body = create_draft(client, channel, thread_ts, text, Some(post_at))?;
    body.pointer("/draft/date_scheduled")
        .and_then(Value::as_i64)
        .filter(|&kept| kept > 0)
        .ok_or_else(|| {
            anyhow!(
                "Slack kept the message as a draft without its time: it will not go out; delete it from the Slack app's drafts"
            )
        })
}

/// `drafts.create` with the fields the Slack app sends from its composer,
/// `date_scheduled` only for a message Slack is to post later.
fn create_draft(
    client: &Client,
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
    date_scheduled: Option<i64>,
) -> Result<Value> {
    let mut destination = serde_json::json!({ "channel_id": channel });
    if let Some(ts) = thread_ts {
        destination["thread_ts"] = ts.into();
        destination["broadcast"] = false.into();
    }
    let blocks = mrkdwn::rich_text(text).to_string();
    let destinations = Value::from(vec![destination]).to_string();
    let client_msg_id = client_msg_id()?;
    let date_scheduled = date_scheduled.map(|at| at.to_string());
    let mut params = vec![
        ("blocks", blocks.as_str()),
        ("client_msg_id", client_msg_id.as_str()),
        ("destinations", destinations.as_str()),
        ("file_ids", "[]"),
        ("is_from_composer", "true"),
    ];
    if let Some(at) = &date_scheduled {
        params.push(("date_scheduled", at));
    }
    Ok(client.call("drafts.create", &params)?)
}

/// A random UUID, the id the Slack app gives each draft.
fn client_msg_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .context("could not read /dev/urandom for the draft's id")?;
    Ok(uuid_v4(bytes))
}

fn uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
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

    #[test]
    fn a_draft_id_is_a_version_4_uuid() {
        assert_eq!(uuid_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
    }
}
