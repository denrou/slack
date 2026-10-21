//! The TUI's state and its pure transitions: what the list holds, what is
//! selected, which pane has the focus, which mode the keys go to. No I/O,
//! so the keys are testable; `mod.rs` performs the actions.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::history::{self, Message};
use crate::mrkdwn::{self, Link};
use crate::names::Names;
use crate::style::Style;
use crate::view::{self, Entry, View};
use crate::{directory, threads};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    DirectMessages,
    Mentions,
    /// The other channels with unreads, mentions aside.
    Channels,
    Threads,
    /// Conversations opened from the picker (`c`), with their recent
    /// history. They stay through a refresh and a reply.
    Opened,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::DirectMessages => "Direct messages",
            Section::Mentions => "Mentions",
            Section::Channels => "Channels",
            Section::Threads => "Threads",
            Section::Opened => "Opened",
        }
    }
}

/// One line of the list: a conversation or a thread, with its messages.
pub struct Item {
    pub section: Section,
    pub channel: String,
    /// Set for a thread: replies go there, and `o` opens it.
    pub thread_ts: Option<String>,
    pub title: String,
    pub detail: String,
    pub messages: Vec<Message>,
    pub has_more: bool,
    /// Where to move the read cursor.
    pub latest: Option<String>,
    /// The read cursor of a history: the pane marks where "new" starts.
    pub unread_from: Option<String>,
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
        let (unread, more) = match &unread_from {
            Some(cursor) => {
                let new = messages
                    .iter()
                    .filter(|message| view::is_new(message, Some(cursor)))
                    .count();
                (new, entry.has_more && new == messages.len())
            }
            None => (messages.len(), entry.has_more),
        };
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
        }
    }

    fn thread(thread: threads::Thread, names: &Names) -> Item {
        let user = |id: &str| names.user_name(id);
        let channel = |id: &str| names.channel_name(id);
        let root = mrkdwn::render(&thread.root.text, &user, &channel, &Style::plain());
        let detail = match thread.unread_replies.len() {
            1 => "1 new reply".to_string(),
            n => format!("{n}{} new replies", if thread.has_more { "+" } else { "" }),
        };
        Item {
            section: Section::Threads,
            title: format!(
                "{} › {}",
                names.channel_label(&thread.channel),
                view::excerpt(&root, 40)
            ),
            detail,
            latest: thread.unread_replies.last().map(|reply| reply.ts.clone()),
            channel: thread.channel,
            thread_ts: Some(thread.thread_ts),
            messages: thread.unread_replies,
            has_more: thread.has_more,
            unread_from: None,
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
            messages: page.messages,
            has_more: page.has_more,
            unread_from,
        };
        item.describe();
        item
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
            None if self.section == Section::Opened => last,
            None => 0,
        }
    }

    /// Nothing is new any more: after a mark or a reply.
    pub fn caught_up(&mut self) {
        self.unread_from = None;
        self.describe();
    }

    fn describe(&mut self) {
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
    /// Write to the chosen conversation straight away (`n`), rather
    /// than only open it (`c`).
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

    /// Space: mark the highlighted person for a group message, or unmark
    /// them. A channel or a group cannot be put in one.
    fn mark(&mut self) {
        let Some(&index) = self.matches().get(self.selected) else {
            return;
        };
        if self.choices[index].user.is_none() {
            return;
        }
        match self.marked.iter().position(|&marked| marked == index) {
            Some(at) => {
                self.marked.remove(at);
            }
            None => self.marked.push(index),
        }
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
    ConfirmReadAll,
    Help,
    Pick(Picker),
    /// Which link of the highlighted message: a digit picks one, to
    /// open it or to copy it.
    Links {
        links: Vec<Link>,
        copy: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    MarkRead(usize),
    MarkAllRead,
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
}

/// The messages pane as drawn, for the keys that move by screens: only
/// the drawing knows how the text wraps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    /// Where the pane starts on its own in list focus, in rows: the new
    /// part of a history.
    pub anchor: u16,
    /// Rows inside the border.
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
    pub focus: Focus,
    /// The highlighted message of the pane, when the pane has the focus.
    pub cursor: usize,
    pub thread: Option<ThreadPane>,
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
    /// opening of the same one, and read it from its newest message.
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
                self.items.push(item);
                self.items.len() - 1
            }
        };
        self.selected = Some(index);
        self.thread = None;
        self.scroll = 0;
        self.read_from_landing();
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

    /// New items after a refresh; the opened conversations stay, and the
    /// selection follows the same conversation when it is still there.
    pub fn replace_items(&mut self, items: Vec<Item>) {
        let key = self
            .selected_item()
            .map(|item| (item.channel.clone(), item.thread_ts.clone()));
        let opened = std::mem::take(&mut self.items)
            .into_iter()
            .filter(|item| item.section == Section::Opened);
        self.items = items;
        self.items.extend(opened);
        self.selected = key
            .and_then(|(channel, thread_ts)| {
                self.items
                    .iter()
                    .position(|item| item.key() == (channel.as_str(), thread_ts.as_deref()))
            })
            .or_else(|| (!self.items.is_empty()).then_some(0));
        self.back_to_list();
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
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
            Mode::ConfirmReadAll => match key.code {
                KeyCode::Char('y' | 'Y') => Action::MarkAllRead,
                _ => Action::None,
            },
            Mode::Help => Action::None,
            Mode::Pick(picker) => self.pick(picker, key),
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
            other => self.mode = other,
        }
        Action::None
    }

    /// `q` and Ctrl-C quit from anywhere; Esc and `h` step back; `gg`,
    /// `G` and the Ctrl keys of vim move by screens.
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
            // Esc backs out of whatever is open, as everywhere else on
            // the screen; at the list there is nothing to back out of.
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
                self.back();
                Action::None
            }
            KeyCode::Char('l') | KeyCode::Right => self.deeper(),
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
                self.close_opened();
                Action::None
            }
            KeyCode::Char('u') => self.link(false),
            KeyCode::Char('y') => self.link(true),
            KeyCode::Char('c') => Action::OpenPicker { compose: false },
            KeyCode::Char('n') => Action::OpenPicker { compose: true },
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                Action::None
            }
            KeyCode::Char('r') => self.selected.map_or(Action::None, Action::MarkRead),
            KeyCode::Char('R') if !self.items.is_empty() => {
                self.mode = Mode::ConfirmReadAll;
                Action::None
            }
            KeyCode::Char('o') => self.selected.map_or(Action::None, |item| Action::Open {
                item,
                message: (self.focus == Focus::Messages).then_some(self.cursor),
            }),
            KeyCode::Enter => self.enter(),
            KeyCode::Char('s') => {
                self.compose_to_conversation();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Enter: reply where you are. From the list, to the conversation
    /// (in the thread for a thread item); on a message of the pane, in
    /// that message's thread; inside a thread, in it.
    fn enter(&mut self) -> Action {
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

    /// `s`, and the end of `n`: a message to the selected conversation
    /// itself, wherever the focus is in it (a thread item takes it in
    /// the thread, since that is the conversation it stands for).
    pub fn compose_to_conversation(&mut self) {
        if let Some(item) = self.selected {
            self.mode = Mode::Compose(Draft::new(self.items[item].thread_ts.clone()));
        }
    }

    /// One level deeper, the way `l` moves in vim: the messages pane from
    /// the list, then the highlighted message's thread.
    fn deeper(&mut self) -> Action {
        match (self.focus, &self.thread) {
            (Focus::List, _) => {
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

    /// The picker opens in its filter, where typing narrows the list,
    /// until Enter hands the keys to the narrowed list (Esc clears what
    /// was typed, and with nothing typed closes the picker). In the list
    /// `j`/`k`, `gg` and `G` move, `/` goes back to the filter, Space
    /// marks a person for a group message, `m` shows the muted ones,
    /// Enter or `l` opens, Esc or `h` gives up, `q` quits as everywhere.
    /// The arrows, Tab and Ctrl-n/p move in both.
    fn pick(&mut self, mut picker: Picker, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let after_g = std::mem::take(&mut self.pending_g);
        match key.code {
            KeyCode::Down | KeyCode::Tab => picker.step(1),
            KeyCode::Up | KeyCode::BackTab => picker.step(-1),
            KeyCode::Char('n') if control => picker.step(1),
            KeyCode::Char('p') if control => picker.step(-1),
            KeyCode::Enter if picker.filtering => picker.filtering = false,
            KeyCode::Esc if picker.filtering && picker.query.is_empty() => return Action::None,
            KeyCode::Esc if picker.filtering => {
                picker.query.clear();
                picker.selected = 0;
            }
            KeyCode::Left if picker.filtering => {}
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
            KeyCode::Char(' ') => picker.mark(),
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

    /// `x`: an opened conversation leaves the list. What needs you stays
    /// until it is read.
    fn close_opened(&mut self) {
        if let Some(index) = self.selected
            && self.items[index].section == Section::Opened
        {
            self.remove(index);
        }
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
    use crate::counts::Kind;

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
        }
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
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
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
        state.key(press(KeyCode::Enter));
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
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
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
        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('j')));
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
    fn o_opens_slack_at_the_highlighted_message_from_the_pane() {
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('o'))),
            Action::Open {
                item: 0,
                message: None
            }
        );
        state.key(press(KeyCode::Char('l')));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Char('o'))),
            Action::Open {
                item: 0,
                message: Some(1)
            }
        );
    }

    #[test]
    fn marking_everything_asks_first() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('R'))), Action::None);
        assert_eq!(state.mode, Mode::ConfirmReadAll);
        assert_eq!(state.key(press(KeyCode::Char('n'))), Action::None);
        state.key(press(KeyCode::Char('R')));
        assert_eq!(state.key(press(KeyCode::Char('y'))), Action::MarkAllRead);
        assert_eq!(state.key(press(KeyCode::Char('r'))), Action::MarkRead(0));
        assert_eq!(state.key(control('r')), Action::Refresh);
        assert_eq!(state.key(press(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(state.key(control('c')), Action::Quit);
    }

    #[test]
    fn the_selection_follows_the_item_across_a_refresh_and_a_removal() {
        let mut state = state();
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('l')));
        state.replace_items(vec![
            item(Section::Mentions, "C1", None),
            item(Section::Threads, "C2", Some("2.000100")),
        ]);
        assert_eq!(state.selected, Some(0), "C1 is now first");
        assert_eq!(state.focus, Focus::List);
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
    fn the_picker_opens_in_its_filter_and_enter_hands_the_keys_to_the_list() {
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('c'))),
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
            Action::None,
            "Enter hands the keys to the list"
        );
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('j')));
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert!(!picker.filtering);
        assert_eq!(picker.query, "dev", "the filter stays");
        assert_eq!(picker.selected, 1, "j moves and stops at the last match");
        state.key(press(KeyCode::Char('/')));
        state.key(press(KeyCode::Char('-')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.filtering && p.query == "dev-" && p.selected == 0),
            "/ goes back to the filter, where typing goes on"
        );
        state.key(press(KeyCode::Backspace));
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char('j')));
        assert_eq!(
            state.key(press(KeyCode::Char('l'))),
            Action::Show {
                choice: choices[3].clone(),
                compose: false
            }
        );
        assert_eq!(state.mode, Mode::Browse);

        state.mode = Mode::Pick(Picker::new(choices.clone(), false));
        for c in "zzz".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Enter));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "nothing matches, nothing opens"
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
    }

    #[test]
    fn the_reply_box_completes_people_and_channels() {
        let mut state = state();
        state.key(press(KeyCode::Enter));
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

        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Enter));
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
    fn n_picks_people_for_a_new_message_and_space_marks_several() {
        let mut state = state();
        assert_eq!(
            state.key(press(KeyCode::Char('n'))),
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
    fn x_closes_an_opened_conversation_and_nothing_else() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('x'))), Action::None);
        assert_eq!(state.items.len(), 3, "what needs you is not closed");
        let page = history::Unread {
            messages: vec![message("3.000100")],
            has_more: false,
        };
        state.open_item(Item::opened("C7".into(), "#ops".into(), page, None));
        assert_eq!((state.selected, state.focus), (Some(3), Focus::Messages));
        assert_eq!(state.key(press(KeyCode::Char('x'))), Action::None);
        assert_eq!(state.items.len(), 3);
        assert_eq!((state.selected, state.focus), (Some(2), Focus::List));
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

        state.replace_items(vec![item(Section::Mentions, "C1", None)]);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[1].section, Section::Opened);
        assert_eq!(state.selected, Some(1), "still on the opened one");
        state.items[1].caught_up();
        assert_eq!(state.items[1].unread_from, None);
    }

    #[test]
    fn control_keys_do_not_type_into_the_reply_box() {
        let mut state = state();
        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Enter));
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
        state.key(press(KeyCode::Enter));
        state.key(press(KeyCode::Char(' ')));
        assert!(
            matches!(&state.mode, Mode::Pick(p) if p.marked == [1, 2]),
            "a mark is an index into the choices, not into the matches"
        );
        state.key(press(KeyCode::Char('/')));
        state.key(press(KeyCode::Esc));
        assert!(
            matches!(
                &state.mode,
                Mode::Pick(p) if p.query.is_empty() && p.selected == 0 && p.marked == [1, 2]
            ),
            "Esc clears the filter and keeps the marks"
        );
        state.key(press(KeyCode::Enter));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::ShowGroup {
                users: vec!["U9".to_string(), "U3".to_string()],
                compose: true
            },
            "the highlighted channel yields to the marks"
        );
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
}
