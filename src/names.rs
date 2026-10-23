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
    /// The ids Slack would not name this run (a private channel you are not
    /// in, another organisation's), kept so a reload does not ask again.
    #[serde(skip)]
    refused: BTreeSet<String>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    path: Option<PathBuf>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct User {
    /// The handle, as in `@handle`.
    pub name: String,
    /// What Slack shows: the display name, else the real name, else the handle.
    pub display: String,
    #[serde(default)]
    pub is_bot: bool,
    /// The full name, for the directory; empty when Slack has none.
    #[serde(default)]
    pub real: String,
    /// The job title from the profile, often empty.
    #[serde(default)]
    pub title: String,
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
            crate::warn::warn(format!(
                "could not save the name cache {}: {err}",
                path.display()
            ));
        }
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.get(id)
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.get(id)
    }

    /// A cache with these entries, for the tests of other modules.
    #[cfg(test)]
    pub fn with(users: Vec<(&str, User)>, channels: Vec<(&str, Channel)>) -> Names {
        Names {
            users: users
                .into_iter()
                .map(|(id, user)| (id.to_string(), user))
                .collect(),
            channels: channels
                .into_iter()
                .map(|(id, channel)| (id.to_string(), channel))
                .collect(),
            refused: BTreeSet::new(),
            dirty: false,
            path: None,
        }
    }

    /// Everyone in the cache, by id.
    pub fn users(&self) -> impl Iterator<Item = (&str, &User)> {
        self.users.iter().map(|(id, user)| (id.as_str(), user))
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
                Err(api::Error::Slack { .. }) => {
                    self.refused.insert(id);
                }
                Err(_) => {} // a network error: asked again next time
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
                Err(api::Error::Slack { .. }) => {
                    self.refused.insert(id);
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// The id of channel `name` (no sigil): from the cache, else after a
    /// walk of `conversations.list`, which fills the cache for next time.
    pub fn channel_id_by_name(
        &mut self,
        client: &Client,
        name: &str,
    ) -> Result<Option<String>, api::Error> {
        if let Some(id) = self.find_channel(name) {
            return Ok(Some(id));
        }
        self.walk(
            client,
            "conversations.list",
            &[
                ("types", "public_channel,private_channel"),
                ("exclude_archived", "true"),
            ],
            Names::add_channels_page,
        )?;
        Ok(self.find_channel(name))
    }

    /// The id of the person called `name` (handle or display name, no
    /// sigil): from the cache, else after a walk of `users.list`.
    pub fn user_id_by_name(
        &mut self,
        client: &Client,
        name: &str,
    ) -> Result<Option<String>, api::Error> {
        if let Some(id) = self.find_user(name) {
            return Ok(Some(id));
        }
        self.walk_users(client)?;
        Ok(self.find_user(name))
    }

    /// Fill the cache with every conversation you are in, in a call or
    /// two: `users.conversations` names the channels and the group
    /// messages and gives the person behind each DM at once, where
    /// `conversations.info` would be one call per conversation.
    pub fn walk_memberships(&mut self, client: &Client) -> Result<(), api::Error> {
        self.walk(
            client,
            "users.conversations",
            &[
                ("types", "public_channel,private_channel,mpim,im"),
                ("exclude_archived", "true"),
            ],
            Names::add_channels_page,
        )
    }

    /// Fill the cache with everyone in the workspace, a thousand at a time.
    pub fn walk_users(&mut self, client: &Client) -> Result<(), api::Error> {
        self.walk(client, "users.list", &[], Names::add_users_page)
    }

    /// The id of channel `name` (no sigil) in the cache, without a call.
    pub fn find_channel(&self, name: &str) -> Option<String> {
        self.channels
            .iter()
            .find(|(_, c)| !c.is_im && !c.is_mpim && c.name.eq_ignore_ascii_case(name))
            .map(|(id, _)| id.clone())
    }

    /// The id of the person whose handle or display name is `name` (no
    /// sigil) in the cache, without a call.
    pub fn find_user(&self, name: &str) -> Option<String> {
        let exact = |field: &str| field.eq_ignore_ascii_case(name);
        self.users
            .iter()
            .find(|(_, u)| exact(&u.name) || exact(&u.display))
            .map(|(id, _)| id.clone())
    }

    /// Page through a directory method, a thousand entries at a time.
    fn walk(
        &mut self,
        client: &Client,
        method: &str,
        params: &[(&str, &str)],
        add_page: fn(&mut Names, &Value) -> Option<String>,
    ) -> Result<(), api::Error> {
        let mut cursor: Option<String> = None;
        loop {
            let mut page_params = params.to_vec();
            page_params.push(("limit", "1000"));
            if let Some(cursor) = &cursor {
                page_params.push(("cursor", cursor));
            }
            let body = client.call(method, &page_params)?;
            cursor = add_page(self, &body);
            if cursor.is_none() {
                return Ok(());
            }
        }
    }

    /// Add one `conversations.list` page; the next cursor, if any.
    fn add_channels_page(&mut self, body: &Value) -> Option<String> {
        for raw in body
            .get("channels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(id), Some(channel)) =
                (raw.get("id").and_then(Value::as_str), channel_from_raw(raw))
            {
                self.channels.insert(id.to_string(), channel);
                self.dirty = true;
            }
        }
        next_cursor(body)
    }

    /// Add one `users.list` page, deactivated accounts left out; the next
    /// cursor, if any.
    fn add_users_page(&mut self, body: &Value) -> Option<String> {
        for raw in body
            .get("members")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if raw.get("deleted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let (Some(id), Some(user)) =
                (raw.get("id").and_then(Value::as_str), user_from_raw(raw))
            {
                self.users.insert(id.to_string(), user);
                self.dirty = true;
            }
        }
        next_cursor(body)
    }

    fn missing<I>(&self, ids: I, known: impl Fn(&Names, &str) -> bool) -> Vec<String>
    where
        I: IntoIterator<Item = String>,
    {
        ids.into_iter()
            .filter(|id| !known(self, id) && !self.refused.contains(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn user_from(body: &Value) -> Option<User> {
    user_from_raw(body.get("user")?)
}

fn user_from_raw(user: &Value) -> Option<User> {
    let name = non_empty(user.get("name"))?.to_string();
    let display = non_empty(user.pointer("/profile/display_name"))
        .or_else(|| non_empty(user.get("real_name")))
        .unwrap_or(&name)
        .to_string();
    Some(User {
        name,
        display,
        is_bot: user.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
        real: non_empty(user.get("real_name"))
            .or_else(|| non_empty(user.pointer("/profile/real_name")))
            .unwrap_or_default()
            .to_string(),
        title: non_empty(user.pointer("/profile/title"))
            .unwrap_or_default()
            .to_string(),
    })
}

fn next_cursor(body: &Value) -> Option<String> {
    non_empty(body.pointer("/response_metadata/next_cursor")).map(str::to_string)
}

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn channel_from(body: &Value) -> Option<Channel> {
    channel_from_raw(body.get("channel")?)
}

fn channel_from_raw(channel: &Value) -> Option<Channel> {
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
        let user = user_from(&json!({"user": {"name": "jdoe", "real_name": "Jane Doe", "profile": {"display_name": "jane", "title": "SRE"}}})).unwrap();
        assert_eq!(
            (user.name.as_str(), user.display.as_str()),
            ("jdoe", "jane")
        );
        assert_eq!(
            (user.real.as_str(), user.title.as_str()),
            ("Jane Doe", "SRE")
        );
        let user = user_from(&json!({"user": {"name": "jdoe", "real_name": "Jane Doe", "profile": {"display_name": ""}}})).unwrap();
        assert_eq!(user.display, "Jane Doe");
        let user = user_from(&json!({"user": {"name": "bot", "is_bot": true}})).unwrap();
        assert!(user.is_bot && user.display == "bot");
        assert!(user_from(&json!({"user": {}})).is_none());
    }

    #[test]
    fn refused_ids_are_not_asked_again() {
        let mut names = Names::default();
        names.refused.insert("C9".to_string());
        let ids = ["C1", "C9", "C1"].map(String::from);
        assert_eq!(
            names.missing(ids, |names, id| names.channels.contains_key(id)),
            ["C1"]
        );
    }

    #[test]
    fn labels_follow_the_conversation_kind() {
        let mut names = Names::default();
        names.users.insert(
            "U1".into(),
            User {
                name: "jdoe".into(),
                display: "jane".into(),
                ..User::default()
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
    fn directory_pages_fill_the_cache_and_hand_back_the_cursor() {
        let mut names = Names::default();
        let page = json!({"channels": [
            {"id": "C1", "name": "general", "is_channel": true},
            {"id": "C2", "name": "SRE-Alerts", "is_private": true},
            {"no_id": true}
        ], "response_metadata": {"next_cursor": "abc"}});
        assert_eq!(names.add_channels_page(&page).as_deref(), Some("abc"));
        let page = json!({"channels": [], "response_metadata": {"next_cursor": ""}});
        assert_eq!(names.add_channels_page(&page), None);
        assert_eq!(names.find_channel("sre-alerts").as_deref(), Some("C2"));
        assert_eq!(names.find_channel("nope"), None);

        let page = json!({"members": [
            {"id": "U1", "name": "jdoe", "profile": {"display_name": "Jane"}},
            {"id": "U2", "name": "gone", "deleted": true}
        ]});
        assert_eq!(names.add_users_page(&page), None);
        assert_eq!(names.find_user("jane").as_deref(), Some("U1"));
        assert_eq!(names.find_user("JDOE").as_deref(), Some("U1"));
        assert_eq!(names.find_user("gone"), None);
        assert!(names.dirty);
    }

    #[test]
    fn group_dm_names_without_the_pattern_are_shown_as_is() {
        assert_eq!(mpim_label("mpdm-a--b-12"), "@a, @b");
        assert_eq!(mpim_label("odd-name"), "#odd-name");
    }
}
