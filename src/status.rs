//! `slack status`: your own status (`users.profile.get`,
//! `users.profile.set`). Its end is Slack's `status_expiration`, so Slack
//! clears it at that time itself: nothing has to run then, and a Mac
//! asleep at that hour changes nothing.

use serde_json::json;

use crate::api::{self, Client};
use crate::emoji;
use crate::presence::{Profile, Status};

/// Your status now; None when none is set or it has ended. An answer
/// without a profile is an error, never "no status", which `--if-unset`
/// would take as leave to replace one.
pub fn get(client: &Client, now: i64) -> Result<Option<Status>, api::Error> {
    let body = client.call("users.profile.get", &[])?;
    if !body
        .get("profile")
        .is_some_and(serde_json::Value::is_object)
    {
        return Err(api::Error::Shape {
            method: "users.profile.get".to_string(),
            reason: "no profile in the answer".to_string(),
        });
    }
    Ok(Profile::parse(&body)
        .status
        .filter(|status| status.current(now)))
}

/// The most Slack keeps of a status text.
pub const MAX_TEXT: usize = 100;

/// Sets your status, until `expires` (in seconds) or for good, and
/// returns it as Slack stored it, None when its answer carries none. An
/// emoji Slack refuses is tried again under its other names, as for a
/// reaction.
pub fn set(
    client: &Client,
    text: &str,
    emoji: &str,
    expires: Option<i64>,
) -> Result<Option<Status>, api::Error> {
    let write = |emoji: &str| {
        let profile = json!({
            "status_text": text,
            "status_emoji": if emoji.is_empty() { String::new() } else { format!(":{emoji}:") },
            "status_expiration": expires.unwrap_or(0),
        });
        client.call("users.profile.set", &[("profile", &profile.to_string())])
    };
    let (_, body) = emoji::with_other_names(emoji, is_refused_emoji, write)?;
    Ok(Profile::parse(&body).status)
}

/// Slack documents `invalid_emoji_not_allowed` only, not the code for a
/// name it does not know; every code about the emoji says so in its name.
/// Deliberate: a policy refusal costs a few vain retries on a write that
/// fails anyway, which is cheaper than missing the unknown-name code.
fn is_refused_emoji(err: &api::Error) -> bool {
    matches!(err, api::Error::Slack { code, .. }
        if code.contains("emoji") && code != "must_clear_both_status_text_and_status_emoji")
}

pub fn clear(client: &Client) -> Result<(), api::Error> {
    set(client, "", "", None).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    use jiff::Zoned;

    use crate::when;

    /// A Thursday, 17:00 in Quebec.
    fn thursday() -> Zoned {
        "2026-10-29T17:00[America/Toronto]".parse().unwrap()
    }

    #[test]
    fn an_end_past_the_week_shows_its_date() {
        let status = Status {
            emoji: "palm_tree".to_string(),
            unicode: None,
            text: "Vacation".to_string(),
            expires: Some(
                when::parse("2026-11-15 07:30", &thursday())
                    .unwrap()
                    .timestamp()
                    .as_second(),
            ),
        };
        assert_eq!(
            status.describe(&thursday()),
            "🌴 Vacation (until Sun 15 Nov 07:30)"
        );
        let soon = Status {
            expires: Some(
                when::parse("sun 07:30", &thursday())
                    .unwrap()
                    .timestamp()
                    .as_second(),
            ),
            ..status
        };
        assert_eq!(soon.describe(&thursday()), "🌴 Vacation (until Sun 07:30)");
    }
}
