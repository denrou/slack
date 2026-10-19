//! The default view: what needs you, grouped the way a person answers it.
//! The model is built first and rendered second, so `--json` and a later
//! TUI read the same thing the text rendering does.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use jiff::{Timestamp, Zoned};
use serde::Serialize;

use crate::api::{self, Client};
use crate::counts::{Conversation, Counts, Kind};
use crate::history::{self, Message, compare_ts};
use crate::names::Names;
use crate::style::Style;
use crate::threads::{self, Thread, ThreadList};
use crate::{mrkdwn, par};

#[derive(Serialize)]
pub struct View {
    /// DMs and group DMs with unreads: someone wrote to you.
    pub direct_messages: Vec<Entry>,
    /// Channels where you were named.
    pub mentions: Vec<Entry>,
    /// Every other conversation with unreads. The text rendering hides
    /// them unless `--all`; the JSON always carries them.
    pub channels: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<ThreadsView>,
    #[serde(skip)]
    pub all: bool,
}

#[derive(Serialize)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    /// `#channel`, `@person`, or the people of a group DM.
    pub name: String,
    pub mention_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// None when the messages were not read: a hidden channel, or a read
    /// that failed (reported on stderr).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
}

/// The Threads view: Slack's summary, and the list when it could be read.
#[derive(Serialize)]
pub struct ThreadsView {
    pub has_unreads: bool,
    pub mention_count: u64,
    /// None when the thread list could not be read; the summary stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<Thread>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
}

impl ThreadsView {
    fn is_unread(&self) -> bool {
        self.has_unreads
            || self.mention_count > 0
            || self.items.as_ref().is_some_and(|items| !items.is_empty())
    }
}

impl View {
    /// The conversations whose messages were shown, with the newest
    /// message each had and whether it was a DM or a mention: what
    /// `slack read` may mark as read.
    pub fn shown(&self) -> impl Iterator<Item = (&str, &str, bool)> {
        let attention = self
            .direct_messages
            .iter()
            .chain(&self.mentions)
            .map(|entry| (entry, true));
        let channels = self
            .channels
            .iter()
            .filter(|_| self.all)
            .map(|entry| (entry, false));
        attention
            .chain(channels)
            .filter(|(entry, _)| entry.messages.is_some())
            .filter_map(|(entry, attention)| {
                Some((entry.id.as_str(), entry.latest.as_deref()?, attention))
            })
    }
}

pub fn build(
    client: &Client,
    counts: &Counts,
    names: &mut Names,
    all: bool,
) -> Result<View, api::Error> {
    let unread: Vec<&Conversation> = counts.unread().collect();
    names.resolve_channels(client, unread.iter().map(|c| c.id.clone()))?;

    let (direct_messages, mentions, channels) = sections(&unread);
    let mut to_read: Vec<&Conversation> = Vec::new();
    to_read.extend(direct_messages.iter().copied());
    to_read.extend(mentions.iter().copied());
    if all {
        to_read.extend(channels.iter().copied());
    }

    let cursors: Vec<(String, Option<String>)> = to_read
        .iter()
        .map(|c| (c.id.clone(), c.last_read.clone()))
        .collect();
    let pages = par::map(cursors, |(id, last_read)| {
        let page = history::fetch(client, &id, last_read.as_deref(), history::DEFAULT_LIMIT);
        (id, page)
    });
    let mut pages_by_id = BTreeMap::new();
    for (id, page) in pages {
        match page {
            Ok(page) => {
                pages_by_id.insert(id, page);
            }
            Err(err @ api::Error::Auth { .. }) => return Err(err),
            Err(err) => eprintln!(
                "warning: could not read {}: {err}",
                names.channel_label(&id)
            ),
        }
    }

    let threads = match counts.threads {
        None => None,
        Some(summary) => {
            let list = if counts.threads_unread() {
                threads::fetch(client, threads::REPLY_LIMIT)?
            } else {
                Some(ThreadList {
                    items: Vec::new(),
                    has_more: false,
                })
            };
            Some(ThreadsView {
                has_unreads: summary.has_unreads,
                mention_count: summary.mention_count,
                has_more: list.as_ref().is_some_and(|list| list.has_more),
                items: list.map(|list| list.items),
            })
        }
    };

    // Everyone the view will name: the people behind the DMs, every
    // author, everyone mentioned, and the channels the threads live in.
    let thread_items: &[Thread] = threads
        .as_ref()
        .and_then(|threads| threads.items.as_deref())
        .unwrap_or(&[]);
    names.resolve_channels(client, thread_items.iter().map(|t| t.channel.clone()))?;
    let mut people = BTreeSet::new();
    people.extend(
        unread
            .iter()
            .filter_map(|c| names.channel(&c.id)?.user.clone()),
    );
    for message in pages_by_id.values().flat_map(|page| &page.messages) {
        collect_people(&mut people, message);
    }
    for thread in thread_items {
        collect_people(&mut people, &thread.root);
        for reply in &thread.unread_replies {
            collect_people(&mut people, reply);
        }
    }
    names.resolve_users(client, people)?;

    let names = &*names;
    let mut entries = |conversations: Vec<&Conversation>| -> Vec<Entry> {
        conversations
            .into_iter()
            .map(|c| {
                let page = pages_by_id.remove(&c.id);
                Entry {
                    id: c.id.clone(),
                    kind: c.kind,
                    name: names.channel_label(&c.id),
                    mention_count: c.mention_count,
                    latest: c.latest.clone(),
                    has_more: page.as_ref().is_some_and(|page| page.has_more),
                    messages: page.map(|page| page.messages),
                }
            })
            .collect()
    };
    Ok(View {
        direct_messages: entries(direct_messages),
        mentions: entries(mentions),
        channels: entries(channels),
        threads,
        all,
    })
}

/// Split the unread conversations into the three sections, most recent
/// activity first (mentions: most mentions first).
fn sections<'a>(
    unread: &[&'a Conversation],
) -> (
    Vec<&'a Conversation>,
    Vec<&'a Conversation>,
    Vec<&'a Conversation>,
) {
    let mut direct_messages = Vec::new();
    let mut mentions = Vec::new();
    let mut channels = Vec::new();
    for conversation in unread {
        match conversation.kind {
            Kind::Im | Kind::Mpim => direct_messages.push(*conversation),
            Kind::Channel if conversation.mention_count > 0 => mentions.push(*conversation),
            Kind::Channel => channels.push(*conversation),
        }
    }
    let newest_first = |a: &&Conversation, b: &&Conversation| {
        compare_ts(
            b.latest.as_deref().unwrap_or("0"),
            a.latest.as_deref().unwrap_or("0"),
        )
    };
    direct_messages.sort_by(newest_first);
    mentions.sort_by(|a, b| {
        b.mention_count
            .cmp(&a.mention_count)
            .then_with(|| newest_first(a, b))
    });
    channels.sort_by(newest_first);
    (direct_messages, mentions, channels)
}

fn collect_people(people: &mut BTreeSet<String>, message: &Message) {
    people.extend(message.user.clone());
    people.extend(mentioned_users(&message.text));
}

/// The user ids named in a message, so their names can be looked up.
fn mentioned_users(text: &str) -> Vec<String> {
    text.match_indices("<@")
        .filter_map(|(at, _)| {
            let rest = &text[at + 2..];
            let end = rest.find(['>', '|'])?;
            let id = &rest[..end];
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
                .then(|| id.to_string())
        })
        .collect()
}

pub fn render(
    view: &View,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    out: &mut impl Write,
) -> io::Result<()> {
    render_at(view, names, style, me, &Zoned::now(), out)
}

fn render_at(
    view: &View,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut impl Write,
) -> io::Result<()> {
    let sections: [(&str, &[Entry], bool); 3] = [
        ("Direct messages", &view.direct_messages, true),
        ("Mentions", &view.mentions, true),
        ("Channels", &view.channels, view.all),
    ];
    let mut printed = false;
    for (title, entries, shown) in sections {
        if entries.is_empty() || !shown {
            continue;
        }
        if printed {
            writeln!(out)?;
        }
        printed = true;
        writeln!(out, "{}", style.heading(title))?;
        for entry in entries {
            render_entry(entry, names, style, me, now, out)?;
        }
    }
    if let Some(threads) = view.threads.as_ref().filter(|t| t.is_unread()) {
        if printed {
            writeln!(out)?;
        }
        printed = true;
        writeln!(out, "{}", style.heading("Threads"))?;
        match &threads.items {
            None => writeln!(
                out,
                "  {}",
                style.dim("Unread replies in followed threads (the list could not be read)")
            )?,
            Some(items) if items.is_empty() => {
                writeln!(out, "  {}", style.dim("Unread replies in followed threads"))?;
            }
            Some(items) => {
                for thread in items {
                    render_thread(thread, names, style, me, now, out)?;
                }
                if threads.has_more {
                    writeln!(out, "  {}", style.dim("more threads not shown"))?;
                }
            }
        }
    }
    let hidden = if view.all { 0 } else { view.channels.len() };
    if !printed {
        let line = if hidden > 0 {
            "Nothing needs you."
        } else {
            "All caught up."
        };
        writeln!(out, "{line}")?;
    }
    if hidden > 0 {
        let plural = if hidden == 1 { "" } else { "s" };
        eprintln!(
            "{}",
            style.dim(&format!(
                "{hidden} channel{plural} with unreads hidden; `slack -a` shows them"
            ))
        );
    }
    Ok(())
}

fn render_entry(
    entry: &Entry,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut impl Write,
) -> io::Result<()> {
    let count = match &entry.messages {
        Some(messages) => format!(
            "{}{} unread",
            messages.len(),
            if entry.has_more { "+" } else { "" }
        ),
        None => "unread".to_string(),
    };
    let mentions = match entry.mention_count {
        0 => String::new(),
        1 => ", 1 mention".to_string(),
        n => format!(", {n} mentions"),
    };
    writeln!(
        out,
        "  {}  {}",
        style.bold(&entry.name),
        style.dim(&format!("({count}{mentions})"))
    )?;
    match &entry.messages {
        Some(messages) => render_messages(
            messages,
            entry.has_more,
            entry.kind != Kind::Im,
            names,
            style,
            me,
            now,
            out,
        ),
        None => Ok(()),
    }
}

fn render_thread(
    thread: &Thread,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut impl Write,
) -> io::Result<()> {
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    let root = mrkdwn::render(&thread.root.text, &user, &channel, &Style::plain());
    let count = match thread.unread_replies.len() {
        1 => "1 new reply".to_string(),
        n => format!("{n}{} new replies", if thread.has_more { "+" } else { "" }),
    };
    writeln!(
        out,
        "  {} › {}  {}",
        style.bold(&names.channel_label(&thread.channel)),
        excerpt(&root, 60),
        style.dim(&format!("({count})"))
    )?;
    render_messages(
        &thread.unread_replies,
        false,
        true,
        names,
        style,
        me,
        now,
        out,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_messages(
    messages: &[Message],
    has_more: bool,
    show_author: bool,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut impl Write,
) -> io::Result<()> {
    let times: Vec<String> = messages.iter().map(|m| format_ts(&m.ts, now)).collect();
    let width = times.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(4 + width + 2);
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    for (message, time) in messages.iter().zip(times) {
        let time = format!("{time:>width$}");
        let time = if me.is_some_and(|me| message.mentions(me)) {
            style.mention(&time)
        } else {
            style.dim(&time)
        };
        let mut head = String::new();
        if show_author {
            let author = match (&message.user, &message.bot) {
                (Some(user), _) => Some(names.user_label(user)),
                (None, Some(bot)) => Some(format!("{bot} (bot)")),
                (None, None) => None,
            };
            if let Some(author) = author {
                head.push_str(&style.name(&author));
                head.push_str(": ");
            }
        }
        let mut text = mrkdwn::render(&message.text, &user, &channel, style);
        let mut notes = Vec::new();
        if message.is_reply() {
            notes.push("in thread".to_string());
        }
        match message.reply_count {
            0 => {}
            1 => notes.push("1 reply".to_string()),
            n => notes.push(format!("{n} replies")),
        }
        for file in &message.files {
            notes.push(format!("file: {file}"));
        }
        for attachment in &message.attachments {
            notes.push(format!("attachment: {attachment}"));
        }
        if !notes.is_empty() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&style.dim(&format!("[{}]", notes.join(", "))));
        }
        let text = text.replace('\n', &format!("\n{indent}"));
        writeln!(out, "    {time}  {head}{text}")?;
    }
    if has_more {
        writeln!(
            out,
            "{indent}{}",
            style.dim("earlier unread messages not shown")
        )?;
    }
    Ok(())
}

/// The first non-empty line, cut to `max` characters.
fn excerpt(text: &str, max: usize) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(no text)");
    let mut chars = first.chars();
    let cut: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// Local time, with the day when it is not today.
fn format_ts(ts: &str, now: &Zoned) -> String {
    let Some(seconds) = ts.split('.').next().and_then(|s| s.parse::<i64>().ok()) else {
        return ts.to_string();
    };
    let Ok(stamp) = Timestamp::from_second(seconds) else {
        return ts.to_string();
    };
    let zoned = stamp.to_zoned(now.time_zone().clone());
    let format = if zoned.date() == now.date() {
        "%H:%M"
    } else if now.timestamp().as_second() - seconds < 6 * 86_400 {
        "%a %H:%M"
    } else {
        "%d %b %H:%M"
    };
    zoned.strftime(format).to_string()
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;
    use serde_json::Value;

    use super::*;

    fn fixture() -> Counts {
        let body: Value =
            serde_json::from_str(include_str!("../tests/fixtures/client_counts.json")).unwrap();
        Counts::parse(&body).unwrap()
    }

    #[test]
    fn sections_split_dms_mentions_and_channels() {
        let counts = fixture();
        let unread: Vec<&Conversation> = counts.unread().collect();
        let (dms, mentions, channels) = sections(&unread);
        let ids = |list: &[&Conversation]| list.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&dms), ["D06JANE"]);
        assert_eq!(ids(&mentions), ["C02ALERTS"]);
        assert_eq!(ids(&channels), ["C03RANDOM"]);
    }

    #[test]
    fn mentioned_users_are_collected_from_both_forms() {
        assert_eq!(
            mentioned_users("hi <@U1> and <@U2|bob>, not <#C1> or <@>"),
            ["U1", "U2"]
        );
    }

    #[test]
    fn excerpts_take_the_first_line_and_cut_long_ones() {
        assert_eq!(excerpt("\n  hello there\nmore", 60), "hello there");
        assert_eq!(excerpt("abcdef", 3), "abc…");
        assert_eq!(excerpt("", 10), "(no text)");
    }

    fn message(ts: &str, user: Option<&str>, text: &str) -> Message {
        Message {
            ts: ts.to_string(),
            user: user.map(str::to_string),
            bot: None,
            text: text.to_string(),
            subtype: None,
            thread_ts: None,
            reply_count: 0,
            files: Vec::new(),
            attachments: Vec::new(),
        }
    }

    fn now() -> Zoned {
        Timestamp::from_second(1760900000)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn rendered(view: &View) -> String {
        let mut out = Vec::new();
        render_at(
            view,
            &Names::default(),
            &Style::plain(),
            Some("U7"),
            &now(),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_text_rendering_groups_and_marks_what_was_shown() {
        let view = View {
            direct_messages: vec![Entry {
                id: "D1".into(),
                kind: Kind::Im,
                name: "@jane".into(),
                mention_count: 0,
                latest: Some("1760899000.000200".into()),
                messages: Some(vec![
                    message(
                        "1760898000.000100",
                        Some("U1"),
                        "can you look at the deploy?",
                    ),
                    message("1760899000.000200", Some("U1"), "no rush"),
                ]),
                has_more: false,
            }],
            mentions: vec![Entry {
                id: "C1".into(),
                kind: Kind::Channel,
                name: "#alerts".into(),
                mention_count: 1,
                latest: Some("1760899500.000300".into()),
                messages: Some(vec![message(
                    "1760899500.000300",
                    Some("U2"),
                    "<@U7> late again",
                )]),
                has_more: true,
            }],
            channels: vec![Entry {
                id: "C2".into(),
                kind: Kind::Channel,
                name: "#random".into(),
                mention_count: 0,
                latest: Some("1760899600.000400".into()),
                messages: None,
                has_more: false,
            }],
            threads: None,
            all: false,
        };
        let text = rendered(&view);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Direct messages");
        assert_eq!(lines[1], "  @jane  (2 unread)");
        assert!(
            lines[2].ends_with("  can you look at the deploy?"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "Mentions");
        assert_eq!(lines[6], "  #alerts  (1+ unread, 1 mention)");
        assert!(lines[7].ends_with("  U2: @U7 late again"), "{}", lines[7]);
        assert!(lines[8].ends_with("earlier unread messages not shown"));
        assert_eq!(lines.len(), 9);
        let shown: Vec<(&str, &str, bool)> = view.shown().collect();
        assert_eq!(
            shown,
            [
                ("D1", "1760899000.000200", true),
                ("C1", "1760899500.000300", true)
            ]
        );
    }

    #[test]
    fn threads_are_listed_or_summarised() {
        let thread = Thread {
            channel: "C9".into(),
            thread_ts: "1760890000.000100".into(),
            root: message(
                "1760890000.000100",
                Some("U1"),
                "monthly update draft\nsecond line",
            ),
            unread_replies: vec![message(
                "1760899700.000500",
                Some("U3"),
                "added the Q4 numbers",
            )],
            has_more: false,
        };
        let mut view = View {
            direct_messages: Vec::new(),
            mentions: Vec::new(),
            channels: Vec::new(),
            threads: Some(ThreadsView {
                has_unreads: true,
                mention_count: 0,
                items: Some(vec![thread]),
                has_more: true,
            }),
            all: false,
        };
        let text = rendered(&view);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Threads");
        assert_eq!(lines[1], "  C9 › monthly update draft  (1 new reply)");
        assert!(
            lines[2].ends_with("  U3: added the Q4 numbers"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[3], "  more threads not shown");

        view.threads.as_mut().unwrap().items = None;
        let text = rendered(&view);
        assert!(text.contains("(the list could not be read)"), "{text}");
    }

    #[test]
    fn nothing_to_show_says_so() {
        let view = View {
            direct_messages: Vec::new(),
            mentions: Vec::new(),
            channels: Vec::new(),
            threads: Some(ThreadsView {
                has_unreads: false,
                mention_count: 0,
                items: Some(Vec::new()),
                has_more: false,
            }),
            all: false,
        };
        assert_eq!(rendered(&view), "All caught up.\n");
    }
}
