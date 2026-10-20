//! The TUI's state and its pure transitions: what the list holds, what is
//! selected, which pane has the focus, which mode the keys go to. No I/O,
//! so the keys are testable; `mod.rs` performs the actions.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::history::{self, Message};
use crate::names::Names;
use crate::style::Style;
use crate::view::{self, Entry, View};
use crate::{directory, mrkdwn, threads};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    DirectMessages,
    Mentions,
    /// Channels you follow: private ones, and those set to "All new
    /// messages" in Slack.
    Channels,
    Threads,
    /// The other channels with unreads, listed with `-a` only.
    MoreChannels,
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
            Section::MoreChannels => "More channels",
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
    /// The read cursor of an opened conversation with unreads: the pane
    /// marks where "new" starts.
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
        if view.all {
            conversations(Section::MoreChannels, view.more_channels, &mut items);
        }
        items
    }

    fn conversation(section: Section, entry: Entry) -> Item {
        let messages = entry.messages.unwrap_or_default();
        let mut detail = format!(
            "{}{} unread",
            messages.len(),
            if entry.has_more { "+" } else { "" }
        );
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
            unread_from: None,
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
    /// The read cursor, when the conversation has unreads.
    pub unread_from: Option<String>,
}

/// The picker: every conversation and person, narrowed by what is typed.
#[derive(Debug, PartialEq, Eq)]
pub struct Picker {
    pub choices: Vec<Choice>,
    pub query: String,
    /// Index into `matches()`.
    pub selected: usize,
}

impl Picker {
    pub fn new(choices: Vec<Choice>) -> Picker {
        Picker {
            choices,
            query: String::new(),
            selected: 0,
        }
    }

    /// The choices the query keeps, as indexes into `choices`.
    pub fn matches(&self) -> Vec<usize> {
        self.choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                directory::matches(&self.query, &format!("{} {}", choice.label, choice.search))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn picked(&self) -> Option<Choice> {
        let matches = self.matches();
        matches
            .get(self.selected)
            .map(|&index| self.choices[index].clone())
    }

    fn step(&mut self, step: isize) {
        let last = self.matches().len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + step).clamp(0, last) as usize;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Writing a reply: to the selected item, or in `thread_ts`.
    Compose {
        text: String,
        thread_ts: Option<String>,
    },
    ConfirmReadAll,
    Help,
    Pick(Picker),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    ToggleAll,
    MarkRead(usize),
    MarkAllRead,
    Send {
        item: usize,
        text: String,
        thread_ts: Option<String>,
    },
    Open(usize),
    /// Open the thread of a message of the selected item.
    OpenThread {
        item: usize,
        message: usize,
    },
    /// Load the directory and show the picker.
    OpenPicker,
    /// Open a conversation from the picker with its recent history.
    Show(Choice),
}

pub struct State {
    pub items: Vec<Item>,
    /// Index into `items`; headers are never selected.
    pub selected: Option<usize>,
    /// Lines scrolled in the messages pane (list focus only).
    pub scroll: u16,
    pub mode: Mode,
    /// Shown in the bottom line until the next key.
    pub status: Option<String>,
    pub all: bool,
    pub me: Option<String>,
    pub focus: Focus,
    /// The highlighted message of the pane, when the pane has the focus.
    pub cursor: usize,
    pub thread: Option<ThreadPane>,
}

const SCROLL_STEP: u16 = 10;

impl State {
    pub fn new(items: Vec<Item>, all: bool, me: Option<String>) -> State {
        let selected = (!items.is_empty()).then_some(0);
        State {
            items,
            selected,
            scroll: 0,
            mode: Mode::Browse,
            status: None,
            all,
            me,
            focus: Focus::List,
            cursor: 0,
            thread: None,
        }
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
        self.read_from_the_end();
    }

    /// The pane focused on the newest message, where a history is read.
    pub fn read_from_the_end(&mut self) {
        let len = self.pane_messages().len();
        self.focus = if len == 0 {
            Focus::List
        } else {
            Focus::Messages
        };
        self.cursor = len.saturating_sub(1);
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
            Mode::Compose { text, thread_ts } => self.compose(text, thread_ts, key),
            Mode::ConfirmReadAll => match key.code {
                KeyCode::Char('y' | 'Y') => Action::MarkAllRead,
                _ => Action::None,
            },
            Mode::Help => Action::None,
            Mode::Pick(picker) => self.pick(picker, key),
        }
    }

    fn browse(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc => {
                if self.thread.is_some() || self.focus == Focus::Messages {
                    self.back();
                    Action::None
                } else {
                    Action::Quit
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.back();
                Action::None
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                if !self.pane_messages().is_empty() {
                    self.focus = Focus::Messages;
                }
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.step(1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.step(-1);
                Action::None
            }
            KeyCode::Char('J') | KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                Action::None
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                Action::None
            }
            KeyCode::Char('g') => Action::Refresh,
            KeyCode::Char('a') => Action::ToggleAll,
            KeyCode::Char('c') => Action::OpenPicker,
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                Action::None
            }
            KeyCode::Char('r') => self.selected.map_or(Action::None, Action::MarkRead),
            KeyCode::Char('R') if !self.items.is_empty() => {
                self.mode = Mode::ConfirmReadAll;
                Action::None
            }
            KeyCode::Char('o') => self.selected.map_or(Action::None, Action::Open),
            KeyCode::Enter | KeyCode::Char('s') => self.enter(),
            _ => Action::None,
        }
    }

    /// Enter: reply to the selected item from the list; open the
    /// highlighted message's thread from the pane; reply in the thread
    /// once one is open.
    fn enter(&mut self) -> Action {
        let Some(item) = self.selected else {
            return Action::None;
        };
        match (self.focus, &self.thread) {
            (Focus::List, _) => {
                self.mode = Mode::Compose {
                    text: String::new(),
                    thread_ts: self.items[item].thread_ts.clone(),
                };
                Action::None
            }
            (Focus::Messages, Some(thread)) => {
                self.mode = Mode::Compose {
                    text: String::new(),
                    thread_ts: Some(thread.thread_ts.clone()),
                };
                Action::None
            }
            (Focus::Messages, None) => Action::OpenThread {
                item,
                message: self.cursor,
            },
        }
    }

    fn compose(&mut self, mut text: String, thread_ts: Option<String>, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => Action::None,
            KeyCode::Enter => {
                let trimmed = text.trim().to_string();
                match self.selected {
                    Some(item) if !trimmed.is_empty() => Action::Send {
                        item,
                        text: trimmed,
                        thread_ts,
                    },
                    Some(_) => {
                        self.mode = Mode::Compose { text, thread_ts };
                        Action::None
                    }
                    None => Action::None,
                }
            }
            KeyCode::Backspace => {
                text.pop();
                self.mode = Mode::Compose { text, thread_ts };
                Action::None
            }
            KeyCode::Char(c) => {
                text.push(c);
                self.mode = Mode::Compose { text, thread_ts };
                Action::None
            }
            _ => {
                self.mode = Mode::Compose { text, thread_ts };
                Action::None
            }
        }
    }

    /// The picker: typing narrows, the arrows (or Tab, Ctrl-n/p) move,
    /// Enter opens, Esc gives up.
    fn pick(&mut self, mut picker: Picker, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Action::None,
            KeyCode::Enter => {
                if let Some(choice) = picker.picked() {
                    return Action::Show(choice);
                }
            }
            KeyCode::Down | KeyCode::Tab => picker.step(1),
            KeyCode::Up | KeyCode::BackTab => picker.step(-1),
            KeyCode::Char('n') if control => picker.step(1),
            KeyCode::Char('p') if control => picker.step(-1),
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c) if !control => {
                picker.query.push(c);
                picker.selected = 0;
            }
            _ => {}
        }
        self.mode = Mode::Pick(picker);
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

    fn step(&mut self, step: isize) {
        match self.focus {
            Focus::List => self.select(step),
            Focus::Messages => {
                let len = self.pane_messages().len();
                if len == 0 {
                    return;
                }
                let last = len as isize - 1;
                self.cursor = (self.cursor as isize + step).clamp(0, last) as usize;
            }
        }
    }

    fn select(&mut self, step: isize) {
        let Some(current) = self.selected else {
            return;
        };
        let last = self.items.len().saturating_sub(1) as isize;
        let next = (current as isize + step).clamp(0, last);
        if next as usize != current {
            self.selected = Some(next as usize);
            self.thread = None;
            self.cursor = 0;
            self.scroll = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn state() -> State {
        State::new(
            vec![
                item(Section::DirectMessages, "D1", None),
                item(Section::Mentions, "C1", None),
                item(Section::Threads, "C2", Some("2.000100")),
            ],
            false,
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
            unread_from: None,
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
        assert_eq!(
            state.mode,
            Mode::Compose {
                text: String::new(),
                thread_ts: None
            }
        );
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
        assert!(matches!(state.mode, Mode::Compose { .. }));
        state.key(press(KeyCode::Esc));
        assert_eq!(state.mode, Mode::Browse);

        // A thread item from the list replies in its thread.
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Char('j')));
        state.key(press(KeyCode::Enter));
        assert_eq!(
            state.mode,
            Mode::Compose {
                text: String::new(),
                thread_ts: Some("2.000100".to_string())
            }
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
        assert_eq!(
            state.key(press(KeyCode::Enter)),
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
            Mode::Compose {
                text: String::new(),
                thread_ts: Some("1.000200".to_string())
            }
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
        assert_eq!(state.key(press(KeyCode::Esc)), Action::Quit);
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
        assert_eq!(state.key(press(KeyCode::Char('o'))), Action::Open(0));
        assert_eq!(state.key(press(KeyCode::Char('g'))), Action::Refresh);
        assert_eq!(state.key(press(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
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
    fn the_picker_narrows_on_what_is_typed_and_opens_on_enter() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Char('c'))), Action::OpenPicker);
        let choices = vec![
            choice(ChoiceKind::Channel, "C5", "#dev-team", ""),
            choice(ChoiceKind::Channel, "C6", "#general", ""),
            choice(ChoiceKind::Dm, "D9", "@Jane", "jdoe Jane Doe SRE"),
            choice(ChoiceKind::Person, "U3", "@Bob", "bob Bob Devlin"),
        ];
        state.mode = Mode::Pick(Picker::new(choices.clone()));
        for c in "dev".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.matches(), [0, 3], "label or search text");
        state.key(press(KeyCode::Down));
        state.key(press(KeyCode::Down));
        let Mode::Pick(picker) = &state.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.selected, 1, "stops at the last match");
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Show(choices[3].clone())
        );
        assert_eq!(state.mode, Mode::Browse);

        state.mode = Mode::Pick(Picker::new(choices));
        for c in "zzz".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::None,
            "nothing matches, nothing opens"
        );
        state.key(press(KeyCode::Backspace));
        assert!(matches!(&state.mode, Mode::Pick(p) if p.query == "zz"));
        assert_eq!(state.key(press(KeyCode::Esc)), Action::None);
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn an_opened_conversation_is_read_from_its_end_and_survives_a_refresh() {
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
        assert_eq!((state.focus, state.cursor), (Focus::Messages, 2));

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

        state.replace_items(vec![item(Section::Mentions, "C1", None)]);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[1].section, Section::Opened);
        assert_eq!(state.selected, Some(1), "still on the opened one");
        state.items[1].caught_up();
        assert_eq!(state.items[1].unread_from, None);
    }
}
