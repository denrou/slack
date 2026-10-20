//! `slack read`: move the read cursors to what the last view showed, so
//! the Slack app, the footer count and the CLI agree afterwards. Only
//! ever marks what was seen: a message that arrived since stays unread.

use anyhow::{Result, anyhow, bail};

use crate::api::Client;
use crate::history::compare_ts;
use crate::names::Names;
use crate::{counts, lastview, target};

pub fn run(client: &Client, names: &mut Names, team_id: &str, targets: &[String]) -> Result<()> {
    let shown = lastview::recall(team_id)?;
    if shown.is_empty() {
        bail!("nothing to mark: run `slack unreads` first, then `slack read` marks what it showed");
    }
    let selected: Vec<(String, String)> = if targets.is_empty() {
        // Without a name, what the view listed: a conversation `slack show`
        // printed was asked for, read or not, so marking it takes its name.
        let picked: Vec<(String, String)> = shown
            .iter()
            .filter(|(_, shown)| shown.listed)
            .map(|(id, shown)| (id.clone(), shown.latest.clone()))
            .collect();
        if picked.is_empty() {
            bail!(
                "nothing from the last view is left to mark; `slack read <name>` marks a \
                 conversation `slack show` printed"
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
                    anyhow!(
                        "{spec} was not in the last view; run `slack unreads` or `slack show {spec}` first"
                    )
                })?;
                Ok((channel, latest))
            })
            .collect::<Result<_>>()?
    };

    names.resolve_channels(client, selected.iter().map(|(channel, _)| channel.clone()))?;
    let cursors = counts::cursors(client)?;
    let mut marked = Vec::new();
    for (channel, latest) in &selected {
        if already_read(cursors.get(channel.as_str()).map(String::as_str), latest) {
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

/// A cursor only moves forward: when Slack already reads at or past
/// `latest` (the app, or the screen), marking there would make the newer
/// messages unread again.
pub fn already_read(cursor: Option<&str>, latest: &str) -> bool {
    cursor.is_some_and(|cursor| compare_ts(cursor, latest).is_ge())
}

#[cfg(test)]
mod tests {
    use super::already_read;

    #[test]
    fn a_cursor_only_moves_forward() {
        assert!(already_read(Some("1700000000.000200"), "1700000000.000100"));
        assert!(already_read(Some("1700000000.000100"), "1700000000.000100"));
        assert!(!already_read(
            Some("1700000000.000100"),
            "1700000000.000200"
        ));
        assert!(!already_read(None, "1700000000.000200"));
        assert!(!already_read(
            Some("0000000000.000000"),
            "1700000000.000200"
        ));
    }
}
