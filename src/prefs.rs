//! The user's own Slack preferences, read for two things: the
//! conversations they muted and the conversations they visited last.
//! `users.prefs.get` is a web-client endpoint and both are JSON strings
//! inside it, so a missing or unreadable pref reads as "nothing muted",
//! which shows more rather than less, and as nothing visited.

use std::collections::HashSet;

use serde_json::Value;

use crate::api::{self, Client};

#[derive(Debug, Default, Clone)]
pub struct Prefs {
    /// Muted conversations: the app badges them for mentions only.
    pub muted: HashSet<String>,
    /// Conversation ids, the last visited first: the app's own recent
    /// list, whose DMs `slack who` takes by default.
    pub recent: Vec<String>,
}

/// The preferences that shape the view, None when they could not be read.
/// Only a dead session is an error; any other failure is reported.
pub fn fetch(client: &Client) -> Result<Option<Prefs>, api::Error> {
    match client.call("users.prefs.get", &[]) {
        Ok(body) => Ok(Some(parse(&body))),
        Err(err @ api::Error::Auth { .. }) => Err(err),
        Err(err) => {
            crate::warn::warn(format!(
                "could not read the notification preferences: {err}"
            ));
            Ok(None)
        }
    }
}

pub fn parse(body: &Value) -> Prefs {
    Prefs {
        muted: muted(body),
        recent: recent(body),
    }
}

/// A pref that holds JSON in a string.
fn nested(body: &Value, name: &str) -> Option<Value> {
    let raw = body.get("prefs")?.get(name)?.as_str()?;
    serde_json::from_str(raw).ok()
}

/// `prefs.all_notifications_prefs` is a JSON string whose `channels` map
/// carries `muted: true` per conversation, among its notification levels.
fn muted(body: &Value) -> HashSet<String> {
    let Some(parsed) = nested(body, "all_notifications_prefs") else {
        return HashSet::new();
    };
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
fn recent(body: &Value) -> Vec<String> {
    let Some(parsed) = nested(body, "recents") else {
        return Vec::new();
    };
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
