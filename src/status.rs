//! `slack status`: your own status (`users.profile.get`,
//! `users.profile.set`). Its end is Slack's `status_expiration`, so Slack
//! clears it at that time itself: nothing has to run then, and a Mac
//! asleep at that hour changes nothing.

use anyhow::{Result, bail};
use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::{Span, ToSpan, Zoned};
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

/// Sets your status, until `expires` (in seconds) or for good, and
/// returns the emoji name Slack took: one it refuses is tried again under
/// its other names, as for a reaction.
pub fn set(
    client: &Client,
    text: &str,
    emoji: &str,
    expires: Option<i64>,
) -> Result<String, api::Error> {
    let call = |emoji: &str| {
        let profile = json!({
            "status_text": text,
            "status_emoji": if emoji.is_empty() { String::new() } else { format!(":{emoji}:") },
            "status_expiration": expires.unwrap_or(0),
        });
        client
            .call("users.profile.set", &[("profile", &profile.to_string())])
            .map(drop)
    };
    let refused = match call(emoji) {
        Err(err) if is_refused_emoji(&err) => err,
        result => return result.map(|()| emoji.to_string()),
    };
    for other in emoji::other_names(emoji) {
        match call(&other) {
            Err(err) if is_refused_emoji(&err) => {}
            result => return result.map(|()| other),
        }
    }
    Err(refused)
}

/// Slack documents `invalid_emoji_not_allowed` only, not the code for a
/// name it does not know; every code about the emoji says so in its name.
fn is_refused_emoji(err: &api::Error) -> bool {
    matches!(err, api::Error::Slack { code, .. }
        if code.contains("emoji") && code != "must_clear_both_status_text_and_status_emoji")
}

pub fn clear(client: &Client) -> Result<(), api::Error> {
    set(client, "", "", None).map(drop)
}

/// The moment `--until` names, on the local clock: a duration (`45m`,
/// `2h`, `1h30m`), a time (`07:30`, the next one), a day and a time
/// (`mon 07:30`, the next Monday at 07:30, today while that time is
/// ahead), or a date and a time (`2026-11-02 07:30`).
pub fn until(spec: &str, now: &Zoned) -> Result<Zoned> {
    let spec = spec.trim();
    // The date first: jiff reads the time out of a whole date and time.
    let at = if let Ok(at) = spec.parse::<DateTime>() {
        at.to_zoned(now.time_zone().clone())?
    } else if let Ok(time) = spec.parse::<Time>() {
        next(now, time, |_| true)?
    } else if let Some((day, time)) = spec.split_once(' ')
        && let (Some(day), Ok(time)) = (weekday(day), time.trim().parse::<Time>())
    {
        next(now, time, |date| date.weekday() == day)?
    } else if let Ok(span) = spec.parse::<Span>() {
        now.checked_add(span)?
    } else {
        bail!(
            "`{spec}` is not a time: give a duration (45m, 2h), a time (07:30), a day and a time (mon 07:30) or a date and a time (2026-11-02 07:30)"
        );
    };
    if at <= *now {
        bail!("`{spec}` is already past");
    }
    Ok(at)
}

/// The first day from today on that `day` accepts, at `time`, still ahead.
fn next(now: &Zoned, time: Time, day: impl Fn(Date) -> bool) -> Result<Zoned> {
    for days in 0..=7 {
        let date = now.date().checked_add(days.days())?;
        if !day(date) {
            continue;
        }
        let at = date.to_datetime(time).to_zoned(now.time_zone().clone())?;
        if at > *now {
            return Ok(at);
        }
    }
    bail!("no such day in the week ahead")
}

fn weekday(name: &str) -> Option<Weekday> {
    let name = name.to_lowercase();
    [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ]
    .into_iter()
    .find(|day| {
        let full = format!("{day:?}").to_lowercase();
        name.len() >= 3 && full.starts_with(&name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Thursday, 17:00 in Quebec.
    fn thursday() -> Zoned {
        "2026-10-29T17:00[America/Toronto]".parse().unwrap()
    }

    fn at(spec: &str, now: &Zoned) -> String {
        until(spec, now)
            .unwrap()
            .strftime("%a %F %H:%M")
            .to_string()
    }

    #[test]
    fn a_time_is_the_next_one() {
        assert_eq!(at("07:30", &thursday()), "Fri 2026-10-30 07:30");
        assert_eq!(at("18:15", &thursday()), "Thu 2026-10-29 18:15");
    }

    #[test]
    fn a_day_is_the_next_one_today_included_while_ahead() {
        let friday: Zoned = "2026-10-30T17:00[America/Toronto]".parse().unwrap();
        assert_eq!(at("mon 07:30", &friday), "Mon 2026-11-02 07:30");
        assert_eq!(at("Monday 07:30", &friday), "Mon 2026-11-02 07:30");
        assert_eq!(at("fri 18:00", &friday), "Fri 2026-10-30 18:00");
        assert_eq!(at("fri 08:00", &friday), "Fri 2026-11-06 08:00");
    }

    #[test]
    fn a_duration_counts_from_now() {
        assert_eq!(at("45m", &thursday()), "Thu 2026-10-29 17:45");
        assert_eq!(at("1h30m", &thursday()), "Thu 2026-10-29 18:30");
        assert_eq!(at("2d", &thursday()), "Sat 2026-10-31 17:00");
    }

    #[test]
    fn a_date_and_a_time_are_taken_as_they_are() {
        assert_eq!(at("2026-11-02 07:30", &thursday()), "Mon 2026-11-02 07:30");
        assert_eq!(at("2026-11-02T07:30", &thursday()), "Mon 2026-11-02 07:30");
    }

    #[test]
    fn the_clock_change_keeps_the_hour() {
        // Quebec leaves daylight saving time on Sunday 2026-11-01.
        let saturday: Zoned = "2026-10-31T17:00[America/Toronto]".parse().unwrap();
        let monday = until("mon 07:30", &saturday).unwrap();
        assert_eq!(
            monday.strftime("%F %H:%M %z").to_string(),
            "2026-11-02 07:30 -0500"
        );
    }

    #[test]
    fn the_past_and_nonsense_are_refused() {
        assert!(until("2026-10-01 07:30", &thursday()).is_err());
        assert!(until("-2h", &thursday()).is_err());
        assert!(until("soon", &thursday()).is_err());
        assert!(until("mo 07:30", &thursday()).is_err());
        assert!(until("mon", &thursday()).is_err());
    }
}
