//! Desktop notifications from the screen: a message aimed at you, heard on
//! the socket while the terminal is in the background, is handed to the
//! terminal as an `OSC 777` notification, which the terminal posts and
//! brings you back to. While the screen can notify, it keeps a heartbeat
//! file fresh, which `slack --count --json` reports as `notifying` so a
//! status-bar fallback stays quiet meanwhile.
//! The rule, the text and the heartbeat's freshness are pure; the screen
//! writes the sequence.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::paths;
use crate::rtm::Posted;

/// How often the screen refreshes the heartbeat while it can notify.
pub const BEAT_EVERY: Duration = Duration::from_secs(30);
/// A heartbeat older than this is a screen that died without removing it.
const FRESH_FOR: Duration = Duration::from_secs(90);
/// A notification shows a line or two; the screen has the rest.
const BODY_CHARS: usize = 200;

/// The terminals known to post an `OSC 777` notification (the rxvt
/// extension): elsewhere the sequence is dropped without a word, and the
/// heartbeat would silence the fallback for nothing.
pub fn supported(term_program: Option<&str>) -> bool {
    matches!(term_program, Some("WezTerm" | "ghostty"))
}

/// Aimed at you by the `@ME` count's rule, as far as one message tells: a
/// new message from someone else in a DM or a group DM that is not muted,
/// or one that mentions you, `@here` and `@channel` included outside a
/// muted channel. A user group's mention is left out: who is in it is not
/// known here.
pub fn aimed(posted: &Posted, me: &str, direct: bool, muted: bool) -> bool {
    if posted.changed || posted.user.as_deref() == Some(me) {
        return false;
    }
    if direct && !muted {
        return true;
    }
    let text = posted.text.as_deref().unwrap_or_default();
    let named = text.contains(&format!("<@{me}>")) || text.contains(&format!("<@{me}|"));
    let everyone = ["<!here", "<!channel", "<!everyone"]
        .iter()
        .any(|special| text.contains(special));
    named || (everyone && !muted)
}

/// What the notification says.
#[derive(Debug, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    pub body: String,
}

impl Note {
    /// `conversation` is its label (`@jane`, `#dev`); `sender` the name of
    /// who wrote, left out of a DM, whose label already says it; `text`
    /// the message as plain text.
    pub fn new(conversation: &str, sender: Option<&str>, thread: bool, text: &str) -> Note {
        let title = if thread {
            format!("Thread in {conversation}")
        } else {
            conversation.to_string()
        };
        let text = if text.trim().is_empty() {
            "sent an attachment"
        } else {
            text
        };
        let body = match sender {
            Some(sender) => format!("{sender}: {text}"),
            None => text.to_string(),
        };
        Note { title, body }
    }

    /// The escape sequence a terminal turns into the notification.
    pub fn sequence(&self) -> String {
        format!(
            "\x1b]777;notify;{};{}\x1b\\",
            clean(&self.title, usize::MAX),
            clean(&self.body, BODY_CHARS)
        )
    }
}

/// One line the sequence can carry. A control character in a message
/// would end the sequence and reach the terminal as its own (deliberate:
/// every one goes, not only ESC and BEL), and a `;` would cut the body
/// short, since the terminal splits on it: it becomes the fullwidth one.
fn clean(text: &str, max: usize) -> String {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .collect();
    let line = words
        .into_iter()
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .replace(';', "\u{ff1b}");
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line,
    }
}

fn heartbeat() -> Result<PathBuf> {
    Ok(paths::cache_dir()?.join("notifying"))
}

/// Say the screen can notify, for `BEAT_EVERY` more.
pub fn beat() -> Result<()> {
    let path = heartbeat()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    paths::write_private(&path, b"")?;
    Ok(())
}

/// The screen can no longer notify: the fallback takes over at once.
pub fn stop() {
    // Deliberate: a heartbeat left behind goes stale within `FRESH_FOR`.
    if let Ok(path) = heartbeat() {
        let _ = fs::remove_file(path);
    }
}

/// Whether a screen is notifying now. Anything unreadable is a no: the
/// fallback then notifies, and twice is better than never.
pub fn covered() -> bool {
    heartbeat()
        .ok()
        .and_then(|path| fs::metadata(path).ok()?.modified().ok())
        .is_some_and(|beat| fresh(beat, SystemTime::now()))
}

fn fresh(beat: SystemTime, now: SystemTime) -> bool {
    // A beat from the future (a clock set back) is taken as fresh.
    now.duration_since(beat).map_or(true, |age| age < FRESH_FOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted(channel: &str, user: Option<&str>, text: &str) -> Posted {
        Posted {
            channel: channel.to_string(),
            ts: "2.0".to_string(),
            thread_ts: None,
            user: user.map(str::to_string),
            changed: false,
            text: Some(text.to_string()),
        }
    }

    #[test]
    fn a_dm_or_a_mention_is_aimed_at_you() {
        let me = "UME";
        let dm = posted("D1", Some("U2"), "lunch?");
        assert!(aimed(&dm, me, true, false));
        assert!(!aimed(&dm, me, true, true), "a muted DM stays out of @ME");
        let bot = posted("D2", None, "build failed");
        assert!(aimed(&bot, me, true, false), "a bot's DM counts too");

        let yours = posted("D1", Some(me), "on my way");
        assert!(
            !aimed(&yours, me, true, false),
            "yours, from another client"
        );
        let mut edit = posted("D1", Some("U2"), "lunch at noon?");
        edit.changed = true;
        assert!(
            !aimed(&edit, me, true, false),
            "an edit notifies of nothing"
        );

        let channel = posted("C1", Some("U2"), "deploy done");
        assert!(!aimed(&channel, me, false, false));
        let named = posted("C1", Some("U2"), "<@UME> can you look?");
        assert!(aimed(&named, me, false, false));
        assert!(
            aimed(&named, me, false, true),
            "a mention counts in a muted channel"
        );
        let labelled = posted("C1", Some("U2"), "<@UME|denis> ping");
        assert!(aimed(&labelled, me, false, false));
        let other = posted("C1", Some("U2"), "<@UMEX> can you look?");
        assert!(
            !aimed(&other, me, false, false),
            "someone whose id starts like yours"
        );

        let here = posted("C1", Some("U2"), "<!here> standup");
        assert!(aimed(&here, me, false, false));
        assert!(!aimed(&here, me, false, true), "not in a muted channel");
        let group = posted("C1", Some("U2"), "<!subteam^S1> review");
        assert!(
            !aimed(&group, me, false, false),
            "a group's members are not known"
        );
    }

    #[test]
    fn the_note_names_the_conversation_and_who_wrote() {
        assert_eq!(
            Note::new("@jane", None, false, "lunch?"),
            Note {
                title: "@jane".to_string(),
                body: "lunch?".to_string()
            }
        );
        let note = Note::new("#dev", Some("jane"), true, "@denis can you look?");
        assert_eq!(note.title, "Thread in #dev");
        assert_eq!(note.body, "jane: @denis can you look?");
        assert_eq!(
            Note::new("@jane", None, false, "  ").body,
            "sent an attachment"
        );
    }

    #[test]
    fn the_sequence_carries_one_clean_line() {
        let note = Note::new("#dev", Some("jane"), false, "a;b\nc\x1b]0;owned\x07  d");
        assert_eq!(
            note.sequence(),
            "\x1b]777;notify;#dev;jane: a\u{ff1b}b c ]0\u{ff1b}owned d\x1b\\"
        );
        let long = Note::new("@jane", None, false, &"é".repeat(300));
        let body = clean(&long.body, BODY_CHARS);
        assert_eq!(body.chars().count(), BODY_CHARS + 1, "cut on a character");
        assert!(body.ends_with('…'));
    }

    #[test]
    fn only_the_terminals_that_post_it_are_sent_it() {
        assert!(supported(Some("WezTerm")));
        assert!(supported(Some("ghostty")));
        assert!(!supported(Some("Apple_Terminal")));
        assert!(!supported(None));
    }

    #[test]
    fn a_heartbeat_goes_stale() {
        let now = SystemTime::now();
        assert!(fresh(now - Duration::from_secs(40), now));
        assert!(!fresh(now - FRESH_FOR, now));
        assert!(fresh(now + Duration::from_secs(5), now), "a clock set back");
    }
}
