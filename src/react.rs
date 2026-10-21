//! `slack react`, and the screen's `R`: put an emoji on a message, or take
//! yours back (`reactions.add`, `reactions.remove`). Asking for what is
//! already so (`already_reacted`, `no_reaction`) is not an error: the
//! message ends up as asked either way. A name Slack refuses is tried
//! again under its other names (`emoji::other_names`).

use anyhow::{Result, bail};

use crate::api::{self, Client};
use crate::emoji;
use crate::history::is_ts;
use crate::names::Names;
use crate::target::{self, parse_permalink};

/// The name Slack took, which may be another than the one given.
pub fn set(
    client: &Client,
    channel: &str,
    ts: &str,
    name: &str,
    add: bool,
) -> Result<String, api::Error> {
    let method = if add {
        "reactions.add"
    } else {
        "reactions.remove"
    };
    let call = |name: &str| match client.call(
        method,
        &[("channel", channel), ("timestamp", ts), ("name", name)],
    ) {
        Err(api::Error::Slack { code, .. })
            if code == "already_reacted" || code == "no_reaction" =>
        {
            Ok(())
        }
        result => result.map(drop),
    };
    let refused = match call(name) {
        Err(err) if is_invalid_name(&err) => err,
        result => return result.map(|()| name.to_string()),
    };
    for other in emoji::other_names(name) {
        match call(&other) {
            Err(err) if is_invalid_name(&err) => {}
            result => return result.map(|()| other),
        }
    }
    Err(refused)
}

fn is_invalid_name(err: &api::Error) -> bool {
    matches!(err, api::Error::Slack { code, .. } if code == "invalid_name")
}

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    message_spec: &str,
    typed: &str,
    ts: Option<&str>,
    remove: bool,
) -> Result<()> {
    let name = emoji::name_of(typed);
    if name.is_empty() {
        bail!("which emoji? give its name, like +1 or white_check_mark");
    }
    let (channel, ts) = message(client, names, team_id, message_spec, ts)?;
    let name = set(client, &channel, &ts, &name, !remove)?;
    names.resolve_channels(client, [channel.clone()])?;
    if let Some(user) = names.channel(&channel).and_then(|c| c.user.clone()) {
        names.resolve_users(client, [user])?;
    }
    let done = if remove { "Took back" } else { "Reacted" };
    println!(
        "{done} {} in {}",
        emoji::label(&name),
        names.channel_label(&channel)
    );
    Ok(())
}

/// The message meant: a permalink's, a thread number's root, or the
/// `--ts` one in a conversation.
fn message(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    spec: &str,
    ts: Option<&str>,
) -> Result<(String, String)> {
    if ts.is_none()
        && let Some(link) = parse_permalink(spec)
    {
        return Ok((link.channel, link.ts));
    }
    let target = target::resolve(client, names, team_id, spec)?;
    match (ts, target.thread_ts) {
        (Some(ts), _) if is_ts(ts) => Ok((target.channel, ts.to_string())),
        (Some(ts), _) => bail!("`{ts}` is not a message ts (like 1760880000.000100)"),
        (None, Some(thread)) => Ok((target.channel, thread)),
        (None, None) => bail!(
            "which message of `{spec}`? give its permalink, a thread number, or --ts with its ts"
        ),
    }
}
