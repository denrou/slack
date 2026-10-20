//! The user's own Slack preferences, read for one thing: the conversations
//! they muted. `users.prefs.get` is a web-client endpoint and the
//! per-channel settings are a JSON string inside it, so a missing or
//! unreadable pref reads as "nothing muted", which shows more rather than
//! less.

use std::collections::HashSet;

use serde_json::Value;

use crate::api::{self, Client};

#[derive(Debug, Default, Clone)]
pub struct Prefs {
    /// Muted conversations: the app badges them for mentions only.
    pub muted: HashSet<String>,
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

/// `prefs.all_notifications_prefs` is a JSON string whose `channels` map
/// carries `muted: true` per conversation, among its notification levels.
pub fn parse(body: &Value) -> Prefs {
    let Some(raw) = body
        .pointer("/prefs/all_notifications_prefs")
        .and_then(Value::as_str)
    else {
        return Prefs::default();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return Prefs::default();
    };
    let Some(channels) = parsed.get("channels").and_then(Value::as_object) else {
        return Prefs::default();
    };
    let mut prefs = Prefs::default();
    for (id, pref) in channels {
        if pref.get("muted").and_then(Value::as_bool) == Some(true) {
            prefs.muted.insert(id.clone());
        }
    }
    prefs
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
        }
    }
}
