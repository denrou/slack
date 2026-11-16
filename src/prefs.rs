//! The user's own Slack preferences, read for two things: the
//! conversations they muted and the conversations they visited last.
//! `users.prefs.get` is a web-client endpoint and both are JSON strings
//! inside it, so a missing or unreadable pref reads as "nothing muted",
//! which shows more rather than less, and as nothing visited. Missing is
//! silent; unreadable is a warning, since it is Slack's shape that moved.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

use crate::api::{self, Client};

#[derive(Debug, Default, Clone)]
pub struct Prefs {
    /// Muted conversations: the app badges them for mentions only.
    pub muted: HashSet<String>,
    /// Conversation ids, the last visited first: the app's own recent
    /// list, whose DMs `slack who` takes by default.
    pub recent: Vec<String>,
    /// The prefs that are there but could not be read, by name: each read
    /// as absent, which for the mute list shows and counts what is muted.
    pub unreadable: Vec<&'static str>,
}

const MUTES: &str = "all_notifications_prefs";
const RECENTS: &str = "recents";

/// The preferences that shape the view, None when they could not be read.
/// Only a dead session is an error; any other failure is reported.
pub fn fetch(client: &Client) -> Result<Option<Prefs>, api::Error> {
    match client.call("users.prefs.get", &[]) {
        Ok(body) => {
            let prefs = parse(&body);
            warn_unreadable(&prefs.unreadable);
            Ok(Some(prefs))
        }
        Err(err @ api::Error::Auth { .. }) => Err(err),
        Err(err) => {
            crate::warn::warn(format!(
                "could not read the notification preferences: {err}"
            ));
            Ok(None)
        }
    }
}

/// Once per pref and per process: the watch reads the prefs at each event.
fn warn_unreadable(names: &[&'static str]) {
    static WARNED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut warned = WARNED.lock().unwrap_or_else(PoisonError::into_inner);
    for name in names {
        if warned.contains(name) {
            continue;
        }
        warned.push(name);
        let effect = match *name {
            MUTES => "muted conversations are shown and counted",
            _ => "it is read as empty",
        };
        crate::warn::warn(format!(
            "the `{name}` preference is not in the form expected, so {effect}"
        ));
    }
}

pub fn parse(body: &Value) -> Prefs {
    let mut unreadable = Vec::new();
    let mut read = |name: &'static str| match nested(body, name) {
        Nested::Parsed(value) => Some(value),
        Nested::Absent => None,
        Nested::Unreadable => {
            unreadable.push(name);
            None
        }
    };
    let muted = read(MUTES).map(|parsed| muted(&parsed)).unwrap_or_default();
    let recent = read(RECENTS)
        .map(|parsed| recent(&parsed))
        .unwrap_or_default();
    Prefs {
        muted,
        recent,
        unreadable,
    }
}

enum Nested {
    Parsed(Value),
    /// Not there, null or blank: nothing set, nothing to say.
    Absent,
    /// There, and not a string holding a JSON object.
    Unreadable,
}

/// A pref that holds JSON in a string.
fn nested(body: &Value, name: &str) -> Nested {
    match body.get("prefs").and_then(|prefs| prefs.get(name)) {
        None | Some(Value::Null) => Nested::Absent,
        Some(Value::String(raw)) if raw.trim().is_empty() => Nested::Absent,
        Some(Value::String(raw)) => match serde_json::from_str(raw) {
            Ok(Value::Null) => Nested::Absent,
            Ok(parsed @ Value::Object(_)) => Nested::Parsed(parsed),
            _ => Nested::Unreadable,
        },
        Some(_) => Nested::Unreadable,
    }
}

/// `prefs.all_notifications_prefs` is a JSON string whose `channels` map
/// carries `muted: true` per conversation, among its notification levels.
fn muted(parsed: &Value) -> HashSet<String> {
    let Some(channels) = parsed.get("channels").and_then(Value::as_object) else {
        return HashSet::new();
    };
    channels
        .iter()
        .filter(|(_, pref)| pref.get("muted").and_then(Value::as_bool) == Some(true))
        .map(|(id, _)| id.clone())
        .collect()
}

/// `prefs.recents` is a JSON string whose `navigation` list holds what the
/// app visited, from every client: an `id`, an `object_type` (`CHANNEL`,
/// `DM`, `PAGE` for a view such as Threads) and a `timestamp` in
/// milliseconds. The pages are left out; what is not a conversation you
/// are in finds no match in the counts anyway.
fn recent(parsed: &Value) -> Vec<String> {
    let Some(visits) = parsed.get("navigation").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut visits: Vec<(&str, u64)> = visits
        .iter()
        .filter(|visit| visit.get("object_type").and_then(Value::as_str) != Some("PAGE"))
        .filter_map(|visit| {
            let id = visit.get("id")?.as_str()?;
            Some((
                id,
                visit.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
            ))
        })
        .collect();
    visits.sort_by_key(|visit| std::cmp::Reverse(visit.1));
    let mut ids: Vec<String> = Vec::new();
    for (id, _) in visits {
        if !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn mutes_are_read_out_of_the_nested_json_string() {
        let inner = json!({"channels": {
            "C1": {"muted": true, "desktop": "everything"},
            "C2": {"muted": false, "desktop": "everything"},
            "C3": {"desktop": "nothing"},
            "D4": {"muted": true}
        }, "global": {}});
        let body = json!({"ok": true, "prefs": {"all_notifications_prefs": inner.to_string()}});
        let prefs = parse(&body);
        assert_eq!(
            prefs.muted,
            HashSet::from(["C1".to_string(), "D4".to_string()])
        );
    }

    #[test]
    fn a_missing_or_broken_pref_changes_nothing() {
        for body in [
            json!({"ok": true, "prefs": {}}),
            json!({"prefs": {"all_notifications_prefs": "not json"}}),
            json!({"prefs": {"all_notifications_prefs": "{\"channels\": []}"}}),
        ] {
            let prefs = parse(&body);
            assert!(prefs.muted.is_empty());
            assert!(prefs.recent.is_empty());
        }
    }

    #[test]
    fn a_pref_that_is_there_but_unreadable_is_named_and_an_absent_one_is_not() {
        for absent in [
            json!({"ok": true}),
            json!({"prefs": {}}),
            json!({"prefs": {"all_notifications_prefs": null, "recents": ""}}),
            json!({"prefs": {"all_notifications_prefs": "null", "recents": " \n"}}),
            json!({"prefs": {"all_notifications_prefs": "{}", "recents": "{}"}}),
        ] {
            assert!(parse(&absent).unreadable.is_empty(), "{absent}");
        }
        let body = json!({"prefs": {"all_notifications_prefs": "not json", "recents": "{}"}});
        assert_eq!(parse(&body).unreadable, ["all_notifications_prefs"]);
        let body = json!({"prefs": {"all_notifications_prefs": "[]"}});
        assert_eq!(parse(&body).unreadable, ["all_notifications_prefs"]);
        // No longer a string: a change of Slack's, not an absence.
        let body = json!({"prefs": {
            "all_notifications_prefs": {"channels": {"C1": {"muted": true}}},
            "recents": "[",
        }});
        let prefs = parse(&body);
        assert_eq!(prefs.unreadable, ["all_notifications_prefs", "recents"]);
        assert!(prefs.muted.is_empty());
    }

    #[test]
    fn the_conversations_visited_last_come_first_and_the_pages_stay_out() {
        let recents = json!({
            "navigation": [
                {"id": "D1", "object_type": "DM", "timestamp": 1_792_618_752_480_u64},
                {"id": "Vall_threads", "object_type": "PAGE", "timestamp": 1_792_700_000_000_u64},
                {"id": "C2", "object_type": "CHANNEL", "timestamp": 1_792_650_000_000_u64},
                {"id": "D1", "object_type": "DM", "timestamp": 1_792_500_000_000_u64},
                {"object_type": "CHANNEL", "timestamp": 1_792_800_000_000_u64}
            ],
            "search": [{"query": "deploy", "timestamp": 1, "type": "messages"}]
        });
        let body = json!({"ok": true, "prefs": {"recents": recents.to_string()}});
        assert_eq!(parse(&body).recent, ["C2", "D1"]);
    }
}
