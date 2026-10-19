//! Names for the ids Slack speaks in, cached in `~/.cache/slack/names.json`
//! so the view does not pay one `users.info` per author on every run.
//! Filled on demand, refreshed per id when one is missing, safe to delete.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{self, Client};
use crate::{par, paths};

#[derive(Default, Serialize, Deserialize)]
pub struct Names {
    #[serde(default)]
    users: BTreeMap<String, User>,
    #[serde(default)]
    channels: BTreeMap<String, Channel>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    path: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    /// The handle, as in `@handle`.
    pub name: String,
    /// What Slack shows: the display name, else the real name, else the handle.
    pub display: String,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub is_im: bool,
    #[serde(default)]
    pub is_mpim: bool,
    /// The other person, for a DM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Names {
    /// The cache, or an empty one: a missing or unreadable cache costs a
    /// few lookups, never the run.
    pub fn load() -> Names {
        let Ok(path) = paths::cache_dir().map(|dir| dir.join("names.json")) else {
            return Names::default();
        };
        let mut names: Names = fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        names.path = Some(path);
        names
    }

    /// Write the cache back if anything was learnt. Best effort.
    pub fn save(&self) {
        let Some(path) = self.path.as_ref().filter(|_| self.dirty) else {
            return;
        };
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| serde_json::to_vec(self).map_err(std::io::Error::other))
            .and_then(|json| paths::write_private(path, &json));
        if let Err(err) = written {
            eprintln!("warning: could not save the name cache: {err}");
        }
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.get(id)
    }

    /// The display name of a user, without the sigil.
    pub fn user_name(&self, id: &str) -> Option<String> {
        self.users.get(id).map(|user| user.display.clone())
    }

    /// The name of a channel, without the sigil. DMs have no name.
    pub fn channel_name(&self, id: &str) -> Option<String> {
        self.channels
            .get(id)
            .filter(|channel| !channel.is_im && !channel.name.is_empty())
            .map(|channel| channel.name.clone())
    }

    /// `@display` for a user, or the bare id when unknown.
    pub fn user_label(&self, id: &str) -> String {
        match self.user_name(id) {
            Some(name) => format!("@{name}"),
            None => id.to_string(),
        }
    }

    /// `#name` for a channel, `@person` for a DM, the people of a group DM,
    /// or the bare id when unknown.
    pub fn channel_label(&self, id: &str) -> String {
        match self.channels.get(id) {
            Some(channel) if channel.is_im => channel
                .user
                .as_deref()
                .map_or_else(|| id.to_string(), |user| self.user_label(user)),
            Some(channel) if channel.is_mpim => mpim_label(&channel.name),
            Some(channel) if !channel.name.is_empty() => format!("#{}", channel.name),
            _ => id.to_string(),
        }
    }

    /// Look up the users not cached yet, a few at a time. A lookup that
    /// fails leaves that id unresolved; a dead session fails the call.
    pub fn resolve_users<I>(&mut self, client: &Client, ids: I) -> Result<(), api::Error>
    where
        I: IntoIterator<Item = String>,
    {
        let missing = self.missing(ids, |names, id| names.users.contains_key(id));
        let fetched = par::map(missing, |id| {
            let result = client.call("users.info", &[("user", &id)]);
            (id, result)
        });
        for (id, result) in fetched {
            match result {
                Ok(body) => {
                    if let Some(user) = user_from(&body) {
                        self.users.insert(id, user);
                        self.dirty = true;
                    }
                }
                Err(err @ api::Error::Auth { .. }) => return Err(err),
                Err(_) => {} // a deactivated account, say: the id stays visible
            }
        }
        Ok(())
    }

    /// Same as `resolve_users`, for conversations.
    pub fn resolve_channels<I>(&mut self, client: &Client, ids: I) -> Result<(), api::Error>
    where
        I: IntoIterator<Item = String>,
    {
        let missing = self.missing(ids, |names, id| names.channels.contains_key(id));
        let fetched = par::map(missing, |id| {
            let result = client.call("conversations.info", &[("channel", &id)]);
            (id, result)
        });
        for (id, result) in fetched {
            match result {
                Ok(body) => {
                    if let Some(channel) = channel_from(&body) {
                        self.channels.insert(id, channel);
                        self.dirty = true;
                    }
                }
                Err(err @ api::Error::Auth { .. }) => return Err(err),
                Err(_) => {}
            }
        }
        Ok(())
    }

    fn missing<I>(&self, ids: I, known: impl Fn(&Names, &str) -> bool) -> Vec<String>
    where
        I: IntoIterator<Item = String>,
    {
        ids.into_iter()
            .filter(|id| !known(self, id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn user_from(body: &Value) -> Option<User> {
    let user = body.get("user")?;
    let name = non_empty(user.get("name"))?.to_string();
    let display = non_empty(user.pointer("/profile/display_name"))
        .or_else(|| non_empty(user.get("real_name")))
        .unwrap_or(&name)
        .to_string();
    Some(User {
        name,
        display,
        is_bot: user.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn channel_from(body: &Value) -> Option<Channel> {
    let channel = body.get("channel")?;
    let flag = |name: &str| channel.get(name).and_then(Value::as_bool).unwrap_or(false);
    Some(Channel {
        name: channel
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        is_private: flag("is_private"),
        is_im: flag("is_im"),
        is_mpim: flag("is_mpim"),
        user: channel
            .get("user")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Slack names a group DM `mpdm-alice--bob--carol-1`; show the people.
fn mpim_label(name: &str) -> String {
    let Some(inner) = name.strip_prefix("mpdm-") else {
        return format!("#{name}");
    };
    let inner = match inner.rsplit_once('-') {
        Some((people, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => people,
        _ => inner,
    };
    inner
        .split("--")
        .map(|handle| format!("@{handle}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn display_name_falls_back_to_real_name_then_handle() {
        let user = user_from(&json!({"user": {"name": "jdoe", "real_name": "Jane Doe", "profile": {"display_name": "jane"}}})).unwrap();
        assert_eq!(
            (user.name.as_str(), user.display.as_str()),
            ("jdoe", "jane")
        );
        let user = user_from(&json!({"user": {"name": "jdoe", "real_name": "Jane Doe", "profile": {"display_name": ""}}})).unwrap();
        assert_eq!(user.display, "Jane Doe");
        let user = user_from(&json!({"user": {"name": "bot", "is_bot": true}})).unwrap();
        assert!(user.is_bot && user.display == "bot");
        assert!(user_from(&json!({"user": {}})).is_none());
    }

    #[test]
    fn labels_follow_the_conversation_kind() {
        let mut names = Names::default();
        names.users.insert(
            "U1".into(),
            User {
                name: "jdoe".into(),
                display: "jane".into(),
                is_bot: false,
            },
        );
        names.channels.insert(
            "C1".into(),
            channel_from(&json!({"channel": {"name": "general", "is_channel": true}})).unwrap(),
        );
        names.channels.insert(
            "D1".into(),
            channel_from(&json!({"channel": {"is_im": true, "user": "U1"}})).unwrap(),
        );
        names.channels.insert(
            "D2".into(),
            channel_from(&json!({"channel": {"is_im": true, "user": "U9"}})).unwrap(),
        );
        names.channels.insert(
            "G1".into(),
            channel_from(&json!({"channel": {"name": "mpdm-jdoe--bob--carol-1", "is_mpim": true}}))
                .unwrap(),
        );
        assert_eq!(names.channel_label("C1"), "#general");
        assert_eq!(names.channel_label("D1"), "@jane");
        assert_eq!(names.channel_label("D2"), "U9");
        assert_eq!(names.channel_label("G1"), "@jdoe, @bob, @carol");
        assert_eq!(names.channel_label("C9"), "C9");
        assert_eq!(names.user_label("U1"), "@jane");
        assert_eq!(names.user_label("U9"), "U9");
        assert_eq!(names.channel_name("D1"), None);
        assert_eq!(names.channel_name("C1").as_deref(), Some("general"));
    }

    #[test]
    fn group_dm_names_without_the_pattern_are_shown_as_is() {
        assert_eq!(mpim_label("mpdm-a--b-12"), "@a, @b");
        assert_eq!(mpim_label("odd-name"), "#odd-name");
    }
}
