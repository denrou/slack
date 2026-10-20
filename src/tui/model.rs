//! The TUI's state and its pure transitions: what the list holds, what is
//! selected, which mode the keys go to. No I/O, so the keys are testable;
//! `mod.rs` performs the actions.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::history::Message;
use crate::names::Names;
use crate::style::Style;
use crate::view::{self, Entry, View};
use crate::{mrkdwn, threads};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    DirectMessages,
    Mentions,
    Threads,
    Channels,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::DirectMessages => "Direct messages",
            Section::Mentions => "Mentions",
            Section::Threads => "Threads",
            Section::Channels => "Channels",
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
        if let Some(threads) = view.threads {
            for thread in threads.items.unwrap_or_default() {
                items.push(Item::thread(thread, names));
            }
        }
        if view.all {
            conversations(Section::Channels, view.channels, &mut items);
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

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Writing a reply to the selected item.
    Compose(String),
    ConfirmReadAll,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    ToggleAll,
    MarkRead(usize),
    MarkAllRead,
    Send { item: usize, text: String },
    Open(usize),
}

pub struct State {
    pub items: Vec<Item>,
    /// Index into `items`; headers are never selected.
    pub selected: Option<usize>,
    /// Lines scrolled in the messages pane.
    pub scroll: u16,
    pub mode: Mode,
    /// Shown in the bottom line until the next key.
    pub status: Option<String>,
    pub all: bool,
    pub me: Option<String>,
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
        }
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.selected.and_then(|index| self.items.get(index))
    }

    /// New items after a refresh; the selection follows the same
    /// conversation when it is still there.
    pub fn replace_items(&mut self, items: Vec<Item>) {
        let key = self
            .selected_item()
            .map(|item| (item.channel.clone(), item.thread_ts.clone()));
        self.items = items;
        self.selected = key
            .and_then(|(channel, thread_ts)| {
                self.items
                    .iter()
                    .position(|item| item.key() == (channel.as_str(), thread_ts.as_deref()))
            })
            .or_else(|| (!self.items.is_empty()).then_some(0));
        self.scroll = 0;
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
        self.scroll = 0;
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        self.status = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Browse => self.browse(key),
            Mode::Compose(text) => self.compose(text, key),
            Mode::ConfirmReadAll => match key.code {
                KeyCode::Char('y' | 'Y') => Action::MarkAllRead,
                _ => Action::None,
            },
            Mode::Help => Action::None,
        }
    }

    fn browse(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.select(1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select(-1);
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
            KeyCode::Enter | KeyCode::Char('s') if self.selected.is_some() => {
                self.mode = Mode::Compose(String::new());
                Action::None
            }
            _ => Action::None,
        }
    }

    fn compose(&mut self, mut text: String, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => Action::None,
            KeyCode::Enter => {
                let trimmed = text.trim().to_string();
                match self.selected {
                    Some(item) if !trimmed.is_empty() => Action::Send {
                        item,
                        text: trimmed,
                    },
                    Some(_) => {
                        self.mode = Mode::Compose(text);
                        Action::None
                    }
                    None => Action::None,
                }
            }
            KeyCode::Backspace => {
                text.pop();
                self.mode = Mode::Compose(text);
                Action::None
            }
            KeyCode::Char(c) => {
                text.push(c);
                self.mode = Mode::Compose(text);
                Action::None
            }
            _ => {
                self.mode = Mode::Compose(text);
                Action::None
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
            self.scroll = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(section: Section, channel: &str, thread_ts: Option<&str>) -> Item {
        Item {
            section,
            channel: channel.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            title: channel.to_string(),
            detail: String::new(),
            messages: Vec::new(),
            has_more: false,
            latest: Some("1.000100".to_string()),
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
    fn replying_goes_through_the_compose_mode() {
        let mut state = state();
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
        assert_eq!(state.mode, Mode::Compose(String::new()));
        for c in "ok!".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        state.key(press(KeyCode::Backspace));
        assert_eq!(state.mode, Mode::Compose("ok".to_string()));
        assert_eq!(
            state.key(press(KeyCode::Enter)),
            Action::Send {
                item: 0,
                text: "ok".to_string()
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
        state.replace_items(vec![
            item(Section::Mentions, "C1", None),
            item(Section::Threads, "C2", Some("2.000100")),
        ]);
        assert_eq!(state.selected, Some(0), "C1 is now first");
        state.remove(0);
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.items[0].channel, "C2");
        state.remove(0);
        assert_eq!(state.selected, None);
        assert_eq!(state.key(press(KeyCode::Enter)), Action::None);
        assert_eq!(state.key(press(KeyCode::Char('r'))), Action::None);
    }
}
