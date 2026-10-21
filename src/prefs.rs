//! The user's own Slack preferences, read for two things: the
//! conversations they muted, and the emoji they use most. `users.prefs.get`
//! is a web-client endpoint and both are JSON strings inside it, so a
//! missing or unreadable pref reads as "nothing muted", which shows more
//! rather than less, and as no emoji used yet.

use std::collections::HashSet;

use serde_json::Value;

use crate::api::{self, Client};

#[derive(Debug, Default, Clone)]
pub struct Prefs {
    /// Muted conversations: the app badges them for mentions only.
    pub muted: HashSet<String>,
    /// Emoji names, the most used first: what the app's picker offers
    /// first, and the screen's `+` too.
    pub frequent_emoji: Vec<String>,
}

/// The preferences that shape the view. Only a dead session is an error;
/// any other failure is reported and reads as the defaults.
pub fn fetch(client: &Client) -> Result<Prefs, api::Error> {
    match client.call("users.prefs.get", &[]) {
        Ok(body) => Ok(parse(&body)),
        Err(err @ api::Error::Auth { .. }) => Err(err),
        Err(err) => {
            crate::warn::warn(format!(
                "could not read the notification preferences: {err}"
            ));
            Ok(Prefs::default())
        }
    }
}

pub fn parse(body: &Value) -> Prefs {
    Prefs {
        muted: muted(body),
        frequent_emoji: frequent_emoji(body),
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

/// `prefs.emoji_use` is a JSON string mapping each emoji name to how many
/// times it was used.
fn frequent_emoji(body: &Value) -> Vec<String> {
    let Some(parsed) = nested(body, "emoji_use") else {
        return Vec::new();
    };
    let Some(uses) = parsed.as_object() else {
        return Vec::new();
    };
    let mut ranked: Vec<(&String, u64)> = uses
        .iter()
        .map(|(name, count)| (name, count.as_u64().unwrap_or(0)))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.into_iter().map(|(name, _)| name.clone()).collect()
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
            assert!(prefs.frequent_emoji.is_empty());
        }
    }

    #[test]
    fn the_most_used_emoji_come_first() {
        let uses = json!({"wink": 346, "+1": 415, "yolo": 7, "joy": 65, "grin": 65});
        let body = json!({"ok": true, "prefs": {"emoji_use": uses.to_string()}});
        assert_eq!(
            parse(&body).frequent_emoji,
            ["+1", "wink", "grin", "joy", "yolo"]
        );
    }
}
