//! The user's own Slack preferences, read for one thing: the channels they
//! muted. `users.prefs.get` is a web-client endpoint and the mute list is
//! a JSON string inside it, so a missing or unreadable pref is an empty
//! set, which shows more rather than less.

use std::collections::HashSet;

use serde_json::Value;

use crate::api::{self, Client};

/// The ids of the muted conversations. Only a dead session is an error;
/// any other failure is reported and reads as "nothing muted".
pub fn muted_channels(client: &Client) -> Result<HashSet<String>, api::Error> {
    match client.call("users.prefs.get", &[]) {
        Ok(body) => Ok(parse_muted(&body)),
        Err(err @ api::Error::Auth { .. }) => Err(err),
        Err(err) => {
            eprintln!("warning: could not read the mute list: {err}");
            Ok(HashSet::new())
        }
    }
}

/// The muted conversations in a `users.prefs.get` response:
/// `prefs.all_notifications_prefs` is a JSON string whose `channels` map
/// carries `muted: true` per conversation.
pub fn parse_muted(body: &Value) -> HashSet<String> {
    let Some(raw) = body
        .pointer("/prefs/all_notifications_prefs")
        .and_then(Value::as_str)
    else {
        return HashSet::new();
    };
    let Ok(prefs) = serde_json::from_str::<Value>(raw) else {
        return HashSet::new();
    };
    prefs
        .get("channels")
        .and_then(Value::as_object)
        .map(|channels| {
            channels
                .iter()
                .filter(|(_, pref)| pref.get("muted").and_then(Value::as_bool) == Some(true))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn muted_conversations_are_read_out_of_the_nested_json_string() {
        let inner = json!({"channels": {
            "C1": {"muted": true, "desktop": "everything"},
            "C2": {"muted": false},
            "C3": {"desktop": "nothing"},
            "D4": {"muted": true}
        }, "global": {}});
        let body = json!({"ok": true, "prefs": {"all_notifications_prefs": inner.to_string()}});
        let muted = parse_muted(&body);
        assert_eq!(muted.len(), 2);
        assert!(muted.contains("C1") && muted.contains("D4"));
    }

    #[test]
    fn a_missing_or_broken_pref_mutes_nothing() {
        assert!(parse_muted(&json!({"ok": true, "prefs": {}})).is_empty());
        assert!(parse_muted(&json!({"prefs": {"all_notifications_prefs": "not json"}})).is_empty());
        assert!(
            parse_muted(&json!({"prefs": {"all_notifications_prefs": "{\"channels\": []}"}}))
                .is_empty()
        );
    }
}
