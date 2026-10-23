//! What a person is up to right now, for `slack who` and the screen:
//! whether they are at their screen (`users.getPresence`; the screen
//! hears it on the socket instead), their status and timezone
//! (`users.info`, read fresh, since the name cache keeps a profile for
//! good and a status lasts hours), and whether their notifications are
//! paused (`dnd.teamInfo`, one call for everyone). Probed live on
//! 2026-10-23.

use std::collections::BTreeMap;

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use serde::Serialize;
use serde_json::Value;

use crate::api::{self, Client};
use crate::{emoji, par};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Active,
    Away,
}

impl Presence {
    pub fn parse(text: &str) -> Option<Presence> {
        match text {
            "active" => Some(Presence::Active),
            "away" => Some(Presence::Away),
            _ => None,
        }
    }
}

/// A status as set in Slack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Slack's emoji name, without the colons; empty for none.
    pub emoji: String,
    /// The emoji's character as Slack gives it, for a name the emoji
    /// table does not know (`spiral_calendar_pad`, Google Calendar's).
    #[serde(skip)]
    pub unicode: Option<String>,
    pub text: String,
    /// When Slack clears it, in seconds; None when it stays.
    pub expires: Option<i64>,
}

impl Status {
    /// The emoji's character; None for a custom one, or none set.
    pub fn character(&self) -> Option<String> {
        self.unicode
            .clone()
            .or_else(|| emoji::glyph(&self.emoji).map(str::to_string))
    }

    /// The emoji as drawn: its character, `:name:` for a custom one.
    pub fn glyph(&self) -> Option<String> {
        if self.emoji.is_empty() {
            return None;
        }
        Some(
            self.character()
                .unwrap_or_else(|| format!(":{}:", self.emoji)),
        )
    }
}

/// `1f5d3-fe0f` as the character it spells.
fn from_hex(code: &str) -> Option<String> {
    code.split('-')
        .map(|part| u32::from_str_radix(part, 16).ok().and_then(char::from_u32))
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub status: Option<Status>,
    /// The IANA name, `Europe/Brussels`.
    pub tz: Option<String>,
}

impl Profile {
    /// From a `users.info` user.
    fn parse(user: &Value) -> Profile {
        let profile = user.get("profile");
        let field = |key: &str| {
            profile
                .and_then(|profile| profile.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        let emoji = field("status_emoji").trim_matches(':').to_string();
        let text = field("status_text").to_string();
        let expires = profile
            .and_then(|profile| profile.get("status_expiration"))
            .and_then(Value::as_i64)
            .filter(|at| *at > 0);
        let unicode = profile
            .and_then(|profile| profile.pointer("/status_emoji_display_info/0/unicode"))
            .and_then(Value::as_str)
            .and_then(from_hex);
        let status = (!emoji.is_empty() || !text.is_empty()).then_some(Status {
            emoji,
            unicode,
            text,
            expires,
        });
        let tz = user
            .get("tz")
            .and_then(Value::as_str)
            .filter(|tz| !tz.is_empty())
            .map(str::to_string);
        Profile { status, tz }
    }
}

/// The do-not-disturb window Slack has scheduled next, in seconds: it
/// may not have started yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dnd {
    pub start: i64,
    pub end: i64,
}

/// What is known of a person; each part None until read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Person {
    pub presence: Option<Presence>,
    pub profile: Option<Profile>,
    pub dnd: Option<Dnd>,
}

impl Person {
    /// What a newer read says, keeping what it could not read.
    pub fn update(&mut self, newer: Person) {
        if newer.presence.is_some() {
            self.presence = newer.presence;
        }
        // The window comes with the profile: none read then is none set.
        if newer.profile.is_some() {
            self.profile = newer.profile;
            self.dnd = newer.dnd;
        }
    }

    /// The status, unless it expired since it was read.
    pub fn status(&self, now: i64) -> Option<&Status> {
        self.profile
            .as_ref()?
            .status
            .as_ref()
            .filter(|status| status.expires.is_none_or(|at| at > now))
    }

    /// When their notifications come back, while they are paused.
    pub fn paused_until(&self, now: i64) -> Option<i64> {
        self.dnd
            .filter(|dnd| dnd.start <= now && now < dnd.end)
            .map(|dnd| dnd.end)
    }

    /// Their clock, when their timezone is known.
    pub fn local(&self, now: &Zoned) -> Option<Zoned> {
        let tz = TimeZone::get(self.profile.as_ref()?.tz.as_deref()?).ok()?;
        Some(now.with_time_zone(tz))
    }

    /// What is said beside their name: the status, their clock when it
    /// is not yours, their paused notifications. The presence is left to
    /// the caller, which draws it as a dot.
    pub fn notes(&self, now: &Zoned) -> Vec<String> {
        let seconds = now.timestamp().as_second();
        let mut notes = Vec::new();
        if let Some(status) = self.status(seconds) {
            let mut note = match status.glyph() {
                Some(glyph) if status.text.is_empty() => glyph,
                Some(glyph) => format!("{glyph} {}", status.text),
                None => status.text.clone(),
            };
            if let Some(at) = status.expires {
                note.push_str(&format!(" (until {})", clock(at, now)));
            }
            notes.push(note);
        }
        if let Some(local) = self.local(now)
            && local.offset() != now.offset()
        {
            let format = if local.date() == now.date() {
                "%H:%M"
            } else {
                "%a %H:%M"
            };
            let city = local
                .time_zone()
                .iana_name()
                .map_or_else(|| "their timezone".to_string(), city);
            notes.push(format!("{} in {city}", local.strftime(format)));
        }
        if let Some(end) = self.paused_until(seconds) {
            notes.push(format!("notifications paused until {}", clock(end, now)));
        }
        notes
    }
}

/// A time to come, on your clock, with the day when it is not today.
fn clock(at: i64, now: &Zoned) -> String {
    let Ok(stamp) = Timestamp::from_second(at) else {
        return at.to_string();
    };
    let zoned = stamp.to_zoned(now.time_zone().clone());
    let format = if zoned.date() == now.date() {
        "%H:%M"
    } else {
        "%a %H:%M"
    };
    zoned.strftime(format).to_string()
}

/// The place a timezone is named after: `America/Los_Angeles` is Los
/// Angeles.
fn city(tz: &str) -> String {
    tz.rsplit('/').next().unwrap_or(tz).replace('_', " ")
}

/// The presence of each of these people, a few at a time. A person
/// Slack would not answer for is left out; a dead session fails.
pub fn presences(
    client: &Client,
    users: &[String],
) -> Result<BTreeMap<String, Presence>, api::Error> {
    const METHOD: &str = "users.getPresence";
    let read = par::map(users.to_vec(), |user| {
        let presence = client.call(METHOD, &[("user", &user)]).and_then(|body| {
            body.get("presence")
                .and_then(Value::as_str)
                .and_then(Presence::parse)
                .ok_or_else(|| shape(METHOD, "missing `presence`"))
        });
        (user, presence)
    });
    let mut presences = BTreeMap::new();
    for (user, presence) in read {
        if let Some(presence) = kept(presence)? {
            presences.insert(user, presence);
        }
    }
    Ok(presences)
}

/// The status, timezone and paused notifications of each of these
/// people, without their presence. Someone Slack would not describe is
/// left without a profile; a dead session fails.
pub fn profiles(client: &Client, users: &[String]) -> Result<BTreeMap<String, Person>, api::Error> {
    const METHOD: &str = "users.info";
    let read = par::map(users.to_vec(), |user| {
        let profile = client.call(METHOD, &[("user", &user)]).and_then(|body| {
            body.get("user")
                .map(Profile::parse)
                .ok_or_else(|| shape(METHOD, "missing `user`"))
        });
        (user, profile)
    });
    let mut people = BTreeMap::new();
    for (user, profile) in read {
        let profile = kept(profile)?;
        people.insert(
            user,
            Person {
                profile,
                ..Person::default()
            },
        );
    }
    if users.is_empty() {
        return Ok(people);
    }
    let body = kept(client.call("dnd.teamInfo", &[("users", &users.join(","))]))?;
    for (user, dnd) in body.as_ref().map(parse_dnd).unwrap_or_default() {
        if let Some(person) = people.get_mut(&user)
            && person.profile.is_some()
        {
            person.dnd = Some(dnd);
        }
    }
    Ok(people)
}

/// The windows of `dnd.teamInfo`, for those who have one.
fn parse_dnd(body: &Value) -> BTreeMap<String, Dnd> {
    let Some(users) = body.get("users").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    users
        .iter()
        .filter(|(_, dnd)| dnd.get("dnd_enabled").and_then(Value::as_bool) == Some(true))
        .filter_map(|(user, dnd)| {
            let at = |key: &str| dnd.get(key).and_then(Value::as_i64);
            let window = Dnd {
                start: at("next_dnd_start_ts")?,
                end: at("next_dnd_end_ts")?,
            };
            Some((user.clone(), window))
        })
        .collect()
}

/// What one person's lookup gave: a failure leaves that person out,
/// unless the session is dead.
fn kept<T>(result: Result<T, api::Error>) -> Result<Option<T>, api::Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err @ api::Error::Auth { .. }) => Err(err),
        Err(_) => Ok(None),
    }
}

fn shape(method: &str, reason: &str) -> api::Error {
    api::Error::Shape {
        method: method.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 2026-10-23 10:00 in Quebec, 16:00 in Brussels.
    fn now() -> Zoned {
        "2026-10-23T10:00:00-04:00[America/Toronto]"
            .parse()
            .unwrap()
    }

    fn person(status: Option<Status>, tz: Option<&str>) -> Person {
        Person {
            presence: Some(Presence::Active),
            profile: Some(Profile {
                status,
                tz: tz.map(str::to_string),
            }),
            dnd: None,
        }
    }

    fn status(emoji: &str, text: &str, expires: Option<i64>) -> Status {
        Status {
            emoji: emoji.to_string(),
            unicode: None,
            text: text.to_string(),
            expires,
        }
    }

    #[test]
    fn profile_reads_status_and_timezone() {
        let user = json!({
            "tz": "Europe/Brussels",
            "profile": {"status_text": "Certified personal trainer", "status_emoji": ":running:", "status_expiration": 0},
        });
        assert_eq!(
            Profile::parse(&user),
            Profile {
                status: Some(status("running", "Certified personal trainer", None)),
                tz: Some("Europe/Brussels".to_string()),
            }
        );
    }

    #[test]
    fn a_renamed_emoji_takes_the_character_slack_gives() {
        let user = json!({"profile": {
            "status_text": "In a meeting",
            "status_emoji": ":spiral_calendar_pad:",
            "status_emoji_display_info": [{"emoji_name": "spiral_calendar_pad", "unicode": "1f5d3-fe0f"}],
        }});
        let status = Profile::parse(&user).status.unwrap();
        assert_eq!(status.glyph().as_deref(), Some("🗓️"));
        let custom = status_with_name("party-parrot");
        assert_eq!(custom.character(), None);
        assert_eq!(custom.glyph().as_deref(), Some(":party-parrot:"));
    }

    fn status_with_name(name: &str) -> Status {
        status(name, "", None)
    }

    #[test]
    fn an_empty_status_is_none() {
        let user =
            json!({"profile": {"status_text": "", "status_emoji": "", "status_expiration": 0}});
        assert_eq!(Profile::parse(&user), Profile::default());
    }

    #[test]
    fn dnd_keeps_the_enabled_windows() {
        let body = json!({"users": {
            "U1": {"dnd_enabled": true, "next_dnd_start_ts": 100, "next_dnd_end_ts": 200},
            "U2": {"dnd_enabled": false, "next_dnd_start_ts": 1, "next_dnd_end_ts": 2},
            "U3": {"dnd_enabled": true},
        }});
        let windows = parse_dnd(&body);
        assert_eq!(windows.len(), 1);
        assert_eq!(
            windows["U1"],
            Dnd {
                start: 100,
                end: 200
            }
        );
    }

    #[test]
    fn notifications_are_paused_inside_the_window_only() {
        let mut someone = person(None, None);
        someone.dnd = Some(Dnd {
            start: 100,
            end: 200,
        });
        assert_eq!(someone.paused_until(99), None);
        assert_eq!(someone.paused_until(100), Some(200));
        assert_eq!(someone.paused_until(200), None);
    }

    #[test]
    fn an_expired_status_is_gone() {
        let someone = person(Some(status("palm_tree", "Off", Some(100))), None);
        assert!(someone.status(99).is_some());
        assert!(someone.status(100).is_none());
    }

    #[test]
    fn notes_give_the_status_their_clock_and_the_pause() {
        let now = now();
        let seconds = now.timestamp().as_second();
        let mut someone = person(
            Some(status("running", "Certified personal trainer", None)),
            Some("Europe/Brussels"),
        );
        someone.dnd = Some(Dnd {
            start: seconds - 60,
            end: seconds + 3600,
        });
        assert_eq!(
            someone.notes(&now),
            [
                "🏃 Certified personal trainer",
                "16:00 in Brussels",
                "notifications paused until 11:00",
            ]
        );
    }

    #[test]
    fn notes_leave_out_a_clock_like_yours() {
        let someone = person(None, Some("America/New_York"));
        assert!(someone.notes(&now()).is_empty());
    }

    #[test]
    fn notes_give_the_day_when_it_is_not_yours() {
        let someone = person(None, Some("Pacific/Auckland"));
        assert_eq!(someone.notes(&now()), ["Sat 03:00 in Auckland"]);
        let someone = person(None, Some("America/Los_Angeles"));
        assert_eq!(someone.notes(&now()), ["07:00 in Los Angeles"]);
    }

    #[test]
    fn notes_give_when_a_status_ends() {
        let now = now();
        let tomorrow = now.timestamp().as_second() + 86_400;
        let someone = person(Some(status("palm_tree", "", Some(tomorrow))), None);
        assert_eq!(someone.notes(&now), ["🌴 (until Sat 10:00)"]);
        let someone = person(Some(status("custom-thing", "Busy", None)), None);
        assert_eq!(someone.notes(&now), [":custom-thing: Busy"]);
    }

    #[test]
    fn an_update_keeps_what_it_could_not_read() {
        let mut known = person(Some(status("running", "Out", None)), None);
        known.dnd = Some(Dnd { start: 1, end: 2 });
        known.update(Person {
            presence: Some(Presence::Away),
            ..Person::default()
        });
        assert_eq!(known.presence, Some(Presence::Away));
        assert!(known.profile.is_some() && known.dnd.is_some());
        known.update(person(None, None));
        assert_eq!(known.presence, Some(Presence::Active));
        assert_eq!(known.dnd, None);
    }
}
