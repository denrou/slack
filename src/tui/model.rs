//! The TUI's state and its pure transitions: what the list holds, what is
//! selected, which pane has the focus, which mode the keys go to. No I/O,
//! so the keys are testable; `mod.rs` performs the actions.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::counts::{Conversation, Kind};
use crate::history::{self, Message, compare_ts};
use crate::mrkdwn::{self, Link};
use crate::names::Names;
use crate::rtm::{Event, Posted};
use crate::search::{Found, Hit};
use crate::style::Style;
use crate::view::{self, Entry, View};
use crate::{directory, emoji, threads};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    DirectMessages,
    Mentions,
    /// The other channels with unreads, mentions aside.
    Channels,
    Threads,
    /// Conversations opened from the picker (`o`), with their recent
    /// history. They stay through a refresh and a reply.
    Opened,
    /// The channels visited last in Slack, read ones only: the unread
    /// ones are listed above.
    RecentChannels,
    /// The direct and group messages visited last in Slack.
    RecentPeople,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::DirectMessages => "Direct messages",
            Section::Mentions => "Mentions",
            Section::Channels => "Channels",
            Section::Threads => "Threads",
            Section::Opened => "Opened",
            Section::RecentChannels => "Recent channels",
            Section::RecentPeople => "Recent people",
        }
    }

    pub fn is_recent(self) -> bool {
        matches!(self, Section::RecentChannels | Section::RecentPeople)
    }

    /// A conversation listed for itself rather than for something new:
    /// read from its newest message, and kept in the list once replied to.
    fn is_browsed(self) -> bool {
        self == Section::Opened || self.is_recent()
    }
}

/// One line of the list: a conversation or a thread, with its messages.
pub struct Item {
    pub section: Section,
    pub channel: String,
    /// Set for a thread: replies go there, and `O` opens it.
    pub thread_ts: Option<String>,
    pub title: String,
    pub detail: String,
    pub messages: Vec<Message>,
    pub has_more: bool,
    /// Where to move the read cursor.
    pub latest: Option<String>,
    /// The read cursor of a history: the pane marks where "new" starts.
    pub unread_from: Option<String>,
    /// False for a recent conversation until its history is read, which
    /// happens once the cursor rests on it, and again when a message came
    /// in since (the old history is shown meanwhile).
    pub loaded: bool,
    /// Muted in Slack: drawn greyed out.
    pub muted: bool,
}

impl Item {
    pub fn from_view(view: View, names: &Names) -> Vec<Item> {
        let mut items = Vec::new();
        let conversations = |section: Section, entries: Vec<Entry>, items: &mut Vec<Item>| {
            items.extend(
                entries
                    .into_iter()
                    .filter(|entry| entry.messages.is_some())
                    .map(|entry| Item::conversation(section, entry)),
            );
        };
        conversations(Section::DirectMessages, view.direct_messages, &mut items);
        conversations(Section::Mentions, view.mentions, &mut items);
        conversations(Section::Channels, view.channels, &mut items);
        if let Some(threads) = view.threads {
            for thread in threads.items.unwrap_or_default() {
                items.push(Item::thread(thread, names));
            }
        }
        items
    }

    fn conversation(section: Section, entry: Entry) -> Item {
        let messages = entry.messages.unwrap_or_default();
        let unread_from = entry.unread_from;
        // A history holds read messages too; without a cursor the page is
        // the unread part itself, or a conversation never read at all.
        let unread = unread_count(&messages, unread_from.as_deref());
        let more = entry.has_more && unread == messages.len();
        let mut detail = format!("{unread}{} unread", if more { "+" } else { "" });
        match entry.mention_count {
            0 => {}
            1 => detail.push_str(", 1 mention"),
            n => detail.push_str(&format!(", {n} mentions")),
        }
        Item {
            section,
            channel: entry.id,
            thread_ts: None,
            title: entry.name,
            detail,
            messages,
            has_more: entry.has_more,
            latest: entry.latest,
            unread_from,
            loaded: true,
            muted: false,
        }
    }

    fn thread(thread: threads::Thread, names: &Names) -> Item {
        let user = |id: &str| names.user_name(id);
        let channel = |id: &str| names.channel_name(id);
        let root = mrkdwn::render(&thread.root.text, &user, &channel, &Style::plain());
        // The read part ends just before the first reply the thread view
        // calls unread, the view that listed the thread. A history cut by
        // the page cap misses its newest replies, the unread ones: the new
        // replies alone then.
        let whole = thread.history.filter(|history| !history.has_more);
        let (messages, has_more, unread_from) = match whole {
            Some(history) => {
                let messages = history.into_messages();
                let cursor = thread.unread_replies.first().and_then(|first| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| compare_ts(&message.ts, &first.ts).is_lt())
                        .map(|message| message.ts.clone())
                });
                (messages, false, cursor)
            }
            None => (thread.unread_replies, thread.has_more, None),
        };
        // `e` marks what the pane shows.
        let latest = messages.last().map(|message| message.ts.clone());
        let detail = match unread_count(&messages, unread_from.as_deref()) {
            1 if !has_more => "1 new reply".to_string(),
            n => format!("{n}{} new replies", if has_more { "+" } else { "" }),
        };
        Item {
            section: Section::Threads,
            title: format!(
                "{} › {}",
                names.channel_label(&thread.channel),
                view::excerpt(&root, 40)
            ),
            detail,
            latest,
            channel: thread.channel,
            thread_ts: Some(thread.thread_ts),
            messages,
            has_more,
            unread_from,
            loaded: true,
            muted: false,
        }
    }

    /// A conversation opened from the picker, with its recent history.
    pub fn opened(
        channel: String,
        title: String,
        page: history::Unread,
        unread_from: Option<String>,
    ) -> Item {
        let mut item = Item {
            section: Section::Opened,
            channel,
            thread_ts: None,
            title,
            detail: String::new(),
            latest: page.messages.last().map(|message| message.ts.clone()),
            messages: Vec::new(),
            has_more: false,
            unread_from,
            loaded: false,
            muted: false,
        };
        item.fill(page);
        item
    }

    /// A conversation opened at a search hit, with the messages around it.
    pub fn around(channel: String, title: String, page: history::Unread) -> Item {
        let mut item = Item::opened(channel, title, page, None);
        item.detail = format!("{} messages around the match", item.messages.len());
        item
    }

    /// A conversation visited last in Slack, its history not read yet.
    /// Its `latest` stays the one the counts gave: a newer one there
    /// means the history shown is out of date.
    pub fn recent(conversation: &Conversation, title: String) -> Item {
        let section = match conversation.kind {
            Kind::Channel => Section::RecentChannels,
            Kind::Im | Kind::Mpim => Section::RecentPeople,
        };
        let mut item = Item {
            section,
            channel: conversation.id.clone(),
            thread_ts: None,
            title,
            detail: String::new(),
            messages: Vec::new(),
            has_more: false,
            latest: conversation.latest.clone(),
            // A muted one can be unread without being listed above.
            unread_from: conversation
                .last_read
                .clone()
                .filter(|_| conversation.is_unread()),
            loaded: false,
            muted: conversation.muted,
        };
        item.describe();
        item
    }

    /// The recent history, read.
    fn fill(&mut self, page: history::Unread) {
        self.messages = page.messages;
        self.has_more = page.has_more;
        self.loaded = true;
        self.describe();
    }

    /// The history this conversation had before a refresh rebuilt it,
    /// kept while it is still current, and shown until it is read again
    /// when it is not.
    fn keep_history(&mut self, old: Item) {
        if old.messages.is_empty() && !old.loaded {
            return;
        }
        self.loaded = old.loaded && old.latest == self.latest;
        self.messages = old.messages;
        self.has_more = old.has_more;
        self.describe();
    }

    /// Where reading starts in the pane: the first new message of a
    /// history, else its newest one; the unread part alone is read from
    /// its start.
    pub fn landing(&self) -> usize {
        let last = self.messages.len().saturating_sub(1);
        match &self.unread_from {
            Some(cursor) => self
                .messages
                .iter()
                .position(|message| view::is_new(message, Some(cursor)))
                .unwrap_or(last),
            None if self.section.is_browsed() => last,
            None => 0,
        }
    }

    fn describe(&mut self) {
        if self.messages.is_empty() && !self.loaded {
            self.detail = if self.unread_from.is_some() {
                "unread".to_string()
            } else {
                String::new()
            };
            return;
        }
        let new = self
            .messages
            .iter()
            .filter(|message| view::is_new(message, self.unread_from.as_deref()))
            .count();
        self.detail = match self.messages.len() {
            0 => "no messages".to_string(),
            n if self.has_more => format!("last {n} messages"),
            1 => "1 message".to_string(),
            n => format!("{n} messages"),
        };
        if new > 0 {
            self.detail.push_str(&format!(", {new} new"));
        }
    }

    fn key(&self) -> (&str, Option<&str>) {
        (&self.channel, self.thread_ts.as_deref())
    }

    fn check_key(&self) -> CheckKey {
        (
            self.channel.clone(),
            self.thread_ts.clone(),
            self.section == Section::Opened,
        )
    }

    fn is(&self, (channel, thread_ts, opened): &CheckKey) -> bool {
        self.key() == (channel.as_str(), thread_ts.as_deref())
            && (self.section == Section::Opened) == *opened
    }
}

/// The messages past the read cursor; without one, the page is the unread
/// part itself.
fn unread_count(messages: &[Message], unread_from: Option<&str>) -> usize {
    match unread_from {
        Some(_) => messages
            .iter()
            .filter(|message| view::is_new(message, unread_from))
            .count(),
        None => messages.len(),
    }
}

/// A line of the list: a section header, or the item at that index.
pub enum Row {
    Header(Section),
    Item(usize),
}

pub fn rows(items: &[Item]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut current = None;
    for (index, item) in items.iter().enumerate() {
        if current != Some(item.section) {
            rows.push(Row::Header(item.section));
            current = Some(item.section);
        }
        rows.push(Row::Item(index));
    }
    rows
}

pub fn row_of(rows: &[Row], item: usize) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, Row::Item(index) if *index == item))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Messages,
}

/// A thread opened from a message, shown in the messages pane in place of
/// the item's messages: the root first, then every reply.
pub struct ThreadPane {
    pub channel: String,
    pub thread_ts: String,
    pub title: String,
    pub messages: Vec<Message>,
    pub has_more: bool,
    /// The message it was opened from, to put the cursor back on it.
    pub from_message: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    Channel,
    Group,
    Dm,
    /// Someone you have no direct message with yet.
    Person,
}

/// Something the picker can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub kind: ChoiceKind,
    /// The conversation id, or the user id of a person.
    pub id: String,
    pub label: String,
    /// What the filter may match besides the label: a handle, a title.
    pub search: String,
    pub note: String,
    /// Muted in Slack: out of the list until `m` shows them.
    pub muted: bool,
    /// The read cursor, when the conversation has unreads.
    pub unread_from: Option<String>,
    /// The person behind a direct message, or the person: what a group
    /// message is made of. None for a channel or a group.
    pub user: Option<String>,
}

/// The picker: every conversation and person, narrowed by what is typed.
#[derive(Debug, PartialEq, Eq)]
pub struct Picker {
    pub choices: Vec<Choice>,
    pub query: String,
    /// Index into `matches()`.
    pub selected: usize,
    /// The keys go to the filter (where the picker opens, and after
    /// `/`) rather than to the list.
    pub filtering: bool,
    /// `m`: the muted conversations are listed too.
    pub show_muted: bool,
    /// People marked with Space, as indexes into `choices`: Enter opens
    /// their group message instead of the highlighted choice.
    pub marked: Vec<usize>,
    /// Write to the chosen conversation straight away (`c`), rather
    /// than only open it (`o`).
    pub compose: bool,
}

impl Picker {
    pub fn new(choices: Vec<Choice>, compose: bool) -> Picker {
        Picker {
            choices,
            query: String::new(),
            selected: 0,
            filtering: true,
            show_muted: false,
            marked: Vec::new(),
            compose,
        }
    }

    /// The choices the query keeps, as indexes into `choices`; the muted
    /// ones only once `m` asked for them.
    pub fn matches(&self) -> Vec<usize> {
        self.choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| self.show_muted || !choice.muted)
            .filter(|(_, choice)| {
                directory::matches(&self.query, &format!("{} {}", choice.label, choice.search))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The muted conversations kept out of the list.
    pub fn hidden_muted(&self) -> usize {
        if self.show_muted {
            0
        } else {
            self.choices.iter().filter(|choice| choice.muted).count()
        }
    }

    fn picked(&self) -> Option<Choice> {
        let matches = self.matches();
        matches
            .get(self.selected)
            .map(|&index| self.choices[index].clone())
    }

    fn step(&mut self, step: isize) {
        let last = self.matches().len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize).saturating_add(step).clamp(0, last) as usize;
    }

    /// Space or Tab: mark the highlighted person for a group message, or
    /// unmark them. A channel or a group cannot be put in one; false then.
    fn mark(&mut self) -> bool {
        let Some(&index) = self.matches().get(self.selected) else {
            return false;
        };
        if self.choices[index].user.is_none() {
            return false;
        }
        match self.marked.iter().position(|&marked| marked == index) {
            Some(at) => {
                self.marked.remove(at);
            }
            None => self.marked.push(index),
        }
        true
    }

    /// Tab in the filter: mark, then clear what was typed for the next
    /// name, since Space is a character of the filter there.
    fn mark_and_clear(&mut self) {
        if self.mark() {
            self.query.clear();
            self.selected = 0;
        }
    }

    /// What was typed, blanks aside: blanks alone match everything.
    fn typed(&self) -> &str {
        self.query.trim()
    }

    /// Enter in the filter hands the keys to the list rather than opening:
    /// with nothing typed nor marked, or when nothing matches, since `m`
    /// shows the muted ones only there.
    pub fn enter_lists(&self) -> bool {
        (self.typed().is_empty() && self.marked.is_empty()) || self.matches().is_empty()
    }

    /// Enter on a name typed after marking others: the highlighted person
    /// joins them, never toggled out. False for a channel or a group, or
    /// when nothing matches.
    fn join(&mut self) -> bool {
        let Some(&index) = self.matches().get(self.selected) else {
            return false;
        };
        self.marked.contains(&index) || self.mark()
    }

    /// What Enter opens: the marked people as one group message, else
    /// the highlighted choice.
    fn chosen(&self) -> Option<Action> {
        if !self.marked.is_empty() {
            let users = self
                .marked
                .iter()
                .filter_map(|&index| self.choices[index].user.clone())
                .collect();
            return Some(Action::ShowGroup {
                users,
                compose: self.compose,
            });
        }
        self.picked().map(|choice| Action::Show {
            choice,
            compose: self.compose,
        })
    }
}

/// A message being written in the reply box, to the selected item.
#[derive(Debug, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    /// The thread it goes in, if any.
    pub thread_ts: Option<String>,
    /// The highlighted entry of the completion list, None once Esc closed
    /// the list; typing opens it again.
    pub pick: Option<usize>,
}

impl Draft {
    pub fn new(thread_ts: Option<String>) -> Draft {
        Draft {
            text: String::new(),
            thread_ts,
            pick: Some(0),
        }
    }

    /// The chosen entry takes the place of the word being completed, a
    /// space after it.
    fn accept(&mut self, start: usize, insert: &str) {
        self.text.truncate(start);
        self.text.push_str(insert);
        self.text.push(' ');
        self.pick = Some(0);
    }
}

/// The search of `/`: the query line, then what Slack found for it. It
/// outlives its mode, so `/` comes back to the last one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Search {
    pub query: String,
    /// The query the hits were found for; None before the first search.
    pub searched: Option<String>,
    pub hits: Vec<Hit>,
    /// Every match Slack counts, beyond the page.
    pub total: u64,
    /// Index into `hits`.
    pub selected: usize,
    /// The keys go to the query line rather than to the hits.
    pub typing: bool,
}

impl Search {
    /// What is typed is what the hits are for: Enter goes to them
    /// rather than asking Slack again.
    pub fn enter_hits(&self) -> bool {
        !self.hits.is_empty() && self.searched.as_deref() == Some(self.query.trim())
    }

    fn step(&mut self, step: isize) {
        let last = self.hits.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize).saturating_add(step).clamp(0, last) as usize;
    }

    fn highlighted(&self) -> Option<&Hit> {
        self.hits.get(self.selected)
    }
}

/// The emoji prompt of `R`, over the highlighted message.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reacting {
    pub query: String,
    /// Index into `State::emoji_choices`.
    pub pick: usize,
}

/// How many emoji the `R` prompt offers at once.
const REACT_ROWS: usize = 8;

/// What the reply box completes: `@handle` for a person, `#name` for a
/// channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What lands in the text.
    pub insert: String,
    /// Shown dimmed next to it: the person's name and title, a channel's
    /// notes.
    pub detail: String,
    /// Lowercase words the typed part may begin: the handle, the names.
    pub search: String,
}

impl Candidate {
    fn begins(&self, word: &str) -> bool {
        word.is_empty() || self.search.split_whitespace().any(|w| w.starts_with(word))
    }
}

/// The completion list under the reply box.
#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
    /// Where the word being completed starts in the text.
    pub start: usize,
    /// The candidates it begins, as indexes into `State::candidates`.
    pub matches: Vec<usize>,
    /// Index into `matches`.
    pub pick: usize,
}

/// Ctrl-J, Ctrl-Enter and Shift-Enter: a line break in the reply box.
/// Ctrl-J is a byte of its own in any terminal; the other two reach the
/// box apart from Enter only where the terminal reports them so (see
/// the keyboard enhancement in `mod.rs`), and are plain Enter elsewhere.
fn breaks_line(key: KeyEvent) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter => control || shift,
        KeyCode::Char('j') => control,
        _ => false,
    }
}

/// What a deleting key takes off the end of the text: Backspace (or the
/// Ctrl-H some terminals send for it) a character; Ctrl-W, or Backspace
/// with Option (Alt) or Ctrl, the word before it; Ctrl-U, or Backspace with Command (Super), the line. The
/// modified Backspaces are a Mac's and reach the box only where the
/// terminal reports them; the Ctrl keys are readline's and work
/// everywhere.
#[derive(Clone, Copy)]
enum Cut {
    Char,
    Word,
    Line,
}

impl Cut {
    fn of(key: KeyEvent) -> Option<Cut> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let word = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        let line = key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Backspace if line => Some(Cut::Line),
            KeyCode::Backspace if word => Some(Cut::Word),
            KeyCode::Backspace => Some(Cut::Char),
            KeyCode::Char('h') if control => Some(Cut::Char),
            KeyCode::Char('w') if control => Some(Cut::Word),
            KeyCode::Char('u') if control => Some(Cut::Line),
            _ => None,
        }
    }

    /// A word is the run before the end and the blanks after it, as on a
    /// Mac. A line break goes alone, for a word or a line as for a
    /// character: the line above is never eaten by accident.
    fn apply(self, text: &mut String) {
        let keep = match self {
            _ if text.ends_with('\n') => text.len() - 1,
            Cut::Char => text.len() - text.chars().next_back().map_or(0, char::len_utf8),
            Cut::Word => {
                let end = text
                    .trim_end_matches(|c: char| c != '\n' && c.is_whitespace())
                    .len();
                word_start(&text[..end])
            }
            Cut::Line => text.rfind('\n').map_or(0, |at| at + 1),
        };
        text.truncate(keep);
    }
}

/// Where the last word of the text starts: after its last blank, or at 0.
fn word_start(text: &str) -> usize {
    text.char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(at, c)| at + c.len_utf8())
}

/// The word being completed: where it starts, its sigil and what follows,
/// when the text ends in a word that starts with `@` or `#`.
fn completing(text: &str) -> Option<(usize, char, &str)> {
    let start = word_start(text);
    let word = &text[start..];
    let sigil = word.chars().next().filter(|c| matches!(c, '@' | '#'))?;
    Some((start, sigil, &word[sigil.len_utf8()..]))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Compose(Draft),
    Help,
    Pick(Picker),
    React(Reacting),
    Search(Search),
    /// Which link of the highlighted message: a digit picks one, to
    /// open it or to copy it.
    Links {
        links: Vec<Link>,
        copy: bool,
    },
}

/// What the selection holds a conversation by: its channel and thread,
/// and whether it is the opened copy, which can sit in the list beside
/// the unread one.
type CheckKey = (String, Option<String>, bool);

/// A conversation `e` marks read, up to the newest message it had when
/// it was chosen: one that came in since was never seen, so it stays
/// unread.
#[derive(Debug, PartialEq, Eq)]
pub struct Mark {
    pub item: usize,
    pub upto: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    MarkRead(Vec<Mark>),
    Send {
        item: usize,
        text: String,
        thread_ts: Option<String>,
    },
    /// Open the conversation in Slack, at the highlighted message when
    /// the pane has the focus.
    Open {
        item: usize,
        message: Option<usize>,
    },
    /// Open the thread of a message of the selected item.
    OpenThread {
        item: usize,
        message: usize,
    },
    /// Load the directory and show the picker; `compose` goes on to
    /// write to what it opens.
    OpenPicker {
        compose: bool,
    },
    /// Load the directory for the reply box's completion, the first time
    /// a `@` or `#` word is typed.
    LoadCandidates,
    /// Read the recent history of a conversation listed without one;
    /// `enter` goes on into the messages pane.
    Preview {
        item: usize,
        enter: bool,
    },
    /// Open a conversation from the picker with its recent history.
    Show {
        choice: Choice,
        compose: bool,
    },
    /// Open the group message of these people (`conversations.open`).
    ShowGroup {
        users: Vec<String>,
        compose: bool,
    },
    /// Hand a URL to the browser.
    OpenLink(String),
    /// Put a URL in the clipboard.
    CopyLink(String),
    /// Put an emoji on a message, or take yours back.
    React {
        channel: String,
        ts: String,
        name: String,
        add: bool,
    },
    /// Ask Slack's search, the search mode staying open for the hits.
    Search {
        query: String,
    },
    /// Open the conversation of a hit at it, in its thread for a reply.
    ShowHit(Hit),
}

/// What an event from Slack's socket asks of the screen, the strongest
/// of a batch winning: a reload covers a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wake {
    Nothing,
    /// Read the counts, and reload if they moved.
    Check,
    /// Reload whatever the counts say: what changed is on the screen,
    /// and the counts would not show it.
    Reload,
}

/// How many of each recent section the list shows: enough to reach the
/// usual places, few enough to keep the list about what is new.
pub const RECENT_ROWS: usize = 8;

/// How long a typing notice holds without another: Slack repeats it every
/// few seconds while someone types.
const TYPING_FOR: Duration = Duration::from_secs(6);

/// Who is typing where, by channel and thread, with when Slack last said
/// so.
type Typing = BTreeMap<(String, Option<String>), BTreeMap<String, Instant>>;

/// The messages pane as drawn, for the keys that move by screens: only
/// the drawing knows how the text wraps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    /// Where the pane starts on its own in list focus, in rows: the new
    /// part of a history.
    pub anchor: u16,
    /// Rows the text scrolls in: inside the border, less the top row a
    /// thread taller than the pane keeps for its root.
    pub height: u16,
    /// Rows of the whole text, wrapped.
    pub rows: usize,
    /// Messages whose first row is on the screen.
    pub in_view: usize,
}

pub struct State {
    pub items: Vec<Item>,
    /// Index into `items`; headers are never selected.
    pub selected: Option<usize>,
    /// Rows scrolled in the messages pane (list focus only), from the
    /// viewport's anchor: negative shows what is above it.
    pub scroll: i32,
    /// What only the drawing knows about the pane, reported after each
    /// draw.
    pub viewport: Viewport,
    /// A `g` was pressed, waiting for the second one.
    pub pending_g: bool,
    pub mode: Mode,
    /// Shown in the bottom line until the next key.
    pub status: Option<String>,
    /// What the reply box completes, once loaded (empty after a failed
    /// load, so the box works without a list).
    pub candidates: Option<Vec<Candidate>>,
    pub me: Option<String>,
    /// Slack's most used emoji, which the `R` prompt offers first.
    pub frequent_emoji: Vec<String>,
    /// The conversations `x` selected for `e`, by channel and thread,
    /// with the newest message each had then.
    pub checked: BTreeMap<CheckKey, Option<String>>,
    pub focus: Focus,
    /// The highlighted message of the pane, when the pane has the focus.
    pub cursor: usize,
    pub thread: Option<ThreadPane>,
    pub typing: Typing,
    /// The search `/` brings back once it is closed or a hit is opened.
    pub last_search: Option<Search>,
    /// A recent conversation whose history failed to load: not tried
    /// again until the cursor leaves it or a reload.
    pub preview_failed: Option<String>,
}

impl State {
    pub fn new(items: Vec<Item>, me: Option<String>) -> State {
        let selected = (!items.is_empty()).then_some(0);
        State {
            items,
            selected,
            scroll: 0,
            mode: Mode::Browse,
            status: None,
            me,
            focus: Focus::List,
            cursor: 0,
            thread: None,
            viewport: Viewport::default(),
            pending_g: false,
            candidates: None,
            frequent_emoji: Vec::new(),
            checked: BTreeMap::new(),
            typing: BTreeMap::new(),
            last_search: None,
            preview_failed: None,
        }
    }

    /// The completion list for the reply box: the candidates that the
    /// `@word` or `#word` ending the text begins. None once Esc closed the
    /// list, before the names are loaded, or when nothing matches.
    pub fn completion(&self, draft: &Draft) -> Option<Completion> {
        let pick = draft.pick?;
        let (start, sigil, word) = completing(&draft.text)?;
        let word = word.to_lowercase();
        let matches: Vec<usize> = self
            .candidates
            .as_ref()?
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.insert.starts_with(sigil) && candidate.begins(&word))
            .map(|(index, _)| index)
            .collect();
        let last = matches.len().checked_sub(1)?;
        Some(Completion {
            start,
            matches,
            pick: pick.min(last),
        })
    }

    /// What the `R` prompt offers for the typed part: the reactions on
    /// the message, then the emoji you use most, then the standard ones.
    /// A name nothing matches is offered as typed, for a custom emoji
    /// you have not used yet.
    pub fn emoji_choices(&self, query: &str) -> Vec<String> {
        let query = emoji::normalize(query);
        let mut names: Vec<&str> = self
            .highlighted()
            .into_iter()
            .flat_map(|message| message.reactions.iter().map(|r| r.name.as_str()))
            .chain(self.frequent_emoji.iter().map(String::as_str))
            .collect();
        if !query.is_empty() {
            // Not `extend`, which would ask the list for `'static` names only.
            for code in emoji::matching(&query) {
                names.push(code);
            }
        }
        let mut choices: Vec<String> = Vec::new();
        for name in names {
            if choices.len() == REACT_ROWS {
                break;
            }
            if emoji::begins(name, &query)
                && !choices.iter().any(|choice| emoji::same(choice, name))
            {
                choices.push(name.to_string());
            }
        }
        if choices.is_empty() && !query.is_empty() {
            choices.push(query);
        }
        choices
    }

    /// The highlighted message, when the pane has the focus.
    pub fn highlighted(&self) -> Option<&Message> {
        (self.focus == Focus::Messages)
            .then(|| self.pane_messages().get(self.cursor))
            .flatten()
    }

    /// Whether you put this emoji on the highlighted message.
    pub fn reacted_with(&self, name: &str) -> bool {
        let Some(me) = self.me.as_deref() else {
            return false;
        };
        self.highlighted().is_some_and(|message| {
            message
                .reactions
                .iter()
                .any(|reaction| emoji::same(&reaction.name, name) && reaction.by(me))
        })
    }

    /// A reaction Slack took, yours or someone else's: the messages on
    /// the screen follow without being read again.
    pub fn apply_reaction(&mut self, channel: &str, ts: &str, user: &str, name: &str, add: bool) {
        for message in self.shown_messages(channel, ts) {
            message.react(name, user, add);
        }
    }

    /// Every copy of a message on the screen, in the thread pane and in
    /// the list alike.
    fn shown_messages(&mut self, channel: &str, ts: &str) -> impl Iterator<Item = &mut Message> {
        let thread = self
            .thread
            .iter_mut()
            .filter(move |thread| thread.channel == channel)
            .flat_map(|thread| thread.messages.iter_mut());
        let items = self
            .items
            .iter_mut()
            .filter(move |item| item.channel == channel)
            .flat_map(|item| item.messages.iter_mut());
        thread.chain(items).filter(move |message| message.ts == ts)
    }

    /// What an event from Slack's socket asks of the screen. A reaction
    /// and a reply's count are applied here, without a call; the rest is
    /// for the counts to tell.
    pub fn hear(&mut self, event: Event, now: Instant) -> Wake {
        match event {
            Event::Typing {
                channel,
                thread_ts,
                user,
            } => {
                if Some(&user) != self.me.as_ref() {
                    let typists = self.typing.entry((channel, thread_ts)).or_default();
                    typists.insert(user, now);
                }
                Wake::Nothing
            }
            Event::Moved => Wake::Check,
            Event::Reaction {
                channel,
                ts,
                user,
                name,
                add,
            } => {
                self.apply_reaction(&channel, &ts, &user, &name, add);
                Wake::Nothing
            }
            Event::Message(posted) => self.posted(&posted),
        }
    }

    fn posted(&mut self, posted: &Posted) -> Wake {
        let channel = posted.channel.as_str();
        // They sent what they were typing.
        if let Some(user) = &posted.user
            && !posted.changed
        {
            let key = (
                posted.channel.clone(),
                posted.reply_to().map(str::to_string),
            );
            if let Some(typists) = self.typing.get_mut(&key) {
                typists.remove(user);
            }
        }
        if posted.changed {
            // A reload rebuilds a recent one from the counts, which an
            // edit does not move: its history is read again instead.
            for item in &mut self.items {
                if item.section.is_recent() && item.channel == channel {
                    item.loaded = false;
                }
            }
            return if self.shows_message(channel, &posted.ts, false) {
                Wake::Reload
            } else {
                Wake::Nothing
            };
        }
        // Yours, sent from here or from another client: the cursor it
        // moves comes as an event of its own. A reply from another client
        // is still missing from its thread on the screen.
        if posted.user.is_some() && posted.user == self.me {
            let missing = posted.reply_to().is_some_and(|root| {
                self.shows_thread(channel, root) && !self.shows_message(channel, &posted.ts, true)
            });
            return if missing { Wake::Reload } else { Wake::Nothing };
        }
        let Some(root) = posted.reply_to() else {
            return Wake::Check;
        };
        if self.shows_thread(channel, root) {
            return Wake::Reload;
        }
        for message in self.shown_messages(channel, root) {
            message.reply_count += 1;
        }
        // A thread you follow moves the threads block.
        Wake::Check
    }

    /// Drop the typing notices that were not repeated.
    pub fn forget_typing(&mut self, now: Instant) {
        self.typing.retain(|_, typists| {
            typists.retain(|_, seen| now.duration_since(*seen) < TYPING_FOR);
            !typists.is_empty()
        });
    }

    /// Who is typing in this conversation, or in this thread.
    pub fn typists(&self, channel: &str, thread_ts: Option<&str>) -> Vec<&str> {
        self.typing
            .iter()
            .filter(|((at, thread), _)| at == channel && thread.as_deref() == thread_ts)
            .flat_map(|(_, typists)| typists.keys().map(String::as_str))
            .collect()
    }

    /// The message is on the screen, and with `browsed` false only where a
    /// reload reads it again: an opened conversation keeps the history it
    /// was opened with.
    fn shows_message(&self, channel: &str, ts: &str, browsed: bool) -> bool {
        let has = |messages: &[Message]| messages.iter().any(|message| message.ts == ts);
        self.items.iter().any(|item| {
            item.channel == channel
                && (browsed || !item.section.is_browsed())
                && has(&item.messages)
        }) || self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.channel == channel && has(&thread.messages))
    }

    /// A thread listed, or open in the pane: shown whole, so a new reply
    /// is read again.
    fn shows_thread(&self, channel: &str, root: &str) -> bool {
        self.items
            .iter()
            .any(|item| item.key() == (channel, Some(root)))
            || self
                .thread
                .as_ref()
                .is_some_and(|thread| thread.channel == channel && thread.thread_ts == root)
    }

    pub fn is_checked(&self, item: &Item) -> bool {
        self.checked_upto(item).is_some()
    }

    /// As far as `e` marks a selected conversation. A lookup by
    /// comparison: the selection is a handful, and the list is drawn
    /// four times a second.
    fn checked_upto(&self, item: &Item) -> Option<&Option<String>> {
        self.checked
            .iter()
            .find_map(|(key, upto)| item.is(key).then_some(upto))
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.selected.and_then(|index| self.items.get(index))
    }

    /// What the messages pane shows: the open thread, else the selected
    /// item's messages.
    pub fn pane_messages(&self) -> &[Message] {
        match &self.thread {
            Some(thread) => &thread.messages,
            None => self
                .selected_item()
                .map_or(&[], |item| item.messages.as_slice()),
        }
    }

    pub fn open_thread(&mut self, pane: ThreadPane) {
        self.thread = Some(pane);
        self.focus = Focus::Messages;
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Put an opened conversation in the list, in place of an earlier
    /// opening of the same one or of its recent row, and read it from its
    /// newest message.
    pub fn open_item(&mut self, item: Item) {
        let index = match self
            .items
            .iter()
            .position(|it| it.section == Section::Opened && it.channel == item.channel)
        {
            Some(index) => {
                self.items[index] = item;
                index
            }
            None => {
                self.items
                    .retain(|it| !(it.section.is_recent() && it.channel == item.channel));
                let index = self
                    .items
                    .iter()
                    .position(|it| it.section.is_recent())
                    .unwrap_or(self.items.len());
                self.items.insert(index, item);
                index
            }
        };
        self.checked
            .retain(|key, _| self.items.iter().any(|item| item.is(key)));
        self.selected = Some(index);
        self.thread = None;
        self.scroll = 0;
        self.read_from_landing();
    }

    /// What Slack found for a query, in the search mode it was asked from.
    pub fn found(&mut self, query: String, found: Found) {
        if let Mode::Search(search) = &mut self.mode {
            search.searched = Some(query);
            search.total = found.total;
            search.hits = found.hits;
            search.selected = 0;
            search.typing = search.hits.is_empty();
        }
    }

    /// The pane focused on this message, when it shows it.
    pub fn highlight(&mut self, ts: &str) {
        if let Some(at) = self.pane_messages().iter().position(|m| m.ts == ts) {
            self.focus = Focus::Messages;
            self.cursor = at;
        }
    }

    /// The pane focused where reading starts (`Item::landing`), or the
    /// list when there is nothing to read.
    pub fn read_from_landing(&mut self) {
        match self
            .selected_item()
            .filter(|item| !item.messages.is_empty())
            .map(Item::landing)
        {
            Some(landing) => {
                self.focus = Focus::Messages;
                self.cursor = landing;
            }
            None => self.focus = Focus::List,
        }
    }

    /// New items after a refresh: the unread ones (None keeps those
    /// listed), then the opened conversations, which stay, then the
    /// recent ones that are listed nowhere above, `RECENT_ROWS` of each
    /// section, each keeping the history it had. A refresh comes on its
    /// own every few seconds, so it never moves you: the selection, the
    /// focus, the highlighted message and the open thread follow the same
    /// conversation while it is still there. Once it is gone (read
    /// elsewhere), its neighbour is selected from the list.
    pub fn replace_items(&mut self, unread: Option<Vec<Item>>, mut recent: Vec<Item>) {
        // Channels, then people, each in the order they were visited.
        recent.sort_by_key(|item| item.section == Section::RecentPeople);
        let previous = self.selected;
        let key = self
            .selected_item()
            .map(|item| (item.channel.clone(), item.thread_ts.clone()));
        // New messages shift the indices: the positions are held by ts.
        let ts_at = |index: usize| {
            self.selected_item()
                .and_then(|item| item.messages.get(index))
                .map(|message| message.ts.clone())
        };
        let cursor_ts = (self.focus == Focus::Messages && self.thread.is_none())
            .then(|| ts_at(self.cursor))
            .flatten();
        let from_ts = self
            .thread
            .as_ref()
            .and_then(|thread| ts_at(thread.from_message));
        let mut listed = Vec::new();
        let mut opened = Vec::new();
        let mut previews = BTreeMap::new();
        for item in std::mem::take(&mut self.items) {
            match item.section {
                Section::Opened => opened.push(item),
                section if section.is_recent() => {
                    previews.insert(item.channel.clone(), item);
                }
                _ => listed.push(item),
            }
        }
        self.items = unread.unwrap_or(listed);
        self.items.extend(opened);
        for mut item in recent {
            let shown = self.items.iter().filter(|it| it.section == item.section);
            if shown.count() == RECENT_ROWS
                || self
                    .items
                    .iter()
                    .any(|it| it.channel == item.channel && it.thread_ts.is_none())
            {
                continue;
            }
            if let Some(old) = previews.remove(&item.channel) {
                item.keep_history(old);
            }
            self.items.push(item);
        }
        self.checked
            .retain(|key, _| self.items.iter().any(|item| item.is(key)));
        let found = key.and_then(|(channel, thread_ts)| {
            self.items
                .iter()
                .position(|item| item.key() == (channel.as_str(), thread_ts.as_deref()))
        });
        let Some(index) = found else {
            self.selected =
                (!self.items.is_empty()).then(|| previous.unwrap_or(0).min(self.items.len() - 1));
            self.back_to_list();
            return;
        };
        self.selected = Some(index);
        let messages = &self.items[index].messages;
        let position = |ts: Option<String>, fallback: usize| {
            ts.and_then(|ts| messages.iter().position(|message| message.ts == ts))
                .unwrap_or_else(|| fallback.min(messages.len().saturating_sub(1)))
        };
        if let Some(thread) = &mut self.thread {
            thread.from_message = position(from_ts, thread.from_message);
        } else if self.focus == Focus::Messages {
            if messages.is_empty() {
                self.back_to_list();
            } else {
                self.cursor = position(cursor_ts, self.cursor);
            }
        }
    }

    /// How many conversations are opened, which the recent sections
    /// leave out.
    pub fn opened(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.section == Section::Opened)
            .count()
    }

    /// The selected conversation, when its history is still to be read.
    pub fn to_preview(&self) -> Option<usize> {
        self.selected.filter(|&index| {
            let item = &self.items[index];
            item.section.is_recent() && !item.loaded
        })
    }

    /// The history of a conversation read, in place: the highlighted
    /// message stays where it was, or the pane lands where reading
    /// starts when `enter` asks for it.
    pub fn preview(&mut self, index: usize, page: history::Unread, enter: bool) {
        let selected = self.selected == Some(index);
        let reading = selected && self.focus == Focus::Messages && self.thread.is_none();
        let Some(item) = self.items.get_mut(index) else {
            return;
        };
        let cursor_ts = reading
            .then(|| item.messages.get(self.cursor).map(|m| m.ts.clone()))
            .flatten();
        item.fill(page);
        if !selected {
            return;
        }
        if enter {
            self.thread = None;
            self.scroll = 0;
            self.read_from_landing();
        } else if reading {
            let messages = &self.items[index].messages;
            self.cursor = cursor_ts
                .and_then(|ts| messages.iter().position(|message| message.ts == ts))
                .unwrap_or_else(|| self.items[index].landing());
        }
    }

    /// A conversation read without leaving the list: a recent one.
    pub fn read_in_place(&mut self, index: usize) {
        if let Some(item) = self.items.get_mut(index) {
            item.unread_from = None;
            item.describe();
        }
    }

    /// Out of the selection, once `e` is done with it.
    pub fn uncheck(&mut self, index: usize) {
        if let Some(item) = self.items.get(index) {
            self.checked.remove(&item.check_key());
        }
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            let item = self.items.remove(index);
            self.checked.remove(&item.check_key());
        }
        self.selected = if self.items.is_empty() {
            None
        } else {
            Some(index.min(self.items.len() - 1))
        };
        self.back_to_list();
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        self.status = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Browse => self.browse(key),
            Mode::Compose(draft) => self.compose(draft, key),
            Mode::Help => Action::None,
            Mode::Pick(picker) => self.pick(picker, key),
            Mode::React(reacting) => self.react(reacting, key),
            Mode::Search(search) => self.search(search, key),
            Mode::Links { links, copy } => self.choose_link(links, copy, key),
        }
    }

    /// Pasted text goes where typing would: the reply box, line breaks
    /// included, or the picker's filter. Anywhere else it is dropped
    /// rather than read as keys.
    pub fn paste(&mut self, pasted: &str) -> Action {
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Compose(mut draft) => {
                draft
                    .text
                    .push_str(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
                return self.typed(draft);
            }
            Mode::Pick(mut picker) if picker.filtering => {
                picker
                    .query
                    .extend(pasted.chars().filter(|c| !c.is_control()));
                picker.selected = 0;
                self.mode = Mode::Pick(picker);
            }
            Mode::React(mut reacting) => {
                reacting
                    .query
                    .extend(pasted.chars().filter(|c| !c.is_whitespace()));
                reacting.pick = 0;
                self.mode = Mode::React(reacting);
            }
            Mode::Search(mut search) if search.typing => {
                search
                    .query
                    .extend(pasted.chars().filter(|c| !c.is_control()));
                self.mode = Mode::Search(search);
            }
            other => self.mode = other,
        }
        Action::None
    }

    /// `q` and Ctrl-C quit from anywhere; Esc and `h` step back; `gg`,
    /// `G` and the Ctrl keys of vim move by screens. The letters are
    /// Gmail's where Gmail has the action (`e`, `x`, `r`, `c`, `o`), and
    /// Slack's `R` for a reaction.
    fn browse(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let after_g = std::mem::take(&mut self.pending_g);
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('r') if control => Action::Refresh,
            KeyCode::Char('d') if control => self.page(true, true),
            KeyCode::Char('u') if control => self.page(false, true),
            KeyCode::Char('f') if control => self.page(true, false),
            KeyCode::Char('b') if control => self.page(false, false),
            KeyCode::PageDown => self.page(true, false),
            KeyCode::PageUp => self.page(false, false),
            // Esc alone unselects: `h` and Left are pressed out of vim
            // habit, and a selection is built one `x` at a time.
            KeyCode::Esc if self.focus == Focus::List && !self.checked.is_empty() => {
                self.checked.clear();
                Action::None
            }
            // Esc backs out of whatever is open, as everywhere else on
            // the screen; at the list there is nothing to back out of.
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
                self.back();
                Action::None
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => self.deeper(),
            KeyCode::Tab => self.jump(true),
            KeyCode::BackTab => self.jump(false),
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('g') if after_g => self.step(isize::MIN),
            KeyCode::Char('g') => {
                self.pending_g = true;
                Action::None
            }
            KeyCode::Home => self.step(isize::MIN),
            KeyCode::Char('G') | KeyCode::End => self.step(isize::MAX),
            KeyCode::Char('x') => {
                self.check();
                Action::None
            }
            KeyCode::Char('e') => self.mark(),
            KeyCode::Char('u') => self.link(false),
            KeyCode::Char('y') => self.link(true),
            KeyCode::Char('R') => {
                if self.highlighted().is_some() {
                    self.mode = Mode::React(Reacting::default());
                } else {
                    self.status = Some("Move onto a message first (l)".to_string());
                }
                Action::None
            }
            KeyCode::Char('c') => Action::OpenPicker { compose: true },
            KeyCode::Char('o') => Action::OpenPicker { compose: false },
            KeyCode::Char('/') => {
                let mut search = self.last_search.take().unwrap_or_default();
                search.typing = true;
                self.mode = Mode::Search(search);
                Action::None
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                Action::None
            }
            KeyCode::Char('O') => self.selected.map_or(Action::None, |item| Action::Open {
                item,
                message: (self.focus == Focus::Messages).then_some(self.cursor),
            }),
            KeyCode::Char('r') => self.reply(),
            KeyCode::Char('s') => {
                self.compose_to_conversation();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// `x`: select the conversation for `e`, or unselect it. It keeps the
    /// newest message the conversation has now, as far as `e` will mark.
    fn check(&mut self) {
        let Some(item) = self.selected_item() else {
            return;
        };
        let (key, latest) = (item.check_key(), item.latest.clone());
        if self.checked.remove(&key).is_none() {
            self.checked.insert(key, latest);
        }
    }

    /// `e`: the selected conversations read, else the one under the
    /// cursor; an opened one is closed instead.
    fn mark(&self) -> Action {
        let marks: Vec<Mark> = if self.checked.is_empty() {
            self.selected
                .map(|item| Mark {
                    item,
                    upto: self.items[item].latest.clone(),
                })
                .into_iter()
                .collect()
        } else {
            self.items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    self.checked_upto(item).map(|upto| Mark {
                        item: index,
                        upto: upto.clone(),
                    })
                })
                .collect()
        };
        if marks.is_empty() {
            Action::None
        } else {
            Action::MarkRead(marks)
        }
    }

    /// `r`: reply where you are. From the list, to the conversation (in
    /// the thread for a thread item); on a message of the pane, in that
    /// message's thread; inside a thread, in it.
    fn reply(&mut self) -> Action {
        let Some(item) = self.selected else {
            return Action::None;
        };
        let thread_ts = match (self.focus, &self.thread) {
            (Focus::List, _) => self.items[item].thread_ts.clone(),
            (Focus::Messages, Some(thread)) => Some(thread.thread_ts.clone()),
            (Focus::Messages, None) => match self.items[item].messages.get(self.cursor) {
                Some(message) => Some(
                    message
                        .thread_ts
                        .clone()
                        .unwrap_or_else(|| message.ts.clone()),
                ),
                None => return Action::None,
            },
        };
        self.mode = Mode::Compose(Draft::new(thread_ts));
        Action::None
    }

    /// `s`, and the end of `c`: a message to the selected conversation
    /// itself, wherever the focus is in it (a thread item takes it in
    /// the thread, since that is the conversation it stands for).
    pub fn compose_to_conversation(&mut self) {
        if let Some(item) = self.selected {
            self.mode = Mode::Compose(Draft::new(self.items[item].thread_ts.clone()));
        }
    }

    /// One level deeper, the way `l` moves in vim and Enter opens in
    /// Gmail: the messages pane from the list, then the highlighted
    /// message's thread.
    fn deeper(&mut self) -> Action {
        match (self.focus, &self.thread) {
            (Focus::List, _) => {
                if let Some(item) = self.to_preview()
                    && self.items[item].messages.is_empty()
                {
                    return Action::Preview { item, enter: true };
                }
                self.read_from_landing();
                Action::None
            }
            (Focus::Messages, None) => {
                self.selected
                    .map_or(Action::None, |item| Action::OpenThread {
                        item,
                        message: self.cursor,
                    })
            }
            (Focus::Messages, Some(_)) => Action::None,
        }
    }

    /// The reply box. Enter sends and Esc gives up, unless a completion
    /// list is open above the box: then Tab or Enter take the highlighted
    /// entry, Up and Down (Ctrl-p, Ctrl-n too) move in it, Esc closes it.
    /// A line break (`breaks_line`) is one whatever is open.
    fn compose(&mut self, mut draft: Draft, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(completion) = self.completion(&draft) {
            let last = completion.matches.len() - 1;
            let down = Some((completion.pick + 1).min(last));
            let up = Some(completion.pick.saturating_sub(1));
            match key.code {
                KeyCode::Tab | KeyCode::Enter if !breaks_line(key) => {
                    let insert = self
                        .candidates
                        .as_ref()
                        .and_then(|candidates| candidates.get(completion.matches[completion.pick]))
                        .map(|candidate| candidate.insert.clone());
                    if let Some(insert) = insert {
                        draft.accept(completion.start, &insert);
                    }
                }
                KeyCode::Down => draft.pick = down,
                KeyCode::Char('n') if control => draft.pick = down,
                KeyCode::Up | KeyCode::BackTab => draft.pick = up,
                KeyCode::Char('p') if control => draft.pick = up,
                KeyCode::Esc => draft.pick = None,
                _ => return self.edit(draft, key),
            }
            self.mode = Mode::Compose(draft);
            return Action::None;
        }
        self.edit(draft, key)
    }

    /// A key into the text of the reply box: Enter sends it, a line break
    /// goes into it, a deleting key (`Cut`) takes from its end, Esc drops
    /// it.
    fn edit(&mut self, mut draft: Draft, key: KeyEvent) -> Action {
        if let Some(cut) = Cut::of(key) {
            cut.apply(&mut draft.text);
            return self.typed(draft);
        }
        match key.code {
            KeyCode::Esc => Action::None,
            _ if breaks_line(key) => {
                draft.text.push('\n');
                self.typed(draft)
            }
            KeyCode::Enter => {
                let trimmed = draft.text.trim().to_string();
                match self.selected {
                    Some(item) if !trimmed.is_empty() => Action::Send {
                        item,
                        text: trimmed,
                        thread_ts: draft.thread_ts,
                    },
                    Some(_) => {
                        self.mode = Mode::Compose(draft);
                        Action::None
                    }
                    None => Action::None,
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.text.push(c);
                self.typed(draft)
            }
            _ => {
                self.mode = Mode::Compose(draft);
                Action::None
            }
        }
    }

    /// The text changed: the completion list opens again, and the names
    /// it needs are loaded the first time a `@` or `#` word appears.
    fn typed(&mut self, mut draft: Draft) -> Action {
        draft.pick = Some(0);
        let load = self.candidates.is_none() && completing(&draft.text).is_some();
        self.mode = Mode::Compose(draft);
        if load {
            Action::LoadCandidates
        } else {
            Action::None
        }
    }

    /// The picker opens in its filter, where typing narrows the list and
    /// Enter opens the highlighted match (Esc clears what was typed, and
    /// with nothing typed closes the picker). Tab marks a person for a
    /// group message and clears the filter for the next one; Enter then
    /// opens the group, with the person typed last when there is one.
    /// Enter with nothing typed nor marked hands the keys to the list,
    /// where `j`/`k`, `gg` and `G` move, `/` goes back to the filter,
    /// Space or Tab marks, `m` shows the muted ones, Enter or `l` opens,
    /// Esc or `h` gives up, `q` quits as everywhere. The arrows and
    /// Ctrl-n/p move in both.
    fn pick(&mut self, mut picker: Picker, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let after_g = std::mem::take(&mut self.pending_g);
        match key.code {
            KeyCode::Down => picker.step(1),
            KeyCode::Up => picker.step(-1),
            KeyCode::Char('n') if control => picker.step(1),
            KeyCode::Char('p') if control => picker.step(-1),
            KeyCode::Tab if picker.filtering => picker.mark_and_clear(),
            KeyCode::Tab => {
                picker.mark();
            }
            KeyCode::Enter if picker.filtering && picker.enter_lists() => picker.filtering = false,
            KeyCode::Enter
                if picker.filtering && !picker.typed().is_empty() && !picker.marked.is_empty() =>
            {
                if picker.join()
                    && let Some(action) = picker.chosen()
                {
                    return action;
                }
            }
            KeyCode::Esc if picker.filtering && picker.query.is_empty() => return Action::None,
            KeyCode::Esc if picker.filtering => {
                picker.query.clear();
                picker.selected = 0;
            }
            KeyCode::Left | KeyCode::Right if picker.filtering => {}
            KeyCode::Backspace if picker.filtering => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c) if picker.filtering && !control => {
                picker.query.push(c);
                picker.selected = 0;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(action) = picker.chosen() {
                    return action;
                }
            }
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => return Action::None,
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('/') => picker.filtering = true,
            KeyCode::Char(' ') => {
                picker.mark();
            }
            KeyCode::Char('m') => {
                picker.show_muted = !picker.show_muted;
                picker.step(0);
            }
            KeyCode::Char('j') => picker.step(1),
            KeyCode::Char('k') => picker.step(-1),
            KeyCode::Char('g') if after_g => picker.step(isize::MIN),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('G') => picker.step(isize::MAX),
            _ => {}
        }
        self.mode = Mode::Pick(picker);
        Action::None
    }

    /// The search opens in its query line, the last search's query and
    /// hits still there, where Enter asks Slack (Ctrl-U clears the line
    /// for a new query) or, the query unchanged, goes to the hits. There
    /// `j`/`k` move, Enter or `l` opens the hit in its conversation, `O`
    /// opens it in Slack and `y` copies its link, `/` goes back to the
    /// query, Esc or `h` closes, `q` quits as everywhere. The arrows and
    /// Ctrl-n/p move in both. Closing keeps the search for the next `/`.
    fn search(&mut self, mut search: Search, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let after_g = std::mem::take(&mut self.pending_g);
        let to_hits = |search: &mut Search| search.typing = search.hits.is_empty();
        if search.typing {
            if let Some(cut) = Cut::of(key) {
                cut.apply(&mut search.query);
                self.mode = Mode::Search(search);
                return Action::None;
            }
            match key.code {
                KeyCode::Esc => {
                    self.last_search = Some(search);
                    return Action::None;
                }
                KeyCode::Enter if search.enter_hits() => to_hits(&mut search),
                KeyCode::Enter if !search.query.trim().is_empty() => {
                    let query = search.query.trim().to_string();
                    self.mode = Mode::Search(search);
                    return Action::Search { query };
                }
                KeyCode::Down => to_hits(&mut search),
                KeyCode::Char('n') if control => to_hits(&mut search),
                KeyCode::Char(c) if !control => search.query.push(c),
                _ => {}
            }
            self.mode = Mode::Search(search);
            return Action::None;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => search.step(1),
            KeyCode::Up | KeyCode::Char('k') => search.step(-1),
            KeyCode::Char('n') if control => search.step(1),
            KeyCode::Char('p') if control => search.step(-1),
            KeyCode::Char('g') if after_g => search.step(isize::MIN),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => search.step(isize::MIN),
            KeyCode::Char('G') | KeyCode::End => search.step(isize::MAX),
            KeyCode::Char('/') => search.typing = true,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(hit) = search.highlighted().cloned() {
                    self.last_search = Some(search);
                    return Action::ShowHit(hit);
                }
            }
            KeyCode::Char(c @ ('O' | 'y')) => {
                if let Some(url) = search.highlighted().map(|hit| hit.permalink.clone()) {
                    self.mode = Mode::Search(search);
                    return link_action(url, c == 'y');
                }
            }
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
                self.last_search = Some(search);
                return Action::None;
            }
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
        self.mode = Mode::Search(search);
        Action::None
    }

    /// `u` and `y`: a link of the highlighted message, opened or copied.
    /// One link goes straight away; several ask which; none, or no
    /// message under the cursor, is a status line.
    fn link(&mut self, copy: bool) -> Action {
        if self.focus != Focus::Messages {
            self.status = Some("Move onto a message first (l)".to_string());
            return Action::None;
        }
        let mut links = self
            .pane_messages()
            .get(self.cursor)
            .map(|message| mrkdwn::links(&message.text))
            .unwrap_or_default();
        match links.len() {
            0 => {
                self.status = Some("No link in this message".to_string());
                Action::None
            }
            1 => link_action(links.swap_remove(0).url, copy),
            _ => {
                // A digit picks, so the prompt stops at nine.
                links.truncate(9);
                self.mode = Mode::Links { links, copy };
                Action::None
            }
        }
    }

    /// The `R` prompt: typing filters, Up and Down (Tab, Ctrl-n and
    /// Ctrl-p too) move, Enter puts the highlighted emoji on the message
    /// or takes yours back, Esc gives up.
    fn react(&mut self, mut reacting: Reacting, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(cut) = Cut::of(key) {
            cut.apply(&mut reacting.query);
            reacting.pick = 0;
            self.mode = Mode::React(reacting);
            return Action::None;
        }
        let choices = self.emoji_choices(&reacting.query);
        let last = choices.len().saturating_sub(1);
        let down = (reacting.pick + 1).min(last);
        let up = reacting.pick.saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Action::None,
            KeyCode::Enter => {
                if let Some(name) = choices.get(reacting.pick.min(last)) {
                    return self.reaction(name);
                }
            }
            KeyCode::Down | KeyCode::Tab => reacting.pick = down,
            KeyCode::Char('n') if control => reacting.pick = down,
            KeyCode::Up | KeyCode::BackTab => reacting.pick = up,
            KeyCode::Char('p') if control => reacting.pick = up,
            KeyCode::Char(c) if !control && !c.is_whitespace() => {
                reacting.query.push(c);
                reacting.pick = 0;
            }
            _ => {}
        }
        self.mode = Mode::React(reacting);
        Action::None
    }

    /// The chosen emoji on the highlighted message: put on, or taken back
    /// when it is already yours, the way a click on it works in the app.
    fn reaction(&self, name: &str) -> Action {
        let channel = match &self.thread {
            Some(thread) => Some(thread.channel.clone()),
            None => self.selected_item().map(|item| item.channel.clone()),
        };
        match (self.highlighted(), channel) {
            (Some(message), Some(channel)) => Action::React {
                channel,
                ts: message.ts.clone(),
                // The message's name for it is one Slack takes.
                name: message
                    .reactions
                    .iter()
                    .find(|reaction| emoji::same(&reaction.name, name))
                    .map_or(name, |reaction| reaction.name.as_str())
                    .to_string(),
                add: !self.reacted_with(name),
            },
            _ => Action::None,
        }
    }

    /// A digit picks a link of the prompt; any other key gives up.
    fn choose_link(&mut self, mut links: Vec<Link>, copy: bool, key: KeyEvent) -> Action {
        if let KeyCode::Char(c) = key.code
            && let Some(index) = c.to_digit(10).map(|digit| digit as usize)
            && (1..=links.len()).contains(&index)
        {
            return link_action(links.swap_remove(index - 1).url, copy);
        }
        Action::None
    }

    /// One level back: close the thread, else leave the messages pane.
    fn back(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.cursor = thread.from_message;
            self.scroll = 0;
        } else if self.focus == Focus::Messages {
            self.focus = Focus::List;
            self.cursor = 0;
        }
    }

    fn back_to_list(&mut self) {
        self.thread = None;
        self.focus = Focus::List;
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Moves the selection, or the highlighted message once the pane
    /// has the focus; `isize::MIN` and `isize::MAX` reach the ends.
    fn step(&mut self, step: isize) -> Action {
        match self.focus {
            Focus::List => self.select(step),
            Focus::Messages => {
                let len = self.pane_messages().len();
                if len == 0 {
                    return Action::None;
                }
                let last = len as isize - 1;
                self.cursor = (self.cursor as isize).saturating_add(step).clamp(0, last) as usize;
            }
        }
        Action::None
    }

    /// Half a screen (Ctrl-D, Ctrl-U) or a whole one (Ctrl-F, Ctrl-B).
    /// From the list the pane scrolls by rows, within its text; in the
    /// pane the highlighted message moves by as many messages as the
    /// screen shows, and the pane follows it.
    fn page(&mut self, down: bool, half: bool) -> Action {
        let Viewport {
            anchor,
            height,
            rows,
            in_view,
        } = self.viewport;
        match self.focus {
            Focus::List => {
                let step = i32::from(if half { height / 2 } else { height }).max(1);
                let step = if down { step } else { -step };
                // The pane started `anchor` rows down the text, and the
                // last screen of the text starts `height` rows before its end.
                let top = -i32::from(anchor);
                let last = i32::try_from(rows.saturating_sub(usize::from(height)))
                    .unwrap_or(i32::MAX)
                    .saturating_add(top)
                    .max(top);
                self.scroll = self.scroll.saturating_add(step).clamp(top, last);
                Action::None
            }
            Focus::Messages => {
                let messages = if half { in_view / 2 } else { in_view }.max(1) as isize;
                self.step(if down { messages } else { -messages })
            }
        }
    }

    /// Tab, Shift-Tab: the first conversation of the next section, of
    /// the previous one, from the list or from the pane.
    fn jump(&mut self, forward: bool) -> Action {
        let Some(current) = self.selected else {
            return Action::None;
        };
        let start = |mut index: usize| {
            let section = self.items[index].section;
            while index > 0 && self.items[index - 1].section == section {
                index -= 1;
            }
            index
        };
        let section = self.items[current].section;
        let target = if forward {
            (current..self.items.len()).find(|&index| self.items[index].section != section)
        } else {
            start(current).checked_sub(1).map(start)
        };
        if let Some(target) = target {
            self.back_to_list();
            self.selected = Some(target);
        }
        Action::None
    }

    fn select(&mut self, step: isize) {
        let Some(current) = self.selected else {
            return;
        };
        let last = self.items.len().saturating_sub(1) as isize;
        let next = (current as isize).saturating_add(step).clamp(0, last);
        if next as usize != current {
            self.selected = Some(next as usize);
            self.thread = None;
            self.cursor = 0;
            self.scroll = 0;
        }
    }
}

fn link_action(url: String, copy: bool) -> Action {
    if copy {
        Action::CopyLink(url)
    } else {
        Action::OpenLink(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_is_read_whole_from_its_first_new_reply() {
        let thread = |history: Option<crate::replies::Replies>| crate::threads::Thread {
            channel: "C1".into(),
            thread_ts: "1.000100".into(),
            root: message("1.000100"),
            unread_replies: vec![message("1.000300"), message("1.000400")],
            has_more: false,
            history,
        };
        let whole = |has_more: bool| crate::replies::Replies {
            root: message("1.000100"),
            replies: ["1.000200", "1.000300", "1.000400", "1.000500"]
                .map(message)
                .to_vec(),
            has_more,
        };
        let names = Names::default();

        let item = Item::thread(thread(Some(whole(false))), &names);
        assert_eq!(item.messages.len(), 5, "the root, then every reply");
        assert_eq!(item.unread_from.as_deref(), Some("1.000200"));
        assert_eq!(
            item.detail, "3 new replies",
            "one came in after the thread view"
        );
        assert_eq!(item.landing(), 2);
        assert_eq!(
            item.latest.as_deref(),
            Some("1.000500"),
            "e marks what the pane shows"
        );

        let item = Item::thread(thread(Some(whole(true))), &names);
        assert_eq!(
            item.messages.len(),
            2,
            "cut by the page cap: the new replies alone"
        );
        let item = Item::thread(thread(None), &names);
        assert_eq!(item.messages.len(), 2, "unreadable: the new replies alone");
        assert_eq!(
            (
                item.unread_from,
                item.detail.as_str(),
                item.latest.as_deref()
            ),
            (None, "2 new replies", Some("1.000400"))
        );
    }

    fn message(ts: &str) -> Message {
        Message {
            ts: ts.to_string(),
            user: None,
            bot: None,
            text: String::new(),
            subtype: None,
            thread_ts: None,
            reply_count: 0,
            files: Vec::new(),
            attachments: Vec::new(),
            reactions: Vec::new(),
        }
    }

    fn item(section: Section, channel: &str, thread_ts: Option<&str>) -> Item {
        Item {
            section,
            channel: channel.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            title: channel.to_string(),
            detail: String::new(),
            messages: vec![message("1.000100"), message("1.000200")],
            has_more: false,
            latest: Some("1.000200".to_string()),
            unread_from: None,
            loaded: true,
            muted: false,
        }
    }

    /// A refresh with these unread items and no recent ones.
    fn refresh(state: &mut State, items: Vec<Item>) {
        state.replace_items(Some(items), Vec::new());
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn state() -> State {
        State::new(
            vec![
                item(Section::DirectMessages, "D1", None),
                item(Section::Mentions, "C1", None),
                item(Section::Threads, "C2", Some("2.000100")),
            ],
            None,
        )
    }

    fn choice(kind: ChoiceKind, id: &str, label: &str, search: &str) -> Choice {
        Choice {
            kind,
            id: id.to_string(),
            label: label.to_string(),
            search: search.to_string(),
            note: String::new(),
            muted: false,
            unread_from: None,
            user: None,
        }
    }

    fn candidate(insert: &str, detail: &str, search: &str) -> Candidate {
        Candidate {
            insert: insert.to_string(),
            detail: detail.to_string(),
            search: search.to_string(),
        }
    }

    #[test]
    fn rows_put_a_header_before_each_section() {
        let state = state();
        let rows = rows(&state.items);
        assert_eq!(rows.len(), 6);
        assert!(matches!(rows[0], Row::Header(Section::DirectMessages)));
        assert!(matches!(rows[1], Row::Item(0)));
        assert!(matches!(rows[2], Row::Header(Section::Mentions)));
        assert_eq!(row_of(&rows, 2), Some(5));
        assert_eq!(row_of(&rows, 7), None);
    }

    #[test]
    fn moving_stays_within_the_items() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('k'))), Action::None);
        assert_eq!(state.selected, Some(0));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Down));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(state.selected, Some(2));
        state.scroll = 7;
        state.key(press(KeyCode::Up));
        assert_eq!((state.selected, state.scroll), (Some(1), 0));
    }

    #[test]
    fn replying_from_the_list_goes_through_the_compose_mode() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('r'))), Action::None);
        assert_eq!(state.mode, Mode::Compose(Draft::new(None)));
        for c in "ok!".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Backspace));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Send {
                item: 0,
                text: "ok".to_string(),
                thread_ts: None
            }
        );
        assert_eq!(state.mode, Mode::Browse);

        state.key(press(KeyCode::Char('s')));
        state.key(press(KeyCode::Char(' ')));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "blank text is not sent"
        );
        assert!(matches!(state.mode, Mode::Compose(_)));
        state.key(press(KeyCode::Esc));
        assert_eq!(state.mode, Mode::Browse);

        // A thread item from the list replies in its thread.
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('r')));
        assert_eq!(
            state.mode,
            Mode::Compose(Draft::new(Some("2.000100".to_string())))
        );
    }

    #[test]
    fn the_messages_pane_opens_threads_and_replies_in_them() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('l'))), Action::None);
        assert_eq!(state.focus, Focus::Messages);
        state.key(press(KeyCode::Char('j')));
        assert_eq!(state.cursor, 1);
        state.key(press(KeyCode::Char('j')));
        assert_eq!(state.cursor, 1, "stays on the last message");
        assert_eq!(state.key(press(KeyCode::Char('r'))), Action::None);
        assert_eq!(
            state.mode,
            Mode::Compose(Draft::new(Some("1.000200".to_string()))),
            "Enter on a message replies in its thread"
        );
        state.key(press(KeyCode::Esc));
        assert_eq!(
            state.key(press(KeyCode::Char('l'))),
            Action::OpenThread {
                item: 0,
                message: 1
            }
        );

        state.open_thread(ThreadPane {
            channel: "D1".into(),
            thread_ts: "1.000200".into(),
            title: "@jane › no rush".into(),
            messages: vec![message("1.000200"), message("1.000300")],
            has_more: false,
            from_message: 1,
        });
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 0));
        state.key(press(KeyCode::Char('r')));
        assert_eq!(
            state.mode,
            Mode::Compose(Draft::new(Some("1.000200".to_string())))
        );
        state.key(press(KeyCode::Esc));

        assert_eq!(
            state.key(press(KeyCode::Esc)),
            Action::None,
            "closes the thread"
        );
        assert!(state.thread.is_none());
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));
        assert_eq!(
            state.key(press(KeyCode::Esc)),
            Action::None,
            "leaves the pane"
        );
        assert_eq!(state.focus, Focus::List);
        assert_eq!(
            state.key(press(KeyCode::Esc)),
            Action::None,
            "at the list there is nothing to back out of; q quits"
        );
        assert_eq!(state.focus, Focus::List);
    }

    #[test]
    fn l_goes_one_level_deeper_and_h_comes_back() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
        assert_eq!(state.focus, Focus::Messages, "Enter goes deeper, as l does");
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::OpenThread {
                item: 0,
                message: 1
            }
        );
        assert_eq!(
            state.key(press(KeyCode::Char('l'))),
            Action::OpenThread {
                item: 0,
                message: 1
            },
            "l on a message opens its thread"
        );
        state.open_thread(ThreadPane {
            channel: "D1".into(),
            thread_ts: "1.000200".into(),
            title: "@jane › no rush".into(),
            messages: vec![message("1.000200"), message("1.000300")],
            has_more: false,
            from_message: 1,
        });
        assert_eq!(
            state.key(press(KeyCode::Right)),
            Action::None,
            "nothing deeper than a thread"
        );
        assert!(state.thread.is_some());
        state.key(press(KeyCode::Char('h')));
        assert!(state.thread.is_none(), "h closes the thread");
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));
        state.key(press(KeyCode::Char('h')));
        assert_eq!(state.focus, Focus::List);
        state.key(press(KeyCode::Tab));
        assert_eq!(state.focus, Focus::List, "Tab is not a key of the screen");
    }

    #[test]
    fn capital_o_opens_slack_at_the_highlighted_message_from_the_pane() {
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('O'))),
            Action::Open {
                item: 0,
                message: None
            }
        );
        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Char('O'))),
            Action::Open {
                item: 0,
                message: Some(1)
            }
        );
    }

    #[test]
    fn e_marks_the_conversation_or_what_x_selected() {
        let mark = |item: usize| Mark {
            item,
            upto: Some("1.000200".to_string()),
        };
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('e'))),
            Action::MarkRead(vec![mark(0)])
        );
        state.key(press(KeyCode::Char('x')));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('x')));
        assert_eq!(state.checked.len(), 2);

        // A message that came in after x is not marked: it was never seen.
        let mut d1 = item(Section::DirectMessages, "D1", None);
        d1.latest = Some("1.000300".to_string());
        refresh(
            &mut state,
            vec![d1, item(Section::Threads, "C2", Some("2.000100"))],
        );
        assert_eq!(
            state.key(press(KeyCode::Char('e'))),
            Action::MarkRead(vec![mark(0), mark(1)])
        );

        assert_eq!(state.selected, Some(1));
        state.key(press(KeyCode::Char('x')));
        assert_eq!(state.checked.len(), 1, "x again unselects");
        state.key(press(KeyCode::Char('h')));
        state.key(press(KeyCode::Left));
        assert_eq!(state.checked.len(), 1, "h and Left leave it alone");
        state.key(press(KeyCode::Esc));
        assert!(state.checked.is_empty(), "Esc unselects everything");
        state.key(press(KeyCode::Char('x')));
        refresh(&mut state, vec![item(Section::DirectMessages, "D1", None)]);
        assert!(
            state.checked.is_empty(),
            "gone from the list, gone from the selection"
        );

        assert_eq!(state.key(control('r')), Action::Refresh);
        assert_eq!(state.key(press(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(state.key(control('c')), Action::Quit);
    }

    #[test]
    fn the_selection_follows_the_item_across_a_refresh_and_a_removal() {
        let mut state = state();
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('l')));
        refresh(
            &mut state,
            vec![
                item(Section::Mentions, "C1", None),
                item(Section::Threads, "C2", Some("2.000100")),
            ],
        );
        assert_eq!(state.selected, Some(0), "C1 is now first");
        assert_eq!(state.focus, Focus::Messages, "a refresh keeps the focus");
        state.remove(0);
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.items[0].channel, "C2");
        state.remove(0);
        assert_eq!(state.selected, None);
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
        assert_eq!(state.key(press(KeyCode::Char('r'))), Action::None);
        assert_eq!(state.key(press(KeyCode::Char('l'))), Action::None);
        assert_eq!(state.focus, Focus::List, "nothing to focus");
    }

    #[test]
    fn a_refresh_keeps_the_highlighted_message_and_the_open_thread() {
        let mut state = state();
        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(state.cursor, 1);
        // Two new messages, the oldest one gone from the page.
        let mut d1 = item(Section::DirectMessages, "D1", None);
        d1.messages = ["1.000200", "1.000300", "1.000400"].map(message).into();
        refresh(&mut state, vec![d1, item(Section::Mentions, "C1", None)]);
        assert_eq!(
            (state.focus, state.cursor),
            (Focus::Messages, 0),
            "same message"
        );

        state.open_thread(ThreadPane {
            channel: "D1".into(),
            thread_ts: "1.000300".into(),
            title: "@jane › no rush".into(),
            messages: vec![message("1.000300"), message("1.000310")],
            has_more: false,
            from_message: 1,
        });
        state.cursor = 1;
        let mut d1 = item(Section::DirectMessages, "D1", None);
        d1.messages = ["1.000300", "1.000400", "1.000500"].map(message).into();
        refresh(&mut state, vec![d1]);
        assert_eq!(state.thread.as_ref().map(|t| t.from_message), Some(0));
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));
        state.key(press(KeyCode::Char('h')));
        assert_eq!(state.cursor, 0, "back on the message it was opened from");

        // Read elsewhere: the neighbour, from the list.
        refresh(
            &mut state,
            vec![
                item(Section::Mentions, "C1", None),
                item(Section::Channels, "C3", None),
            ],
        );
        assert_eq!((state.selected, state.focus), (Some(0), Focus::List));
        state.key(press(KeyCode::Char('j')));
        refresh(&mut state, vec![item(Section::Mentions, "C1", None)]);
        assert_eq!(state.selected, Some(0));
    }

    #[test]
    fn the_picker_opens_in_its_filter_and_enter_opens_the_highlighted_match() {
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('o'))),
            Action::OpenPicker { compose: false }
        );
        let choices = vec![
            choice(ChoiceKind::Channel, "C5", "#dev-team", ""),
            choice(ChoiceKind::Channel, "C6", "#general", ""),
            choice(ChoiceKind::Dm, "D9", "@Jane", "jdoe Jane Doe SRE"),
            choice(ChoiceKind::Person, "U3", "@Bob", "bob Bob Devlin"),
        ];
        state.mode = Mode::Pick(Picker::new(choices.clone(), false));
        for c in "dev".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert!(picker.filtering, "the keys type straight away");
        assert_eq!(picker.matches(), [0, 3], "label or search text");
        assert_eq!(picker.selected, 0);
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Show {
                choice: choices[0].clone(),
                compose: false
            },
            "Enter opens the first match, no move needed"
        );
        assert_eq!(state.mode, Mode::Browse);

        state.mode = Mode::Pick(Picker::new(choices.clone(), false));
        for c in "dev".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Down));
        state.key(press(KeyCode::Down));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Show {
                choice: choices[3].clone(),
                compose: false
            },
            "Down moves in the filter and stops at the last match"
        );

        state.mode = Mode::Pick(Picker::new(choices.clone(), false));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "with nothing typed, Enter hands the keys to the list"
        );
        state.key(press(KeyCode::Char('j')));
        assert!(matches!(&state.mode, Mode::Pick(p) if !p.filtering && p.selected == 1));
        state.key(press(KeyCode::Char('/')));
        state.key(press(KeyCode::Char('j')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.filtering && p.query == "j" && p.selected == 0),
            "/ goes back to the filter, where j types"
        );
        state.key(press(KeyCode::Backspace));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Char('l'))),
            Action::Show {
                choice: choices[1].clone(),
                compose: false
            },
            "l opens from the list"
        );

        state.mode = Mode::Pick(Picker::new(choices.clone(), false));
        for c in "zzz".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "nothing matches, nothing opens"
        );
        assert!(
            matches!(&state.mode, Mode::Pick(p) if !p.filtering && p.query == "zzz"),
            "the keys go to the list, where m can show the muted ones"
        );
        state.key(press(KeyCode::Char('/')));
        state.key(press(KeyCode::Backspace));
        assert!(matches!(&state.mode, Mode::Pick(p) if p.query == "zz"));
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.query.is_empty() && p.filtering),
            "Esc clears what was typed"
        );
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert_eq!(
            state.mode,
            Mode::Browse,
            "with nothing typed, it closes the picker"
        );

        state.mode = Mode::Pick(Picker::new(choices, false));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('G')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.selected == 3),
            "G: the last"
        );
        state.key(press(KeyCode::Char('g')));
        state.key(press(KeyCode::Char('g')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.selected == 0),
            "gg: the first"
        );
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert_eq!(state.mode, Mode::Browse, "Esc closes from the list");

        state.mode = Mode::Pick(Picker::new(Vec::new(), false));
        assert_eq!(
            state.key(press(KeyCode::Char('q'))),
            Action::None,
            "q types in the filter"
        );
        state.key(press(KeyCode::Backspace));
        state.key(press(KeyCode::Enter));
        assert_eq!(
            state.key(press(KeyCode::Char('q'))),
            Action::Quit,
            "and quits from the list, as everywhere"
        );
    }

    #[test]
    fn muted_conversations_stay_out_of_the_picker_until_m() {
        let mut state = state();
        let mut choices = vec![
            choice(ChoiceKind::Channel, "C5", "#dev-team", ""),
            choice(ChoiceKind::Channel, "C7", "#noise", ""),
            choice(ChoiceKind::Dm, "D9", "@Jane", "jdoe Jane Doe SRE"),
        ];
        choices[1].muted = true;
        state.mode = Mode::Pick(Picker::new(choices, false));
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.matches(), [0, 2]);
        assert_eq!(picker.hidden_muted(), 1);
        state.key(press(KeyCode::Char('m')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.query == "m"),
            "in the filter, m types"
        );
        state.key(press(KeyCode::Backspace));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('G')));
        state.key(press(KeyCode::Char('m')));
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.matches(), [0, 1, 2], "m shows the muted ones");
        assert_eq!(picker.hidden_muted(), 0);
        state.key(press(KeyCode::Char('G')));
        state.key(press(KeyCode::Char('m')));
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.matches(), [0, 2], "m again hides them");
        assert_eq!(
            picker.selected, 1,
            "the selection stays on the shorter list"
        );

        state.key(press(KeyCode::Char('/')));
        for c in "noise".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('m')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.query == "noise" && p.matches() == [1]),
            "a muted name typed matches nothing: Enter to the list, then m"
        );
    }

    #[test]
    fn the_reply_box_completes_people_and_channels() {
        let mut state = state();
        state.key(press(KeyCode::Char('r')));
        for c in "hi ".chars() {
            assert_eq!(state.key(press(KeyCode::Char(c))), Action::None);
        }
        assert_eq!(
            state.key(press(KeyCode::Char('@'))),
            Action::LoadCandidates,
            "the first @ loads the names"
        );
        state.candidates = Some(vec![
            candidate("@fvallon", "Fabien Vallon", "fvallon fabien vallon"),
            candidate("@jdoe", "Jane Doe · SRE", "jdoe jane doe"),
            candidate("#dev-team", "", "dev-team dev team"),
        ]);
        let Mode::Compose(draft) = &state.mode else {
            panic!("still writing");
        };
        assert_eq!(
            state.completion(draft).map(|c| c.matches),
            Some(vec![0, 1]),
            "@ alone lists everyone"
        );
        state.key(press(KeyCode::Down));
        state.key(press(KeyCode::Down));
        let Mode::Compose(draft) = &state.mode else {
            panic!("still writing");
        };
        assert_eq!(
            state.completion(draft).map(|c| c.pick),
            Some(1),
            "Down stops at the last"
        );
        state.key(press(KeyCode::Char('v')));
        let Mode::Compose(draft) = &state.mode else {
            panic!("still writing");
        };
        assert_eq!(
            state.completion(draft).map(|c| (c.matches, c.pick)),
            Some((vec![0], 0)),
            "a typed letter narrows by handle or name and starts over"
        );
        assert_eq!(state.key(press(KeyCode::Tab)), Action::None);
        assert!(
            matches!(&state.mode, Mode::Compose(d) if d.text == "hi @fvallon "),
            "Tab takes the highlighted one, a space after it"
        );
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Send {
                item: 0,
                text: "hi @fvallon".to_string(),
                thread_ts: None
            },
            "with the list closed, Enter sends"
        );

        state.key(press(KeyCode::Char('r')));
        for c in "#te".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        let Mode::Compose(draft) = &state.mode else {
            panic!("still writing");
        };
        assert_eq!(
            state.completion(draft).map(|c| c.matches),
            Some(vec![2]),
            "# completes channels, on any part of the name"
        );
        state.key(press(KeyCode::Enter));
        assert!(
            matches!(&state.mode, Mode::Compose(d) if d.text == "#dev-team "),
            "Enter takes it too"
        );
        for c in "@j".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert!(
            matches!(&state.mode, Mode::Compose(d) if d.pick.is_none()),
            "Esc closes the list and keeps the text"
        );
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Send {
                item: 0,
                text: "#dev-team @j".to_string(),
                thread_ts: None
            }
        );
        state.key(press(KeyCode::Char('r')));
        for c in "@zz".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Send {
                item: 0,
                text: "@zz".to_string(),
                thread_ts: None
            },
            "no match, no list: Enter sends as typed"
        );
    }

    #[test]
    fn tab_marks_people_while_typing_and_enter_opens_the_group() {
        let mut state = state();
        let mut choices = vec![
            choice(ChoiceKind::Channel, "C5", "#dev-team", ""),
            choice(ChoiceKind::Dm, "D9", "@Jane", "jdoe Jane Doe SRE"),
            choice(ChoiceKind::Person, "U3", "@Bob", "bob Bob Devlin"),
        ];
        choices[1].user = Some("U9".to_string());
        choices[2].user = Some("U3".to_string());
        state.mode = Mode::Pick(Picker::new(choices.clone(), true));
        for c in "dev".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Tab));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.marked.is_empty() && p.query == "dev"),
            "a channel cannot be grouped, and what was typed stays"
        );
        state.key(press(KeyCode::Down));
        state.key(press(KeyCode::Tab));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.marked == [2] && p.query.is_empty() && p.filtering),
            "Tab marks Bob and clears the filter for the next name"
        );
        for c in "dev".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "a channel typed after the marks cannot join them"
        );
        state.key(press(KeyCode::Esc));
        for c in "jane".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U3".to_string(), "U9".to_string()],
                compose: true
            },
            "the person typed last joins the marks"
        );

        state.mode = Mode::Pick(Picker::new(choices.clone(), true));
        for c in "bob".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Tab));
        for c in "bob".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U3".to_string()],
                compose: true
            },
            "typed again, a marked person stays marked"
        );

        state.mode = Mode::Pick(Picker::new(choices.clone(), true));
        for c in "bob".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Tab));
        state.key(press(KeyCode::Char(' ')));
        state.key(press(KeyCode::Right));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.filtering),
            "Right opens nothing from the filter"
        );
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U3".to_string()],
                compose: true
            },
            "a stray blank matches everything but joins no one"
        );

        state.mode = Mode::Pick(Picker::new(choices.clone(), true));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Tab));
        state.key(press(KeyCode::Char('/')));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U9".to_string()],
                compose: true
            },
            "Tab marks from the list too, and Enter on an empty filter opens the group"
        );
    }

    #[test]
    fn c_picks_people_for_a_new_message_and_space_marks_several() {
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('c'))),
            Action::OpenPicker { compose: true }
        );
        let mut choices = vec![
            choice(ChoiceKind::Channel, "C5", "#dev-team", ""),
            choice(ChoiceKind::Dm, "D9", "@Jane", "jdoe Jane Doe SRE"),
            choice(ChoiceKind::Person, "U3", "@Bob", "bob Bob Devlin"),
        ];
        choices[1].user = Some("U9".to_string());
        choices[2].user = Some("U3".to_string());
        state.mode = Mode::Pick(Picker::new(choices.clone(), true));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char(' ')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.marked.is_empty()),
            "a channel cannot be grouped"
        );
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char(' ')));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char(' ')));
        assert!(matches!(&state.mode, Mode::Pick(p) if p.marked == [1, 2]));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U9".to_string(), "U3".to_string()],
                compose: true
            }
        );

        state.mode = Mode::Pick(Picker::new(choices.clone(), true));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char(' ')));
        state.key(press(KeyCode::Char(' ')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.marked.is_empty()),
            "Space again unmarks"
        );
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Show {
                choice: choices[2].clone(),
                compose: true
            }
        );
        // Once the conversation is in the list, the box opens on it.
        let page = history::Unread {
            messages: vec![message("5.000100")],
            has_more: false,
        };
        state.open_item(Item::opened("D3".into(), "@Bob".into(), page, None));
        state.compose_to_conversation();
        assert_eq!(state.mode, Mode::Compose(Draft::new(None)));
    }

    #[test]
    fn s_writes_to_the_conversation_from_anywhere() {
        let mut state = state();
        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('s')));
        assert_eq!(
            state.mode,
            Mode::Compose(Draft::new(None)),
            "not the highlighted message's thread"
        );
        state.key(press(KeyCode::Esc));
        state.open_thread(ThreadPane {
            channel: "D1".into(),
            thread_ts: "1.000200".into(),
            title: "@jane › no rush".into(),
            messages: vec![message("1.000200")],
            has_more: false,
            from_message: 1,
        });
        state.key(press(KeyCode::Char('s')));
        assert_eq!(
            state.mode,
            Mode::Compose(Draft::new(None)),
            "not the open thread either"
        );
    }

    #[test]
    fn an_opened_conversation_is_selected_apart_from_its_unread_row() {
        let mut state = state();
        let page = history::Unread {
            messages: vec![message("3.000100")],
            has_more: false,
        };
        // C1 is listed unread, and opened from the picker as well.
        state.open_item(Item::opened("C1".into(), "#ops".into(), page, None));
        assert_eq!((state.selected, state.focus), (Some(3), Focus::Messages));
        let Action::MarkRead(marks) = state.key(press(KeyCode::Char('e'))) else {
            panic!("e marks");
        };
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].item, 3, "mod.rs closes it without a mark");

        state.key(press(KeyCode::Char('x')));
        assert!(state.is_checked(&state.items[3]));
        assert!(
            !state.is_checked(&state.items[1]),
            "the unread row is apart"
        );
        state.remove(1);
        assert!(
            state.is_checked(&state.items[2]),
            "and leaving takes only its own tick"
        );
    }

    #[test]
    fn another_name_of_your_reaction_takes_it_back() {
        let mut state = state();
        state.me = Some("U7".into());
        state.frequent_emoji = vec!["thumbsup".into()];
        state.items[0].messages[0].reactions = vec![history::Reaction {
            name: "+1".into(),
            count: 1,
            users: vec!["U7".into()],
        }];
        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('R')));
        assert_eq!(state.emoji_choices(""), ["+1"], "one emoji, one row");
        for c in "thumbsu".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        let ts = state.items[0].messages[0].ts.clone();
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::React {
                channel: "D1".into(),
                ts: ts.clone(),
                name: "+1".into(),
                add: false,
            },
            "under the name the message has it"
        );
        state.apply_reaction("D1", &ts, "U7", "thumbsup", false);
        assert!(state.items[0].messages[0].reactions.is_empty());
    }

    #[test]
    fn plus_puts_an_emoji_on_the_highlighted_message_or_takes_yours_back() {
        let mut state = state();
        state.me = Some("U7".into());
        state.frequent_emoji = vec!["+1".into(), "joy".into(), "yolo".into()];
        state.items[0].messages[0].reactions = vec![history::Reaction {
            name: "eyes".into(),
            count: 2,
            users: vec!["U2".into(), "U7".into()],
        }];
        assert_eq!(state.key(press(KeyCode::Char('R'))), Action::None);
        assert_eq!(
            state.status.as_deref(),
            Some("Move onto a message first (l)")
        );

        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('R')));
        assert!(matches!(state.mode, Mode::React(_)));
        assert_eq!(
            state.emoji_choices(""),
            ["eyes", "+1", "joy", "yolo"],
            "the message's own first, then the most used"
        );
        let ts = state.items[0].messages[0].ts.clone();
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::React {
                channel: "D1".into(),
                ts: ts.clone(),
                name: "eyes".into(),
                add: false,
            },
            "Enter on yours takes it back"
        );

        state.key(press(KeyCode::Char('R')));
        for c in "che".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert!(
            state
                .emoji_choices("che")
                .contains(&"white_check_mark".to_string())
        );
        state.key(press(KeyCode::Backspace));
        state.key(press(KeyCode::Char('q')));
        assert!(
            matches!(&state.mode, Mode::React(r) if r.query == "chq"),
            "q types"
        );
        assert_eq!(
            state.emoji_choices("chq"),
            ["chq"],
            "no match is sent as typed"
        );
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert!(matches!(state.mode, Mode::Browse));

        let frequent = std::mem::take(&mut state.frequent_emoji);
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('R')));
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
        assert!(
            matches!(state.mode, Mode::React(_)),
            "nothing to pick keeps the prompt"
        );
        state.key(press(KeyCode::Esc));
        state.key(press(KeyCode::Char('k')));
        state.frequent_emoji = frequent;

        state.key(press(KeyCode::Char('R')));
        state.key(press(KeyCode::Down));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::React {
                channel: "D1".into(),
                ts: ts.clone(),
                name: "+1".into(),
                add: true,
            }
        );
        state.apply_reaction("D1", &ts, "U7", "+1", true);
        state.apply_reaction("D1", &ts, "U7", "eyes", false);
        let reactions = &state.items[0].messages[0].reactions;
        assert_eq!(reactions.len(), 2);
        assert_eq!(
            (reactions[0].name.as_str(), reactions[0].count),
            ("eyes", 1)
        );
        assert_eq!(reactions[1].users, ["U7"]);
    }

    #[test]
    fn u_opens_a_link_of_the_highlighted_message_and_y_copies_it() {
        let mut state = state();
        state.items[0].messages[0].text = "one <https://a.b/1|doc>".into();
        state.items[0].messages[1].text = "two <https://a.b/2> and <https://a.b/3|three>".into();
        assert_eq!(state.key(press(KeyCode::Char('u'))), Action::None);
        assert_eq!(
            state.status.as_deref(),
            Some("Move onto a message first (l)")
        );

        state.key(press(KeyCode::Char('l')));
        assert_eq!(
            state.key(press(KeyCode::Char('u'))),
            Action::OpenLink("https://a.b/1".into()),
            "one link goes straight away"
        );
        assert_eq!(
            state.key(press(KeyCode::Char('y'))),
            Action::CopyLink("https://a.b/1".into())
        );

        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Char('u'))),
            Action::None,
            "two links ask"
        );
        assert!(matches!(&state.mode, Mode::Links { links, copy: false } if links.len() == 2));
        assert_eq!(
            state.key(press(KeyCode::Char('2'))),
            Action::OpenLink("https://a.b/3".into())
        );
        assert_eq!(state.mode, Mode::Browse);
        state.key(press(KeyCode::Char('y')));
        assert!(matches!(&state.mode, Mode::Links { copy: true, .. }));
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None, "Esc gives up");
        assert_eq!(state.mode, Mode::Browse);
        state.key(press(KeyCode::Char('u')));
        assert_eq!(
            state.key(press(KeyCode::Char('7'))),
            Action::None,
            "no seventh link"
        );

        state.items[0].messages[1].text = "plain".into();
        state.key(press(KeyCode::Char('u')));
        assert_eq!(state.status.as_deref(), Some("No link in this message"));
    }

    #[test]
    fn a_history_is_read_from_its_first_new_message() {
        let entry = Entry {
            id: "C1".into(),
            kind: Kind::Channel,
            name: "#dev".into(),
            mention_count: 1,
            latest: Some("1.000300".into()),
            messages: Some(vec![
                message("1.000100"),
                message("1.000200"),
                message("1.000300"),
            ]),
            has_more: true,
            unread_from: Some("1.000150".into()),
        };
        let item = Item::conversation(Section::Mentions, entry);
        assert_eq!(
            item.detail, "2 unread, 1 mention",
            "the read part is not counted, and the page reaches past the cursor"
        );
        assert_eq!(item.landing(), 1);

        let mut state = State::new(vec![item], None);
        state.key(press(KeyCode::Char('l')));
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));
        state.key(press(KeyCode::Char('h')));
        // From the list the pane starts at the new part; Ctrl-U shows
        // what is above it, down to the top of the pane and no further.
        state.viewport = Viewport {
            anchor: 15,
            height: 20,
            rows: 60,
            in_view: 5,
        };
        state.key(control('u'));
        state.key(control('u'));
        assert_eq!(state.scroll, -15);
        state.key(control('d'));
        assert_eq!(state.scroll, -5);
    }

    #[test]
    fn an_opened_conversation_is_read_from_its_new_part_and_survives_a_refresh() {
        let mut state = state();
        let page = history::Unread {
            messages: vec![
                message("3.000100"),
                message("3.000200"),
                message("3.000300"),
            ],
            has_more: true,
        };
        let opened = Item::opened("C7".into(), "#ops".into(), page, Some("3.000150".into()));
        assert_eq!(opened.detail, "last 3 messages, 2 new");
        state.open_item(opened);
        assert_eq!(state.selected, Some(3));
        assert_eq!(
            (state.focus, state.cursor),
            (Focus::Messages, 1),
            "on the first new message"
        );

        let again = Item::opened(
            "C7".into(),
            "#ops".into(),
            history::Unread {
                messages: vec![message("3.000400")],
                has_more: false,
            },
            None,
        );
        state.open_item(again);
        assert_eq!(state.items.len(), 4, "the same conversation is replaced");
        assert_eq!(state.items[3].detail, "1 message");
        assert_eq!(state.cursor, 0, "nothing new: on the newest message");

        refresh(&mut state, vec![item(Section::Mentions, "C1", None)]);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[1].section, Section::Opened);
        assert_eq!(state.selected, Some(1), "still on the opened one");
    }

    #[test]
    fn control_keys_do_not_type_into_the_reply_box() {
        let mut state = state();
        state.key(press(KeyCode::Char('r')));
        state.key(press(KeyCode::Char('h')));
        state.key(control('d'));
        state.key(press(KeyCode::Char('i')));
        assert!(matches!(&state.mode, Mode::Compose(draft) if draft.text == "hi"));
    }

    #[test]
    fn deleting_keys_take_a_character_a_word_or_the_line_back() {
        let text = |state: &State| match &state.mode {
            Mode::Compose(draft) => draft.text.clone(),
            other => panic!("still writing, not {other:?}"),
        };
        let mut state = state();
        state.key(press(KeyCode::Char('r')));
        for c in "one two".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(control('j'));
        for c in "three four".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(text(&state), "one two\nthree ", "Option-Backspace: a word");
        state.key(control('w'));
        assert_eq!(
            text(&state),
            "one two\n",
            "Ctrl-W: the same, blanks included"
        );
        state.key(control('w'));
        assert_eq!(text(&state), "one two", "a line break goes alone");
        state.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(text(&state), "", "Command-Backspace: the line");
        state.key(press(KeyCode::Char('a')));
        state.key(control('j'));
        for c in "bc".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(control('u'));
        assert_eq!(text(&state), "a\n", "Ctrl-U: the line, not the one above");
        state.key(press(KeyCode::Backspace));
        assert_eq!(text(&state), "a");
        state.key(control('h'));
        assert_eq!(text(&state), "", "Ctrl-H is Backspace");
    }

    #[test]
    fn a_word_cut_takes_the_blanks_after_it() {
        let cut = |text: &str, cut: Cut| {
            let mut text = text.to_string();
            cut.apply(&mut text);
            text
        };
        assert_eq!(cut("hello world  ", Cut::Word), "hello ");
        assert_eq!(cut("hello", Cut::Word), "");
        assert_eq!(cut("one\n  ", Cut::Word), "one\n");
        assert_eq!(cut("one\ntwo", Cut::Line), "one\n");
        assert_eq!(cut("", Cut::Char), "");
        assert_eq!(cut("é", Cut::Char), "");
    }

    #[test]
    fn a_paste_lands_in_the_reply_box_with_its_line_breaks() {
        let mut state = state();
        assert_eq!(state.paste("stray"), Action::None);
        assert!(matches!(state.mode, Mode::Browse));
        state.key(press(KeyCode::Char('r')));
        state.paste("Thanks!\r\nWill look tomorrow");
        assert!(matches!(
            &state.mode,
            Mode::Compose(draft) if draft.text == "Thanks!\nWill look tomorrow"
        ));
        let sent = state.key(press(KeyCode::Enter));
        assert!(matches!(
            &sent,
            Action::Send { text, .. } if text == "Thanks!\nWill look tomorrow"
        ));
    }

    #[test]
    fn a_line_break_is_ctrl_j_or_a_modified_enter_and_enter_still_sends() {
        let mut state = state();
        state.key(press(KeyCode::Char('r')));
        for c in "one".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(control('j'));
        state.key(press(KeyCode::Char('2')));
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        state.key(press(KeyCode::Char('3')));
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(matches!(&state.mode, Mode::Compose(draft) if draft.text == "one\n2\n3\n"));
        state.key(press(KeyCode::Backspace));
        assert!(
            matches!(&state.mode, Mode::Compose(draft) if draft.text == "one\n2\n3"),
            "Backspace takes a break back like any character"
        );
        assert!(matches!(
            state.key(press(KeyCode::Enter)),
            Action::Send { text, .. } if text == "one\n2\n3"
        ));
    }

    #[test]
    fn a_line_break_closes_the_completion_list_instead_of_picking() {
        let mut state = state();
        state.candidates = Some(vec![candidate("@jdoe", "Jane Doe", "jdoe jane doe")]);
        state.key(press(KeyCode::Char('r')));
        for c in "cc @j".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        let Mode::Compose(draft) = &state.mode else {
            panic!("still writing");
        };
        assert!(state.completion(draft).is_some(), "the list is open");
        state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        let Mode::Compose(draft) = &state.mode else {
            panic!("still writing");
        };
        assert_eq!(draft.text, "cc @j\n", "the word stays as typed");
        assert!(state.completion(draft).is_none(), "and the list is gone");
    }

    #[test]
    fn paging_scrolls_the_pane_from_the_list_and_moves_the_cursor_in_it() {
        let mut state = state();
        state.items[0].messages = (1..=12).map(|n| message(&format!("1.{n:06}"))).collect();
        state.viewport = Viewport {
            anchor: 30,
            height: 20,
            rows: 100,
            in_view: 6,
        };
        state.key(control('d'));
        assert_eq!(state.scroll, 10, "half a screen");
        state.key(control('f'));
        assert_eq!(state.scroll, 30, "a whole one");
        state.key(press(KeyCode::PageDown));
        state.key(press(KeyCode::PageDown));
        assert_eq!(
            state.scroll, 50,
            "no further than the last screen of the text"
        );
        for _ in 0..9 {
            state.key(control('b'));
        }
        assert_eq!(state.scroll, -30, "no higher than the top of the pane");
        state.key(press(KeyCode::Char('G')));
        assert_eq!(state.selected, Some(2), "G: the last item");
        state.key(press(KeyCode::Char('g')));
        state.key(press(KeyCode::Char('g')));
        assert_eq!(state.selected, Some(0), "gg: the first");
        state.key(press(KeyCode::Char('g')));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.selected,
            Some(1),
            "a lone g is forgotten by the next key"
        );

        state.key(press(KeyCode::Char('k')));
        state.key(press(KeyCode::Char('l')));
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 0));
        state.key(control('d'));
        assert_eq!(state.cursor, 3, "half of what the screen shows");
        state.key(control('f'));
        assert_eq!(state.cursor, 9, "all of it");
        state.key(press(KeyCode::Char('G')));
        assert_eq!(state.cursor, 11);
        state.key(press(KeyCode::Char('g')));
        state.key(press(KeyCode::Char('g')));
        assert_eq!(state.cursor, 0);
        state.key(press(KeyCode::End));
        state.key(control('u'));
        assert_eq!(state.cursor, 8);
        assert_eq!(
            state.scroll, 0,
            "the pane follows the highlighted message instead"
        );
    }

    #[test]
    fn marks_survive_the_filter_and_beat_the_highlighted_choice() {
        let mut state = state();
        let mut choices = vec![
            choice(ChoiceKind::Channel, "C5", "#dev-team", ""),
            choice(ChoiceKind::Dm, "D9", "@Jane", "jdoe Jane Doe SRE"),
            choice(ChoiceKind::Person, "U3", "@Bob", "bob Bob Devlin"),
        ];
        choices[1].user = Some("U9".to_string());
        choices[2].user = Some("U3".to_string());
        state.mode = Mode::Pick(Picker::new(choices, true));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char(' ')));
        state.key(press(KeyCode::Char('/')));
        for c in "bob".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Tab));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.marked == [1, 2]),
            "a mark is an index into the choices, not into the matches"
        );
        for c in "de".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Esc));
        assert!(
            matches!(
                &state.mode,
                Mode::Pick(p) if p.query.is_empty() && p.selected == 0 && p.marked == [1, 2]
            ),
            "Esc clears the filter and keeps the marks"
        );
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U9".to_string(), "U3".to_string()],
                compose: true
            },
            "the highlighted channel yields to the marks"
        );
    }

    fn visited(id: &str, kind: Kind, latest: &str) -> Item {
        let conversation = Conversation {
            id: id.to_string(),
            kind,
            has_unreads: false,
            mention_count: 0,
            last_read: Some(latest.to_string()),
            latest: Some(latest.to_string()),
            muted: false,
        };
        Item::recent(&conversation, id.to_string())
    }

    fn page(ts: &[&str]) -> history::Unread {
        history::Unread {
            messages: ts.iter().map(|ts| message(ts)).collect(),
            has_more: false,
        }
    }

    fn sections(state: &State) -> Vec<(Section, &str)> {
        state
            .items
            .iter()
            .map(|item| (item.section, item.channel.as_str()))
            .collect()
    }

    #[test]
    fn the_recent_conversations_follow_what_is_listed_above_them() {
        let mut state = State::new(Vec::new(), None);
        assert!(state.selected.is_none());
        let recent = || {
            let mut recent = vec![
                visited("C1", Kind::Channel, "1.1"),
                visited("D9", Kind::Im, "9.1"),
                visited("C7", Kind::Channel, "7.1"),
            ];
            recent.extend((10..20).map(|n| visited(&format!("C{n}"), Kind::Channel, "1.1")));
            recent
        };
        state.replace_items(Some(vec![item(Section::Mentions, "C1", None)]), recent());
        assert_eq!(state.selected, Some(0));
        state.open_item(Item::opened(
            "C7".to_string(),
            "#c7".to_string(),
            page(&["7.1"]),
            None,
        ));
        state.replace_items(None, recent());
        let listed = sections(&state);
        assert_eq!(
            &listed[..4],
            [
                (Section::Mentions, "C1"),
                (Section::Opened, "C7"),
                (Section::RecentChannels, "C10"),
                (Section::RecentChannels, "C11"),
            ],
            "C1 is listed for its mention and C7 was opened: neither shows twice"
        );
        let channels = listed
            .iter()
            .filter(|(section, _)| *section == Section::RecentChannels)
            .count();
        assert_eq!(channels, RECENT_ROWS);
        assert_eq!(listed.last(), Some(&(Section::RecentPeople, "D9")));
    }

    #[test]
    fn a_recent_history_is_read_once_and_kept_while_nothing_came_in() {
        let mut state = State::new(Vec::new(), None);
        state.replace_items(Some(Vec::new()), vec![visited("C1", Kind::Channel, "1.2")]);
        assert_eq!(state.to_preview(), Some(0));
        assert_eq!(state.pane_messages().len(), 0);
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Preview {
                item: 0,
                enter: true
            },
            "Enter does not wait for the cursor to rest"
        );
        state.preview(0, page(&["1.1", "1.2"]), true);
        assert_eq!(state.to_preview(), None);
        assert_eq!(state.focus, Focus::Messages);
        assert_eq!(state.cursor, 1, "read from its newest message");

        state.replace_items(None, vec![visited("C1", Kind::Channel, "1.2")]);
        assert_eq!(state.to_preview(), None, "nothing came in: kept as read");
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));

        state.replace_items(None, vec![visited("C1", Kind::Channel, "1.3")]);
        assert_eq!(state.to_preview(), Some(0), "a message came in: read again");
        assert_eq!(
            state.pane_messages().len(),
            2,
            "the old history shows meanwhile"
        );
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));
        state.preview(0, page(&["1.1", "1.2", "1.3"]), false);
        assert_eq!(state.cursor, 1, "the highlighted message stays");

        let edit = Event::Message(Posted {
            channel: "C1".to_string(),
            ts: "1.2".to_string(),
            thread_ts: None,
            user: Some("U2".to_string()),
            changed: true,
        });
        assert_eq!(
            state.hear(edit, Instant::now()),
            Wake::Nothing,
            "no reload, which would not read it again"
        );
        assert_eq!(state.to_preview(), Some(0), "read again instead");
    }

    #[test]
    fn tab_moves_from_section_to_section() {
        let mut state = State::new(Vec::new(), None);
        state.replace_items(
            Some(vec![
                item(Section::DirectMessages, "D1", None),
                item(Section::DirectMessages, "D2", None),
                item(Section::Mentions, "C1", None),
            ]),
            vec![
                visited("C5", Kind::Channel, "5.1"),
                visited("C6", Kind::Channel, "6.1"),
                visited("D5", Kind::Im, "5.1"),
            ],
        );
        let channel = |state: &State| state.selected_item().unwrap().channel.clone();
        state.key(press(KeyCode::Char('j')));
        assert_eq!(channel(&state), "D2");
        for expected in ["C1", "C5", "D5", "D5"] {
            state.key(press(KeyCode::Tab));
            assert_eq!(channel(&state), expected);
        }
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::BackTab));
        assert_eq!(
            channel(&state),
            "C5",
            "back from the pane, to the section above"
        );
        assert_eq!(state.focus, Focus::List);
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::BackTab));
        assert_eq!(
            channel(&state),
            "C1",
            "from within a section, the one above"
        );
        state.key(press(KeyCode::BackTab));
        state.key(press(KeyCode::BackTab));
        assert_eq!(channel(&state), "D1");
    }

    #[test]
    fn a_history_lands_on_its_first_new_message_else_its_newest() {
        let mut it = item(Section::Channels, "C1", None);
        it.messages = vec![
            message("1.000100"),
            message("1.000200"),
            message("1.000300"),
        ];
        it.unread_from = Some("1.000150".to_string());
        assert_eq!(it.landing(), 1);
        it.unread_from = Some("1.000900".to_string());
        assert_eq!(
            it.landing(),
            2,
            "a cursor past everything lands on the newest"
        );
        it.unread_from = None;
        assert_eq!(it.landing(), 0, "the unread part is read from its start");
        it.section = Section::Opened;
        assert_eq!(
            it.landing(),
            2,
            "an opened conversation with no cursor, from its end"
        );
    }

    fn posted(channel: &str, ts: &str, thread_ts: Option<&str>, user: &str) -> Event {
        Event::Message(Posted {
            channel: channel.to_string(),
            ts: ts.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            user: Some(user.to_string()),
            changed: false,
        })
    }

    impl State {
        fn hear_now(&mut self, event: Event) -> Wake {
            self.hear(event, Instant::now())
        }
    }

    #[test]
    fn the_socket_reloads_only_for_what_the_counts_would_miss() {
        let mut state = state();
        state.me = Some("U7".into());

        assert_eq!(state.hear_now(posted("C9", "5.0", None, "U1")), Wake::Check);
        assert_eq!(state.hear_now(Event::Moved), Wake::Check);
        assert_eq!(
            state.hear_now(posted("C1", "5.0", None, "U7")),
            Wake::Nothing,
            "yours: the cursor it moves comes on its own"
        );

        assert_eq!(
            state.hear_now(posted("C2", "5.0", Some("2.000100"), "U1")),
            Wake::Reload,
            "a reply in a listed thread"
        );
        assert_eq!(
            state.hear_now(posted("C2", "5.0", Some("2.000100"), "U7")),
            Wake::Reload,
            "yours from another client, missing from the thread"
        );
        assert_eq!(
            state.hear_now(posted("C2", "1.000200", Some("2.000100"), "U7")),
            Wake::Nothing,
            "yours, already shown"
        );
        state.thread = Some(ThreadPane {
            channel: "C1".into(),
            thread_ts: "3.0".into(),
            title: String::new(),
            messages: vec![message("3.0")],
            has_more: false,
            from_message: 0,
        });
        assert_eq!(
            state.hear_now(posted("C1", "5.0", Some("3.0"), "U1")),
            Wake::Reload,
            "a reply in the open thread"
        );

        assert_eq!(
            state.hear_now(posted("D1", "5.0", Some("1.000100"), "U1")),
            Wake::Check,
            "a reply elsewhere"
        );
        assert_eq!(
            state.items[0].messages[0].reply_count, 1,
            "its root counts it without a call"
        );

        let Event::Message(mut edit) = posted("D1", "1.000100", None, "U1") else {
            unreachable!()
        };
        edit.changed = true;
        assert_eq!(
            state.hear_now(Event::Message(edit)),
            Wake::Reload,
            "an edit on the screen"
        );
        let Event::Message(mut edit) = posted("C9", "1.0", None, "U1") else {
            unreachable!()
        };
        edit.changed = true;
        assert_eq!(
            state.hear_now(Event::Message(edit)),
            Wake::Nothing,
            "an edit elsewhere"
        );
        let Event::Message(mut edit) = posted("D1", "0.5", None, "U1") else {
            unreachable!()
        };
        edit.changed = true;
        assert_eq!(
            state.hear_now(Event::Message(edit)),
            Wake::Nothing,
            "an edit to a message the screen does not show"
        );

        let reaction = |user: &str, add: bool| Event::Reaction {
            channel: "D1".into(),
            ts: "1.000200".into(),
            user: user.into(),
            name: "eyes".into(),
            add,
        };
        assert_eq!(state.hear_now(reaction("U1", true)), Wake::Nothing);
        assert_eq!(state.hear_now(reaction("U7", true)), Wake::Nothing);
        assert_eq!(
            state.hear_now(reaction("U7", true)),
            Wake::Nothing,
            "yours again, as the socket echoes what `R` did"
        );
        let reactions = &state.items[0].messages[1].reactions;
        assert_eq!((reactions[0].count, reactions[0].users.len()), (2, 2));
        state.hear_now(reaction("U1", false));
        assert_eq!(state.items[0].messages[1].reactions[0].users, ["U7"]);
    }

    #[test]
    fn typing_shows_until_the_message_or_until_it_stops() {
        let mut state = state();
        state.me = Some("U7".into());
        let typing = |channel: &str, thread_ts: Option<&str>, user: &str| Event::Typing {
            channel: channel.into(),
            thread_ts: thread_ts.map(str::to_string),
            user: user.into(),
        };
        let start = Instant::now();
        state.hear(typing("D1", None, "U1"), start);
        state.hear(typing("D1", None, "U7"), start);
        state.hear(typing("C2", Some("2.000100"), "U2"), start);
        assert_eq!(state.typists("D1", None), ["U1"], "never you");
        assert_eq!(state.typists("C2", Some("2.000100")), ["U2"]);
        assert!(
            state.typists("C2", None).is_empty(),
            "the thread, not its channel"
        );

        state.hear(posted("D1", "5.0", None, "U1"), start);
        assert!(state.typists("D1", None).is_empty(), "sent");

        state.forget_typing(start + TYPING_FOR / 2);
        assert_eq!(state.typists("C2", Some("2.000100")), ["U2"]);
        state.forget_typing(start + TYPING_FOR);
        assert!(state.typing.is_empty(), "not repeated");
    }

    fn hit(channel: &str, ts: &str, thread_ts: Option<&str>) -> Hit {
        let mut message = message(ts);
        message.thread_ts = thread_ts.map(str::to_string);
        Hit {
            channel: channel.to_string(),
            message,
            permalink: format!(
                "https://acme.slack.com/archives/{channel}/p{}",
                ts.replace('.', "")
            ),
        }
    }

    fn typed(state: &mut State, text: &str) {
        for c in text.chars() {
            state.key(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn slash_searches_and_enter_opens_a_hit_keeping_the_search_for_later() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('/'))), Action::None);
        assert!(matches!(&state.mode, Mode::Search(s) if s.typing && s.query.is_empty()));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "nothing to search"
        );
        typed(&mut state, "deploy in:#dev");
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Search {
                query: "deploy in:#dev".to_string()
            }
        );
        let hits = vec![
            hit("C5", "3.000100", None),
            hit("C6", "2.000200", Some("2.000100")),
        ];
        state.found(
            "deploy in:#dev".to_string(),
            Found {
                total: 40,
                hits: hits.clone(),
            },
        );
        assert!(matches!(&state.mode, Mode::Search(s) if !s.typing && s.total == 40));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Char('y'))),
            Action::CopyLink(hits[1].permalink.clone())
        );
        assert!(
            matches!(&state.mode, Mode::Search(_)),
            "copying keeps it open"
        );
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowHit(hits[1].clone())
        );
        assert_eq!(state.mode, Mode::Browse);

        // `/` brings it back in its query line; unchanged, Enter goes to
        // the hits without asking Slack again.
        state.key(press(KeyCode::Char('/')));
        assert!(matches!(&state.mode, Mode::Search(s) if s.typing && s.selected == 1));
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
        assert!(matches!(&state.mode, Mode::Search(s) if !s.typing));
        state.key(press(KeyCode::Char('/')));
        state.key(control('u'));
        typed(&mut state, "other");
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Search {
                query: "other".to_string()
            }
        );
        state.found(
            "other".to_string(),
            Found {
                total: 0,
                hits: Vec::new(),
            },
        );
        assert!(
            matches!(&state.mode, Mode::Search(s) if s.typing),
            "no match leaves the keys in the query line"
        );
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert_eq!(state.mode, Mode::Browse);
        assert!(
            state
                .last_search
                .as_ref()
                .is_some_and(|s| s.query == "other")
        );
    }

    #[test]
    fn a_hit_opens_around_itself_with_the_cursor_on_it() {
        let mut state = state();
        let page = history::Unread {
            messages: vec![
                message("1.000100"),
                message("1.000200"),
                message("1.000300"),
            ],
            has_more: true,
        };
        state.open_item(Item::around("C9".to_string(), "#dev".to_string(), page));
        state.highlight("1.000200");
        let item = state.selected_item().unwrap();
        assert_eq!(
            (item.section, item.detail.as_str()),
            (Section::Opened, "3 messages around the match")
        );
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 1));
        state.highlight("9.000000");
        assert_eq!(state.cursor, 1, "a message not shown moves nothing");
    }
}
