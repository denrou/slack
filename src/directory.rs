//! The directory: every conversation you are in and everyone in the
//! workspace, behind `slack channels`, `slack people` and the TUI's
//! picker. `client.counts` already lists what you are in with its unread
//! state; the names come from the cache, filled in one walk when some are
//! missing, so the first run costs a call or two and the next ones none.

use std::cmp::Ordering;

use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::counts::{self, Conversation, Kind};
use crate::history::compare_ts;
use crate::names::{Names, User};

/// Past this many unknown people, one directory walk beats one lookup
/// each.
const WALK_THRESHOLD: usize = 10;

/// A conversation you are in.
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    pub id: String,
    pub kind: Kind,
    /// `#channel`, `@person`, or the people of a group message.
    pub name: String,
    pub private: bool,
    pub muted: bool,
    pub unread: bool,
    pub mention_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// The read cursor: where "new" starts when something is unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_read: Option<String>,
    /// The person behind a direct message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Listed {
    /// What to say after the name: `private`, `muted`, `unread`, mentions.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.private {
            notes.push("private".to_string());
        }
        if self.muted {
            notes.push("muted".to_string());
        }
        if self.unread {
            notes.push("unread".to_string());
        }
        match self.mention_count {
            0 => {}
            1 => notes.push("1 mention".to_string()),
            n => notes.push(format!("{n} mentions")),
        }
        notes
    }

    /// The read cursor, when there is something new after it.
    pub fn unread_from(&self) -> Option<&str> {
        self.last_read.as_deref().filter(|_| self.unread)
    }
}

/// Someone in the workspace.
#[derive(Debug, Clone, Serialize)]
pub struct Person {
    pub id: String,
    /// The handle: `@handle` names them in a target.
    pub handle: String,
    /// What Slack shows next to their messages.
    pub display: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub real: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
}

impl Person {
    pub fn from_cache(id: &str, user: &User) -> Person {
        Person {
            id: id.to_string(),
            handle: user.name.clone(),
            display: user.display.clone(),
            real: user.real.clone(),
            title: user.title.clone(),
        }
    }

    /// Everything a filter may match.
    pub fn search(&self) -> String {
        format!(
            "{} {} {} {}",
            self.handle, self.display, self.real, self.title
        )
    }
}

/// Every conversation you are in: channels by name, then group messages,
/// then direct messages, the most recent first.
pub fn conversations(client: &Client, names: &mut Names) -> Result<Vec<Listed>, api::Error> {
    let counts = counts::fetch(client)?;
    name_all(client, names, &counts.conversations)?;
    let mut listed: Vec<Listed> = counts
        .conversations
        .iter()
        .map(|conversation| listed(conversation, names))
        .collect();
    listed.sort_by(order);
    Ok(listed)
}

/// The people you have a direct message with, the most recent first.
/// The directory is walked first, so names and titles are current.
pub fn correspondents(client: &Client, names: &mut Names) -> Result<Vec<Person>, api::Error> {
    names.walk_users(client)?;
    let people = conversations(client, names)?
        .into_iter()
        .filter(|conversation| conversation.kind == Kind::Im)
        .filter_map(|conversation| {
            let id = conversation.user?;
            let user = names.user(&id)?;
            Some(Person::from_cache(&id, user))
        })
        .collect();
    Ok(people)
}

/// The members of a channel, bots and you included, or None when there
/// are more than `max`: which of them one page holds says nothing.
pub fn members(
    client: &Client,
    channel: &str,
    max: usize,
) -> Result<Option<Vec<String>>, api::Error> {
    const METHOD: &str = "conversations.members";
    let limit = (max + 1).to_string();
    let body = client.call(METHOD, &[("channel", channel), ("limit", &limit)])?;
    let members: Vec<String> = body
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| api::Error::Shape {
            method: METHOD.to_string(),
            reason: "missing `members`".to_string(),
        })?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let more = body
        .pointer("/response_metadata/next_cursor")
        .and_then(Value::as_str)
        .is_some_and(|cursor| !cursor.is_empty());
    Ok((!more && members.len() <= max).then_some(members))
}

/// Everyone in the workspace matching `filter`, from a fresh walk of the
/// directory, by display name.
pub fn people(client: &Client, names: &mut Names, filter: &str) -> Result<Vec<Person>, api::Error> {
    names.walk_users(client)?;
    Ok(everyone(names)
        .into_iter()
        .filter(|person| matches(filter, &person.search()))
        .collect())
}

/// Everyone the cache knows, by display name; bots and Slackbot left out,
/// since nobody writes to them.
pub fn everyone(names: &Names) -> Vec<Person> {
    let mut people: Vec<Person> = names
        .users()
        .filter(|(id, user)| !user.is_bot && *id != "USLACKBOT")
        .map(|(id, user)| Person::from_cache(id, user))
        .collect();
    people.sort_by_cached_key(|person| person.display.to_lowercase());
    people
}

/// Every word of the filter appears in the text, whatever the case. An
/// empty filter matches everything.
pub fn matches(filter: &str, text: &str) -> bool {
    let text = text.to_lowercase();
    filter
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}

/// Make sure every conversation and the person behind every DM has a
/// name: one walk of the memberships when conversations are unknown, one
/// of the directory when many people are, single lookups for the rest.
fn name_all(
    client: &Client,
    names: &mut Names,
    conversations: &[Conversation],
) -> Result<(), api::Error> {
    let unknown = |names: &Names| -> Vec<String> {
        conversations
            .iter()
            .filter(|c| names.channel(&c.id).is_none())
            .map(|c| c.id.clone())
            .collect()
    };
    if !unknown(names).is_empty() {
        names.walk_memberships(client)?;
        // Still unknown after the walk: archived, say. One by one.
        names.resolve_channels(client, unknown(names))?;
    }
    let people: Vec<String> = conversations
        .iter()
        .filter_map(|c| names.channel(&c.id)?.user.clone())
        .collect();
    let unknown_people = people.iter().filter(|id| names.user(id).is_none()).count();
    if unknown_people > WALK_THRESHOLD {
        names.walk_users(client)?;
    }
    names.resolve_users(client, people)
}

fn listed(conversation: &Conversation, names: &Names) -> Listed {
    let cached = names.channel(&conversation.id);
    Listed {
        id: conversation.id.clone(),
        kind: conversation.kind,
        name: names.channel_label(&conversation.id),
        private: conversation.kind == Kind::Channel && cached.is_some_and(|c| c.is_private),
        muted: conversation.muted,
        unread: conversation.is_unread(),
        mention_count: conversation.mention_count,
        latest: conversation.latest.clone(),
        last_read: conversation.last_read.clone(),
        user: cached.and_then(|c| c.user.clone()),
    }
}

/// Channels, then group messages, by name; direct messages last, the most
/// recent first, the way the app lists them.
fn order(a: &Listed, b: &Listed) -> Ordering {
    let rank = |kind: Kind| match kind {
        Kind::Channel => 0,
        Kind::Mpim => 1,
        Kind::Im => 2,
    };
    rank(a.kind).cmp(&rank(b.kind)).then_with(|| match a.kind {
        Kind::Im => compare_ts(
            b.latest.as_deref().unwrap_or("0"),
            a.latest.as_deref().unwrap_or("0"),
        ),
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: Kind, name: &str, latest: &str) -> Listed {
        Listed {
            id: id.into(),
            kind,
            name: name.into(),
            private: false,
            muted: false,
            unread: false,
            mention_count: 0,
            latest: Some(latest.into()),
            last_read: None,
            user: None,
        }
    }

    #[test]
    fn filters_match_every_word_whatever_the_case() {
        assert!(matches("", "anything"));
        assert!(matches("DEV", "#dev-team"));
        assert!(matches("jane sre", "jdoe Jane Doe SRE"));
        assert!(!matches("jane ops", "jdoe Jane Doe SRE"));
    }

    #[test]
    fn channels_come_by_name_then_groups_then_recent_dms() {
        let mut listed = [
            entry("D1", Kind::Im, "@old", "1.0"),
            entry("C2", Kind::Channel, "#Zeta", "9.0"),
            entry("G1", Kind::Mpim, "@a, @b", "5.0"),
            entry("D2", Kind::Im, "@new", "2.0"),
            entry("C1", Kind::Channel, "#alpha", "1.0"),
        ];
        listed.sort_by(order);
        let ids: Vec<&str> = listed.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["C1", "C2", "G1", "D2", "D1"]);
    }

    #[test]
    fn notes_say_what_matters_about_a_channel() {
        let mut listed = entry("C1", Kind::Channel, "#ops", "1.0");
        assert!(listed.notes().is_empty());
        listed.private = true;
        listed.muted = true;
        listed.unread = true;
        listed.mention_count = 2;
        listed.last_read = Some("0.5".into());
        assert_eq!(listed.notes(), ["private", "muted", "unread", "2 mentions"]);
        assert_eq!(listed.unread_from(), Some("0.5"));
        listed.unread = false;
        assert_eq!(listed.unread_from(), None);
    }
}
