//! `slack read`: move the read cursors to what the last view showed, so
//! the Slack app, the footer count and the CLI agree afterwards. Only
//! ever marks what was seen: a message that arrived since stays unread.

use anyhow::{Result, anyhow, bail};

use crate::api::Client;
use crate::history::compare_ts;
use crate::names::Names;
use crate::{counts, lastview, target};

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    targets: &[String],
    all: bool,
) -> Result<()> {
    let shown = lastview::recall(team_id)?;
    if shown.is_empty() {
        bail!("nothing to mark: run `slack` first, then `slack read` marks what it showed");
    }
    let selected: Vec<(String, String)> = if targets.is_empty() {
        // Without a name, stop at what needed you: the channels `-a`
        // listed are a wider net than "I read these", so they take --all.
        let picked: Vec<(String, String)> = shown
            .iter()
            .filter(|(_, shown)| all || shown.attention)
            .map(|(id, shown)| (id.clone(), shown.latest.clone()))
            .collect();
        if picked.is_empty() {
            bail!(
                "the last view showed no DM or mention; `slack read --all` also marks the \
                 channels it listed, or name them"
            );
        }
        picked
    } else {
        targets
            .iter()
            .map(|spec| {
                let channel = target::resolve(client, names, team_id, spec)?.channel;
                let latest = shown.get(&channel).map(|shown| shown.latest.clone());
                let latest = latest.ok_or_else(|| {
                    anyhow!("{spec} was not in the last view; run `slack` (or `slack -a`) first")
                })?;
                Ok((channel, latest))
            })
            .collect::<Result<_>>()?
    };

    names.resolve_channels(client, selected.iter().map(|(channel, _)| channel.clone()))?;
    // A cursor only moves forward: when Slack already reads further (the
    // app, or `slack ui`), marking at an older message would make the newer
    // ones unread again.
    let current = counts::fetch(client)?;
    let cursors: std::collections::BTreeMap<&str, &str> = current
        .conversations
        .iter()
        .filter_map(|c| Some((c.id.as_str(), c.last_read.as_deref()?)))
        .collect();
    let mut marked = Vec::new();
    for (channel, latest) in &selected {
        let already_read = cursors
            .get(channel.as_str())
            .is_some_and(|cursor| compare_ts(cursor, latest).is_ge());
        if already_read {
            println!("{} was already read", names.channel_label(channel));
        } else {
            client.call(
                "conversations.mark",
                &[("channel", channel), ("ts", latest)],
            )?;
            println!("Marked {} as read", names.channel_label(channel));
        }
        marked.push(channel.clone());
    }
    lastview::forget(team_id, &marked)?;
    Ok(())
}
