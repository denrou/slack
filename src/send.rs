//! `slack send`: post a message, in a thread when asked, and print where
//! it landed. The `@handle` and `#channel` words the name cache knows
//! become mentions. With `--at`, Slack posts it at that time itself, so
//! nothing has to run then and a Mac asleep at that hour changes nothing.
//! A session token is refused by `chat.scheduleMessage`
//! (`not_allowed_token_type`, 2026-11-04), so it goes the way the Slack
//! app schedules one: a draft with `date_scheduled` (`drafts.create`,
//! undocumented), which the app lists with its scheduled messages. With
//! `--draft`, the same call without the time leaves it in the app's
//! drafts, to finish and send from there. Either way the draft goes to
//! the draft log (`drafts.rs`), to be set beside what was sent.

use std::fs::File;
use std::io::{self, IsTerminal, Read};

use anyhow::{Context, Result, anyhow, bail};
use jiff::Zoned;
use serde_json::Value;

use crate::api::Client;
use crate::drafts;
use crate::history::is_ts;
use crate::mention;
use crate::mrkdwn;
use crate::names::Names;
use crate::presence;
use crate::target::{self, parse_permalink};
use crate::upload::{self, Uploaded};

/// What `send` does with the message.
pub enum Delivery {
    /// Post it now.
    Now,
    /// Have Slack post it then.
    At(Zoned),
    /// Leave it in the Slack app's drafts.
    Draft,
}

/// What was asked on the command line.
pub struct Request<'a> {
    pub target: &'a str,
    /// Read from stdin when None.
    pub text: Option<&'a str>,
    pub thread: Option<&'a str>,
    pub delivery: Delivery,
    pub files: &'a [upload::Local],
}

pub fn run(client: &Client, names: &mut Names, team_id: &str, request: &Request) -> Result<()> {
    let Request {
        text,
        thread,
        ref delivery,
        files,
        ..
    } = *request;
    let target = target::resolve(client, names, team_id, request.target)?;
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
    let with_files = upload::with_files(files.len());

    if !matches!(delivery, Delivery::Now) {
        let post_at = match delivery {
            Delivery::At(at) => Some(at.timestamp().as_second()),
            _ => None,
        };
        let id = client_msg_id()?;
        // Before the upload too: a file kept for a draft that is then
        // refused would stay in Slack, attached to nothing.
        still_ahead(post_at)?;
        let uploaded = upload::send(client, files)?;
        if !uploaded.is_empty() {
            upload::complete(client, &uploaded, None)?;
        }
        let saved = Zoned::now().timestamp().as_second();
        let kept = save_draft(
            client,
            &target.channel,
            thread_ts.as_deref(),
            &text,
            post_at,
            &id,
            &uploaded,
        )?;
        drafts::record(&drafts::Record {
            saved,
            team: team_id.to_string(),
            channel: target.channel.clone(),
            thread_ts: thread_ts.clone(),
            text: text.clone(),
            client_msg_id: id,
            scheduled: kept,
            session: drafts::session(),
        });
        let where_to = landing(client, names, &target.channel, thread_ts.is_some());
        match kept {
            Some(at) => println!(
                "Scheduled for {} in {where_to}{with_files}",
                presence::clock(at, &Zoned::now())
            ),
            None => println!("Draft saved in {where_to}{with_files}, to send from the Slack app"),
        }
        return Ok(());
    }
    if !files.is_empty() {
        // One call shares the files with the text as their message, so
        // `chat.postMessage` has no part in it.
        let uploaded = upload::send(client, files)?;
        let share = upload::Share {
            channel: &target.channel,
            thread_ts: thread_ts.as_deref(),
            text: &text,
        };
        let shared = upload::complete(client, &uploaded, Some(&share))?;
        let where_to = landing(client, names, &target.channel, thread_ts.is_some());
        println!("Sent to {where_to}{with_files}");
        if let Some(ts) = upload::shared_ts(&shared, &target.channel) {
            print_permalink(client, &target.channel, &ts);
        }
        return Ok(());
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

    let where_to = landing(client, names, channel, thread_ts.is_some());
    println!("Sent to {where_to}");
    print_permalink(client, channel, ts);
    Ok(())
}

/// Best effort: the message is posted either way.
fn print_permalink(client: &Client, channel: &str, ts: &str) {
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
}

/// The time of `--at` is checked again once the message is read, since
/// `--at 1m` may have passed while it was typed on stdin.
fn still_ahead(post_at: Option<i64>) -> Result<()> {
    if post_at.is_some_and(|at| at <= Zoned::now().timestamp().as_second()) {
        bail!("the time of --at passed while the message was read; nothing was scheduled");
    }
    Ok(())
}

/// Leaves the message in the Slack app's drafts, for Slack to post at
/// `post_at` when there is one, and returns the time Slack kept. The time
/// is checked once more, after the files went up. A draft kept without
/// the time asked is an error: it would sit in the drafts and never go
/// out.
fn save_draft(
    client: &Client,
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
    post_at: Option<i64>,
    id: &str,
    files: &[Uploaded],
) -> Result<Option<i64>> {
    still_ahead(post_at)?;
    let body = create_draft(client, channel, thread_ts, text, post_at, id, files);
    let body = if files.is_empty() {
        body?
    } else {
        body.context(
            "the draft was not saved; what was uploaded for it stays in your Slack files, shared with nobody",
        )?
    };
    let Some(_) = post_at else {
        return Ok(None);
    };
    body.pointer("/draft/date_scheduled")
        .and_then(Value::as_i64)
        .filter(|&kept| kept > 0)
        .map(Some)
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
    client_msg_id: &str,
    files: &[Uploaded],
) -> Result<Value> {
    let params = draft_params(
        channel,
        thread_ts,
        text,
        date_scheduled,
        client_msg_id,
        files,
    );
    let params: Vec<(&str, &str)> = params
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    Ok(client.call("drafts.create", &params)?)
}

fn draft_params(
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
    date_scheduled: Option<i64>,
    client_msg_id: &str,
    files: &[Uploaded],
) -> Vec<(&'static str, String)> {
    let mut destination = serde_json::json!({ "channel_id": channel });
    if let Some(ts) = thread_ts {
        destination["thread_ts"] = ts.into();
        destination["broadcast"] = false.into();
    }
    let mut params = vec![
        ("blocks", mrkdwn::rich_text(text).to_string()),
        ("client_msg_id", client_msg_id.to_string()),
        ("destinations", Value::from(vec![destination]).to_string()),
        ("file_ids", upload::ids_param(files)),
        ("is_from_composer", "true".to_string()),
    ];
    if let Some(at) = date_scheduled {
        params.push(("date_scheduled", at.to_string()));
    }
    params
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

/// Where a message went, as printed: the conversation's name, or a
/// thread in it. Best effort, the id when a lookup fails: the message is
/// already in Slack, and an error here would read as "not sent", which a
/// retry would turn into a second copy.
fn landing(client: &Client, names: &mut Names, channel: &str, in_thread: bool) -> String {
    if names
        .resolve_channels(client, [channel.to_string()])
        .is_ok()
        && let Some(user) = names.channel(channel).and_then(|c| c.user.clone())
    {
        let _ = names.resolve_users(client, [user]);
    }
    let label = names.channel_label(channel);
    if in_thread {
        format!("a thread in {label}")
    } else {
        label
    }
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
    fn a_draft_carries_its_files_and_its_time() {
        let param = |params: &[(&'static str, String)], name: &str| {
            params
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        };
        let bare = draft_params("D0A1", None, "hi", None, "id-1", &[]);
        assert_eq!(param(&bare, "file_ids").as_deref(), Some("[]"));
        assert_eq!(
            param(&bare, "destinations").as_deref(),
            Some(r#"[{"channel_id":"D0A1"}]"#)
        );
        assert_eq!(param(&bare, "date_scheduled"), None);

        let files = [upload::uploaded("F1", "bundle.json")];
        let full = draft_params(
            "C0A1",
            Some("1760880000.000100"),
            "hi",
            Some(1_762_000_000),
            "id-2",
            &files,
        );
        assert_eq!(param(&full, "file_ids").as_deref(), Some(r#"["F1"]"#));
        assert_eq!(
            param(&full, "destinations").as_deref(),
            Some(r#"[{"broadcast":false,"channel_id":"C0A1","thread_ts":"1760880000.000100"}]"#)
        );
        assert_eq!(
            param(&full, "date_scheduled").as_deref(),
            Some("1762000000")
        );
    }

    #[test]
    fn a_draft_id_is_a_version_4_uuid() {
        assert_eq!(uuid_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
    }
}
