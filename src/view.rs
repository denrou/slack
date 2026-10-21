//! The default view: what is unread, grouped the way a person answers it.
//! The model is built first and rendered second, so `--json` and the TUI
//! read the same thing the text rendering does.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use jiff::{Timestamp, Zoned};
use serde::Serialize;

use crate::api::{self, Client};
use crate::counts::{Conversation, Counts, Kind};
use crate::history::{self, Message, Reaction, compare_ts};
use crate::lastview::ThreadRef;
use crate::names::Names;
use crate::style::Style;
use crate::threads::{self, Thread, ThreadList};
use crate::{emoji, mrkdwn, par, replies};

/// How much of each unread conversation the view reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The unread messages only: what the text view prints.
    Unread,
    /// The recent history, with the read cursor to tell the new part:
    /// what the screen shows, so a message is read with what came before.
    Recent,
}

/// A history on the screen holds today's messages and the unread ones,
/// never fewer than the last `RECENT_MIN` (a quiet channel keeps its
/// context) nor more than one page of `RECENT_MAX` (a busy one is read
/// again on every reload).
pub const RECENT_MIN: usize = 50;
pub const RECENT_MAX: usize = 200;

/// A conversation's recent history, as the screen shows it.
pub fn recent(
    client: &Client,
    channel: &str,
    last_read: Option<&str>,
) -> Result<history::Unread, api::Error> {
    let page = history::fetch(client, channel, None, RECENT_MAX)?;
    Ok(keep_recent(page, last_read, &Zoned::now()))
}

/// Today's messages and the unread ones, or the last `RECENT_MIN` when
/// they are fewer: a backlog from before midnight is never trimmed, since
/// `e` marks it read.
fn keep_recent(mut page: history::Unread, last_read: Option<&str>, now: &Zoned) -> history::Unread {
    let today = match now.start_of_day() {
        Ok(midnight) => {
            let midnight = format!("{}.000000", midnight.timestamp().as_second());
            page.messages
                .iter()
                .filter(|message| compare_ts(&message.ts, &midnight).is_ge())
                .count()
        }
        Err(_) => 0,
    };
    let unread = page
        .messages
        .iter()
        .filter(|message| is_new(message, last_read.filter(|c| history::is_usable_cursor(c))))
        .count();
    let keep = today.max(unread).max(RECENT_MIN);
    if page.messages.len() > keep {
        page.messages.drain(..page.messages.len() - keep);
        page.has_more = true;
    }
    page
}

#[derive(Serialize)]
pub struct View {
    /// DMs and group DMs with unreads: someone wrote to you.
    pub direct_messages: Vec<Entry>,
    /// Channels where you were named.
    pub mentions: Vec<Entry>,
    /// The other channels with unreads, mentions aside.
    pub channels: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<ThreadsView>,
    /// Unread conversations left out because they are muted in Slack.
    #[serde(skip)]
    pub muted: usize,
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
    /// None when the read failed (a warning says which).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
    /// The read cursor, when `messages` is a history rather than the
    /// unread part alone: what follows it is new.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_from: Option<String>,
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
    /// message each had: what `slack read` may mark as read.
    pub fn shown(&self) -> impl Iterator<Item = (&str, &str)> {
        self.direct_messages
            .iter()
            .chain(&self.mentions)
            .chain(&self.channels)
            .filter(|entry| entry.messages.is_some())
            .filter_map(|entry| Some((entry.id.as_str(), entry.latest.as_deref()?)))
    }

    /// Every thread the view shows, in reading order: the messages with
    /// replies, then the threads of the Threads section. The 1-based
    /// position is the number printed next to it, for `slack thread N`.
    pub fn thread_refs(&self) -> Vec<ThreadRef> {
        let mut refs: Vec<ThreadRef> = Vec::new();
        let entries = |entries: &[Entry], refs: &mut Vec<ThreadRef>| {
            for entry in entries {
                if let Some(messages) = &entry.messages {
                    add_threads(refs, &entry.id, messages);
                }
            }
        };
        entries(&self.direct_messages, &mut refs);
        entries(&self.mentions, &mut refs);
        entries(&self.channels, &mut refs);
        for thread in self.threads.iter().flat_map(|t| t.items.iter().flatten()) {
            add_thread(&mut refs, &thread.channel, &thread.thread_ts);
        }
        refs
    }
}

/// The threads the messages with replies belong to, in order, once each.
fn add_threads(refs: &mut Vec<ThreadRef>, channel: &str, messages: &[Message]) {
    for message in messages.iter().filter(|m| m.reply_count > 0) {
        add_thread(
            refs,
            channel,
            message.thread_ts.as_deref().unwrap_or(&message.ts),
        );
    }
}

fn add_thread(refs: &mut Vec<ThreadRef>, channel: &str, ts: &str) {
    let thread = ThreadRef {
        channel: channel.to_string(),
        ts: ts.to_string(),
    };
    if !refs.contains(&thread) {
        refs.push(thread);
    }
}

/// The thread numbers of a view, for the rendering.
pub(crate) struct Refs(Vec<ThreadRef>);

impl Refs {
    pub(crate) fn of(view: &View) -> Refs {
        Refs(view.thread_refs())
    }

    /// The threads among one conversation's messages, for `slack show`.
    pub(crate) fn of_messages(channel: &str, messages: &[Message]) -> Refs {
        let mut refs = Vec::new();
        add_threads(&mut refs, channel, messages);
        Refs(refs)
    }

    pub(crate) fn none() -> Refs {
        Refs(Vec::new())
    }

    pub(crate) fn into_threads(self) -> Vec<ThreadRef> {
        self.0
    }

    fn number(&self, channel: &str, ts: &str) -> Option<usize> {
        self.0
            .iter()
            .position(|thread| thread.channel == channel && thread.ts == ts)
            .map(|index| index + 1)
    }
}

pub fn build(
    client: &Client,
    counts: &Counts,
    names: &mut Names,
    reach: Reach,
) -> Result<View, api::Error> {
    let unread: Vec<&Conversation> = counts.unread().collect();
    names.resolve_channels(client, unread.iter().map(|c| c.id.clone()))?;

    let sections = sections(&unread);
    let mut to_read: Vec<&Conversation> = Vec::new();
    to_read.extend(sections.direct_messages.iter().copied());
    to_read.extend(sections.mentions.iter().copied());
    to_read.extend(sections.channels.iter().copied());

    let cursors: Vec<(String, Option<String>)> = to_read
        .iter()
        .map(|c| (c.id.clone(), c.last_read.clone()))
        .collect();
    let pages = par::map(cursors, |(id, last_read)| {
        let page = match reach {
            Reach::Unread => {
                history::fetch(client, &id, last_read.as_deref(), history::DEFAULT_LIMIT)
            }
            Reach::Recent => recent(client, &id, last_read.as_deref()),
        };
        (id, page)
    });
    let mut pages_by_id = BTreeMap::new();
    for (id, page) in pages {
        match page {
            Ok(page) => {
                pages_by_id.insert(id, page);
            }
            Err(err @ api::Error::Auth { .. }) => return Err(err),
            Err(err) => crate::warn::warn(format!(
                "could not read {}: {err}",
                names.channel_label(&id)
            )),
        }
    }

    let mut threads = match counts.threads {
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
    let thread_items: &mut [Thread] = threads
        .as_mut()
        .and_then(|threads| threads.items.as_deref_mut())
        .unwrap_or(&mut []);
    names.resolve_channels(client, thread_items.iter().map(|t| t.channel.clone()))?;
    if reach == Reach::Recent {
        read_whole(client, names, thread_items)?;
    }
    let thread_items: &[Thread] = thread_items;
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
        let whole = thread.history.iter().flat_map(|history| &history.replies);
        for reply in thread.unread_replies.iter().chain(whole) {
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
                    unread_from: match reach {
                        Reach::Recent => c
                            .last_read
                            .clone()
                            .filter(|cursor| history::is_usable_cursor(cursor)),
                        Reach::Unread => None,
                    },
                }
            })
            .collect()
    };
    Ok(View {
        direct_messages: entries(sections.direct_messages),
        mentions: entries(sections.mentions),
        channels: entries(sections.channels),
        threads,
        muted: counts.muted_unread_count(),
    })
}

/// Each thread whole, as a channel's recent history is read: a reply is
/// understood with the ones before it. One that cannot be read keeps its
/// new replies alone.
fn read_whole(client: &Client, names: &Names, threads: &mut [Thread]) -> Result<(), api::Error> {
    let wanted = threads
        .iter()
        .map(|thread| (thread.channel.clone(), thread.thread_ts.clone()))
        .collect();
    let fetched = par::map(wanted, |(channel, ts)| {
        replies::fetch(client, &channel, &ts)
    });
    for (thread, fetched) in threads.iter_mut().zip(fetched) {
        match fetched {
            Ok(history) => thread.history = Some(history),
            Err(err @ api::Error::Auth { .. }) => return Err(err),
            Err(err) => crate::warn::warn(format!(
                "could not read a thread in {}: {err}",
                names.channel_label(&thread.channel)
            )),
        }
    }
    Ok(())
}

struct Sections<'a> {
    direct_messages: Vec<&'a Conversation>,
    mentions: Vec<&'a Conversation>,
    channels: Vec<&'a Conversation>,
}

/// Split the unread conversations into the sections, most recent
/// activity first (mentions: most mentions first).
fn sections<'a>(unread: &[&'a Conversation]) -> Sections<'a> {
    let mut sections = Sections {
        direct_messages: Vec::new(),
        mentions: Vec::new(),
        channels: Vec::new(),
    };
    for conversation in unread {
        match conversation.kind {
            Kind::Im | Kind::Mpim => sections.direct_messages.push(*conversation),
            Kind::Channel if conversation.mention_count > 0 => {
                sections.mentions.push(*conversation)
            }
            Kind::Channel => sections.channels.push(*conversation),
        }
    }
    let newest_first = |a: &&Conversation, b: &&Conversation| {
        compare_ts(
            b.latest.as_deref().unwrap_or("0"),
            a.latest.as_deref().unwrap_or("0"),
        )
    };
    sections.direct_messages.sort_by(newest_first);
    sections.mentions.sort_by(|a, b| {
        b.mention_count
            .cmp(&a.mention_count)
            .then_with(|| newest_first(a, b))
    });
    sections.channels.sort_by(newest_first);
    sections
}

fn collect_people(people: &mut BTreeSet<String>, message: &Message) {
    people.extend(message.user.clone());
    people.extend(mentioned_users(&message.text));
}

/// The user ids named in a message, so their names can be looked up.
pub(crate) fn mentioned_users(text: &str) -> Vec<String> {
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
    out: &mut dyn Write,
) -> io::Result<()> {
    render_at(view, names, style, me, &Zoned::now(), out)
}

fn render_at(
    view: &View,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut dyn Write,
) -> io::Result<()> {
    let refs = Refs::of(view);
    let printed = Cell::new(false);
    let section = |title: &str, entries: &[Entry], out: &mut dyn Write| -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if printed.get() {
            writeln!(out)?;
        }
        printed.set(true);
        writeln!(out, "{}", style.heading(title))?;
        for entry in entries {
            render_entry(entry, &refs, names, style, me, now, out)?;
        }
        Ok(())
    };
    section("Direct messages", &view.direct_messages, out)?;
    section("Mentions", &view.mentions, out)?;
    section("Channels", &view.channels, out)?;

    if let Some(threads) = view.threads.as_ref().filter(|t| t.is_unread()) {
        if printed.get() {
            writeln!(out)?;
        }
        printed.set(true);
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
                    render_thread(thread, &refs, names, style, me, now, out)?;
                }
                if threads.has_more {
                    writeln!(out, "  {}", style.dim("more threads not shown"))?;
                }
            }
        }
    }

    if !printed.get() {
        writeln!(out, "All caught up.")?;
    }
    if view.muted > 0 {
        let plural = if view.muted == 1 { "" } else { "s" };
        eprintln!(
            "{}",
            style.dim(&format!(
                "{} muted conversation{plural} left out, as in the app",
                view.muted
            ))
        );
    }
    Ok(())
}

fn render_entry(
    entry: &Entry,
    refs: &Refs,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut dyn Write,
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
        Some(messages) => {
            let list = MessageList {
                messages,
                channel: &entry.id,
                has_more: entry.has_more,
                show_author: entry.kind != Kind::Im,
                scope: Scope::Unread,
                unread_from: None,
            };
            render_messages(&list, refs, names, style, me, now, out)
        }
        None => Ok(()),
    }
}

fn render_thread(
    thread: &Thread,
    refs: &Refs,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut dyn Write,
) -> io::Result<()> {
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    let root = mrkdwn::render(&thread.root.text, &user, &channel, &Style::plain());
    let count = match thread.unread_replies.len() {
        1 => "1 new reply".to_string(),
        n => format!("{n}{} new replies", if thread.has_more { "+" } else { "" }),
    };
    let number = refs
        .number(&thread.channel, &thread.thread_ts)
        .map(|n| format!(", thread {n}"))
        .unwrap_or_default();
    writeln!(
        out,
        "  {} › {}  {}",
        style.bold(&names.channel_label(&thread.channel)),
        excerpt(&root, 60),
        style.dim(&format!("({count}{number})"))
    )?;
    let list = MessageList {
        messages: &thread.unread_replies,
        channel: &thread.channel,
        has_more: false,
        show_author: true,
        scope: Scope::Thread,
        unread_from: None,
    };
    render_messages(&list, refs, names, style, me, now, out)
}

/// What a block of messages is, for the notes around it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The unread part of a conversation.
    Unread,
    /// A thread: every message is a reply, no need to say so each time.
    Thread,
    /// The recent history of a conversation, read or not.
    History,
}

/// Messages to render as a block, with what the notes need to know.
pub(crate) struct MessageList<'a> {
    pub messages: &'a [Message],
    pub channel: &'a str,
    /// More messages exist beyond this page: older for a history, newer
    /// for a thread.
    pub has_more: bool,
    pub show_author: bool,
    pub scope: Scope,
    /// The read cursor: a separator goes before the first message after it.
    pub unread_from: Option<&'a str>,
}

pub(crate) fn render_messages(
    list: &MessageList,
    refs: &Refs,
    names: &Names,
    style: &Style,
    me: Option<&str>,
    now: &Zoned,
    out: &mut dyn Write,
) -> io::Result<()> {
    let times: Vec<String> = list
        .messages
        .iter()
        .map(|m| format_ts(&m.ts, now))
        .collect();
    let width = times.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(4 + width + 2);
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    // A history reads down from its oldest message, so what is missing
    // before it is said first; the other blocks keep the note as a footnote.
    if list.has_more && list.scope == Scope::History {
        writeln!(
            out,
            "{indent}{}",
            style.dim("earlier messages not shown; -n asks for more")
        )?;
    }
    let mut separated = false;
    for (message, time) in list.messages.iter().zip(times) {
        if !separated && is_new(message, list.unread_from) {
            separated = true;
            writeln!(out, "{indent}{}", style.dim("── new ──"))?;
        }
        let time = format!("{time:>width$}");
        let time = if me.is_some_and(|me| message.mentions(me)) {
            style.mention(&time)
        } else {
            style.dim(&time)
        };
        let mut head = String::new();
        if list.show_author {
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
        if list.scope != Scope::Thread && message.is_reply() {
            notes.push("in thread".to_string());
        }
        if message.reply_count > 0 {
            let replies = match message.reply_count {
                1 => "1 reply".to_string(),
                n => format!("{n} replies"),
            };
            let thread_ts = message.thread_ts.as_deref().unwrap_or(&message.ts);
            notes.push(match refs.number(list.channel, thread_ts) {
                Some(number) => format!("thread {number}: {replies}"),
                None => replies,
            });
        }
        for file in &message.files {
            notes.push(format!("file: {file}"));
        }
        for attachment in &message.attachments {
            notes.push(format!("attachment: {attachment}"));
        }
        if !message.reactions.is_empty() {
            let labels: Vec<String> = message.reactions.iter().map(reaction_label).collect();
            notes.push(labels.join("  "));
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
    if list.has_more && list.scope != Scope::History {
        let note = match list.scope {
            Scope::Unread => "earlier unread messages not shown",
            Scope::Thread | Scope::History => "more replies not shown",
        };
        writeln!(out, "{indent}{}", style.dim(note))?;
    }
    Ok(())
}

/// An emoji and how many put it there: `👍 3`, `:yolo: 1`.
pub(crate) fn reaction_label(reaction: &Reaction) -> String {
    format!("{} {}", emoji::label(&reaction.name), reaction.count)
}

/// Newer than the read cursor, when there is one.
pub(crate) fn is_new(message: &Message, unread_from: Option<&str>) -> bool {
    unread_from.is_some_and(|cursor| compare_ts(&message.ts, cursor).is_gt())
}

/// The first non-empty line, cut to `max` characters.
pub(crate) fn excerpt(text: &str, max: usize) -> String {
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
pub(crate) fn format_ts(ts: &str, now: &Zoned) -> String {
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
        let ids = |list: &[&Conversation]| list.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        let unread: Vec<&Conversation> = counts.unread().collect();
        let sections = sections(&unread);
        assert_eq!(ids(&sections.direct_messages), ["D06JANE"]);
        assert_eq!(ids(&sections.mentions), ["C02ALERTS"]);
        assert_eq!(ids(&sections.channels), ["C03RANDOM"]);
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
            reactions: Vec::new(),
        }
    }

    fn entry(
        id: &str,
        kind: Kind,
        name: &str,
        mention_count: u64,
        latest: &str,
        messages: Option<Vec<Message>>,
        has_more: bool,
    ) -> Entry {
        Entry {
            id: id.into(),
            kind,
            name: name.into(),
            mention_count,
            latest: Some(latest.into()),
            messages,
            has_more,
            unread_from: None,
        }
    }

    fn empty_view() -> View {
        View {
            direct_messages: Vec::new(),
            mentions: Vec::new(),
            channels: Vec::new(),
            threads: None,
            muted: 0,
        }
    }

    fn now() -> Zoned {
        Timestamp::from_second(1760900000)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    #[test]
    fn the_screen_keeps_today_the_unread_and_at_least_the_minimum() {
        // Midnight of `now()` is 1760832000.
        let page = |older: usize, today: usize| history::Unread {
            messages: (0..older)
                .map(|n| 1760800000 + n)
                .chain((0..today).map(|n| 1760832000 + n))
                .map(|second| message(&format!("{second}.000100"), None, ""))
                .collect(),
            has_more: false,
        };
        let kept = keep_recent(page(10, 3), None, &now());
        assert_eq!(
            (kept.messages.len(), kept.has_more),
            (13, false),
            "a quiet channel"
        );
        let kept = keep_recent(page(100, 3), None, &now());
        assert_eq!((kept.messages.len(), kept.has_more), (RECENT_MIN, true));
        assert_eq!(
            kept.messages[0].ts, "1760800053.000100",
            "the newest of the older ones"
        );
        let kept = keep_recent(page(100, 80), None, &now());
        assert_eq!(
            (kept.messages.len(), kept.has_more),
            (80, true),
            "a busy day"
        );
        assert!(kept.messages[0].ts.starts_with("1760832000."));
        // Read up to the 21st older message: 79 older ones and 3 of today
        // are unread, and none of them is trimmed.
        let kept = keep_recent(page(100, 3), Some("1760800020.000100"), &now());
        assert_eq!(kept.messages.len(), 82, "a backlog from before midnight");
        let never = Some("0000000000.000000");
        let kept = keep_recent(page(100, 3), never, &now());
        assert_eq!(
            kept.messages.len(),
            RECENT_MIN,
            "never read: no cursor to keep from"
        );
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
    fn the_text_rendering_groups_numbers_and_marks_what_was_shown() {
        let mut with_replies = message("1760899000.000200", Some("U1"), "no rush");
        with_replies.reply_count = 2;
        with_replies.reactions = vec![Reaction {
            name: "eyes".into(),
            count: 1,
            users: vec!["U7".into()],
        }];
        let view = View {
            direct_messages: vec![entry(
                "D1",
                Kind::Im,
                "@jane",
                0,
                "1760899000.000200",
                Some(vec![
                    message(
                        "1760898000.000100",
                        Some("U1"),
                        "can you look at the deploy?",
                    ),
                    with_replies,
                ]),
                false,
            )],
            mentions: vec![entry(
                "C1",
                Kind::Channel,
                "#alerts",
                1,
                "1760899500.000300",
                Some(vec![message(
                    "1760899500.000300",
                    Some("U2"),
                    "<@U7> late again",
                )]),
                true,
            )],
            channels: vec![
                entry(
                    "C3",
                    Kind::Channel,
                    "#team",
                    0,
                    "1760899550.000350",
                    Some(vec![message(
                        "1760899550.000350",
                        Some("U3"),
                        "standup moved to 10",
                    )]),
                    false,
                ),
                // Its messages could not be read (reported on stderr).
                entry(
                    "C2",
                    Kind::Channel,
                    "#random",
                    0,
                    "1760899600.000400",
                    None,
                    false,
                ),
            ],
            ..empty_view()
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
        assert!(
            lines[3].ends_with("  no rush [thread 1: 2 replies, 👀 1]"),
            "{}",
            lines[3]
        );
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "Mentions");
        assert_eq!(lines[6], "  #alerts  (1+ unread, 1 mention)");
        assert!(lines[7].ends_with("  U2: @U7 late again"), "{}", lines[7]);
        assert!(lines[8].ends_with("earlier unread messages not shown"));
        assert_eq!(lines[10], "Channels");
        assert_eq!(lines[11], "  #team  (1 unread)");
        assert!(
            lines[12].ends_with("  U3: standup moved to 10"),
            "{}",
            lines[12]
        );
        assert_eq!(lines[13], "  #random  (unread)");
        assert_eq!(lines.len(), 14);

        let shown: Vec<(&str, &str)> = view.shown().collect();
        assert_eq!(
            shown,
            [
                ("D1", "1760899000.000200"),
                ("C1", "1760899500.000300"),
                ("C3", "1760899550.000350"),
            ],
            "a channel whose messages were not shown is not marked"
        );
        assert_eq!(
            view.thread_refs(),
            [ThreadRef {
                channel: "D1".into(),
                ts: "1760899000.000200".into()
            }]
        );
    }

    #[test]
    fn threads_are_listed_numbered_or_summarised() {
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
            history: None,
        };
        let mut view = View {
            threads: Some(ThreadsView {
                has_unreads: true,
                mention_count: 0,
                items: Some(vec![thread]),
                has_more: true,
            }),
            ..empty_view()
        };
        let text = rendered(&view);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Threads");
        assert_eq!(
            lines[1],
            "  C9 › monthly update draft  (1 new reply, thread 1)"
        );
        assert!(
            lines[2].ends_with("  U3: added the Q4 numbers"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[3], "  more threads not shown");
        assert_eq!(view.thread_refs().len(), 1);

        view.threads.as_mut().unwrap().items = None;
        let text = rendered(&view);
        assert!(text.contains("(the list could not be read)"), "{text}");
    }

    #[test]
    fn nothing_to_show_says_so() {
        let view = View {
            threads: Some(ThreadsView {
                has_unreads: false,
                mention_count: 0,
                items: Some(Vec::new()),
                has_more: false,
            }),
            ..empty_view()
        };
        assert_eq!(rendered(&view), "All caught up.\n");
    }
}
