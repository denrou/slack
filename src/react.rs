//! `slack react`: put an emoji on a message, or take yours back
//! (`reactions.add`, `reactions.remove`). Asking for what is already so
//! (`already_reacted`, `no_reaction`) is not an error: the message ends
//! up as asked either way, and the confirmation says nothing changed.
//! A name Slack refuses is tried again under its other names
//! (`emoji::other_names`).

use anyhow::{Result, bail};

use crate::api::{self, Client};
use crate::emoji;
use crate::history::is_ts;
use crate::names::Names;
use crate::target::{self, parse_permalink};

/// What a call did to the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Changed,
    /// Slack answered `already_reacted` or `no_reaction`: nothing to do.
    AlreadySo,
}

/// The name Slack took, which may be another than the one given, and
/// whether the call changed anything.
pub fn set(
    client: &Client,
    channel: &str,
    ts: &str,
    name: &str,
    add: bool,
) -> Result<(String, Outcome), api::Error> {
    let method = if add {
        "reactions.add"
    } else {
        "reactions.remove"
    };
    let call = |name: &str| {
        outcome(client.call(
            method,
            &[("channel", channel), ("timestamp", ts), ("name", name)],
        ))
    };
    emoji::with_other_names(name, is_invalid_name, call)
}

fn outcome<T>(answer: Result<T, api::Error>) -> Result<Outcome, api::Error> {
    match answer {
        Ok(_) => Ok(Outcome::Changed),
        Err(api::Error::Slack { code, .. })
            if code == "already_reacted" || code == "no_reaction" =>
        {
            Ok(Outcome::AlreadySo)
        }
        Err(err) => Err(err),
    }
}

fn confirmation(outcome: Outcome, remove: bool, emoji: &str, channel: &str) -> String {
    match (outcome, remove) {
        (Outcome::Changed, false) => format!("Reacted {emoji} in {channel}"),
        (Outcome::Changed, true) => format!("Took back {emoji} in {channel}"),
        (Outcome::AlreadySo, false) => {
            format!("Already reacted {emoji} in {channel}, nothing changed")
        }
        (Outcome::AlreadySo, true) => {
            format!("No {emoji} of yours to take back in {channel}, nothing changed")
        }
    }
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
    let (name, outcome) = set(client, &channel, &ts, &name, !remove)?;
    names.resolve_channels(client, [channel.clone()])?;
    if let Some(user) = names.channel(&channel).and_then(|c| c.user.clone()) {
        names.resolve_users(client, [user])?;
    }
    println!(
        "{}",
        confirmation(
            outcome,
            remove,
            &emoji::label(&name),
            &names.channel_label(&channel)
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slack(method: &str, code: &str) -> Result<(), api::Error> {
        Err(api::Error::Slack {
            method: method.to_string(),
            code: code.to_string(),
        })
    }

    #[test]
    fn an_answer_without_error_is_a_change() {
        assert_eq!(outcome(Ok(())).unwrap(), Outcome::Changed);
    }

    #[test]
    fn what_is_already_so_is_not_a_change() {
        assert_eq!(
            outcome(slack("reactions.add", "already_reacted")).unwrap(),
            Outcome::AlreadySo
        );
        assert_eq!(
            outcome(slack("reactions.remove", "no_reaction")).unwrap(),
            Outcome::AlreadySo
        );
    }

    #[test]
    fn any_other_code_stays_an_error() {
        for code in ["invalid_name", "message_not_found", "too_many_reactions"] {
            let err = outcome(slack("reactions.add", code)).unwrap_err();
            assert!(matches!(err, api::Error::Slack { code: got, .. } if got == code));
        }
    }

    #[test]
    fn the_confirmation_claims_a_change_only_when_there_was_one() {
        let line = |outcome, remove| confirmation(outcome, remove, "👍", "#general");
        assert_eq!(line(Outcome::Changed, false), "Reacted 👍 in #general");
        assert_eq!(line(Outcome::Changed, true), "Took back 👍 in #general");
        assert_eq!(
            line(Outcome::AlreadySo, false),
            "Already reacted 👍 in #general, nothing changed"
        );
        assert_eq!(
            line(Outcome::AlreadySo, true),
            "No 👍 of yours to take back in #general, nothing changed"
        );
    }
}
