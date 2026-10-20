//! `slack` with no command: the view as something to move through and
//! answer. One list
//! of what needs you on the left, the messages of the selected item on the
//! right (or the thread opened from one of them), a reply box at the
//! bottom. Same model as the text view; this module adds state and keys.
//! API calls stay blocking: each takes a few hundred milliseconds, and the
//! status line says so meanwhile.

mod model;
mod ui;

use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Result, anyhow};
use jiff::Zoned;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use serde_json::Value;

use self::model::{Action, Choice, ChoiceKind, Item, Mode, Picker, Section, State, ThreadPane};
use crate::api::{self, Client};
use crate::counts::Kind;
use crate::history::Message;
use crate::names::Names;
use crate::style::Style;
use crate::{counts, directory, history, lastview, mrkdwn, replies, target, view};

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    me: Option<&str>,
    workspace_url: &str,
) -> Result<()> {
    let items = load(client, names)?;
    let mut app = App {
        client,
        names,
        team_id,
        workspace_url: workspace_url.trim_end_matches('/'),
        state: State::new(items, me.map(str::to_string)),
        directory_walked: false,
    };
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

fn load(client: &Client, names: &mut Names) -> Result<Vec<Item>> {
    let counts = counts::fetch(client)?;
    // The screen reads with context: the recent history, the new part
    // told apart. The text view reads the new part alone.
    let view = view::build(client, &counts, names, view::Reach::Recent)?;
    Ok(Item::from_view(view, names))
}

struct App<'a> {
    client: &'a Client,
    names: &'a mut Names,
    team_id: &'a str,
    workspace_url: &'a str,
    state: State,
    /// The whole directory was read once this session; the picker needs
    /// it for the people without a DM, and once is enough.
    directory_walked: bool,
}

impl App<'_> {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.draw(terminal)?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue; // a resize redraws on the next pass
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let action = self.state.key(key);
            if action == Action::Quit {
                return Ok(());
            }
            self.perform(action, terminal)?;
        }
    }

    fn draw(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let now = Zoned::now();
        let mut anchor = 0;
        terminal.draw(|frame| anchor = ui::draw(frame, &self.state, self.names, &now))?;
        self.state.anchor = anchor;
        Ok(())
    }

    /// Show a status line before a call that takes a moment.
    fn busy(&mut self, terminal: &mut DefaultTerminal, text: &str) -> Result<()> {
        self.state.status = Some(text.to_string());
        self.draw(terminal)
    }

    fn perform(&mut self, action: Action, terminal: &mut DefaultTerminal) -> Result<()> {
        match action {
            Action::None | Action::Quit => Ok(()),
            Action::Refresh => self.refresh(terminal),
            Action::MarkRead(index) => {
                self.busy(terminal, "Marking as read…")?;
                match self.mark_read(index) {
                    Ok(title) => {
                        self.after_mark(index);
                        self.state.status = Some(format!("Marked {title} as read"));
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::MarkAllRead => {
                self.busy(terminal, "Marking everything as read…")?;
                let mut marked = 0;
                // Last to first, so an index stays valid after a removal.
                for index in (0..self.state.items.len()).rev() {
                    match self.mark_read(index) {
                        Ok(_) => {
                            self.after_mark(index);
                            marked += 1;
                        }
                        Err(err) => return self.fail(err),
                    }
                }
                self.state.status = Some(format!("Marked {marked} as read"));
                Ok(())
            }
            Action::Send {
                item,
                text,
                thread_ts,
            } => {
                self.busy(terminal, "Sending…")?;
                match self.send(item, &text, thread_ts.as_deref()) {
                    Ok(title) => self.after_send(item, &title),
                    Err(err) => self.fail(err),
                }
            }
            Action::Open(index) => match self.open(index) {
                Ok(()) => {
                    self.state.status = Some("Opened in Slack".to_string());
                    Ok(())
                }
                Err(err) => self.fail(err),
            },
            Action::OpenThread { item, message } => {
                self.busy(terminal, "Loading the thread…")?;
                match self.open_thread(item, message) {
                    Ok(pane) => {
                        self.state.open_thread(pane);
                        self.state.status = None;
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::OpenPicker => {
                self.busy(terminal, "Loading the directory…")?;
                match self.choices() {
                    Ok(choices) => {
                        self.state.mode = Mode::Pick(Picker::new(choices));
                        self.state.status = None;
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::Show(choice) => {
                self.busy(terminal, &format!("Loading {}…", choice.label))?;
                match self.show(&choice) {
                    Ok(item) => {
                        self.state.open_item(item);
                        self.state.status = None;
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
        }
    }

    fn refresh(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.busy(terminal, "Refreshing…")?;
        match load(self.client, self.names) {
            Ok(items) => {
                self.state.replace_items(items);
                self.state.status = None;
                Ok(())
            }
            Err(err) => self.fail(err),
        }
    }

    /// A dead session ends the program (main says to log in again); any
    /// other failure is a status line, and the list stays as it was.
    fn fail(&mut self, err: anyhow::Error) -> Result<()> {
        if matches!(
            err.downcast_ref::<api::Error>(),
            Some(api::Error::Auth { .. })
        ) {
            return Err(err);
        }
        self.state.status = Some(format!("Error: {err:#}"));
        Ok(())
    }

    fn mark_read(&mut self, index: usize) -> Result<String> {
        let item = &self.state.items[index];
        let latest = item
            .latest
            .as_deref()
            .ok_or_else(|| anyhow!("{} has no message to mark at", item.title))?;
        match &item.thread_ts {
            None => {
                self.client.call(
                    "conversations.mark",
                    &[("channel", &item.channel), ("ts", latest)],
                )?;
                let _ = lastview::forget(self.team_id, std::slice::from_ref(&item.channel));
            }
            Some(thread_ts) => {
                self.client.call(
                    "subscriptions.thread.mark",
                    &[
                        ("channel", &item.channel),
                        ("thread_ts", thread_ts),
                        ("ts", latest),
                        ("read", "1"),
                    ],
                )?;
            }
        }
        Ok(item.title.clone())
    }

    /// What needed you leaves the list once read; an opened conversation
    /// stays, with nothing new in it.
    fn after_mark(&mut self, index: usize) {
        if self.state.items[index].section == Section::Opened {
            self.state.items[index].caught_up();
        } else {
            self.state.remove(index);
        }
    }

    /// Post the reply. A conversation answered from the list is then read
    /// up to the reply, as it is in the app when you answer.
    fn send(&mut self, index: usize, text: &str, thread_ts: Option<&str>) -> Result<String> {
        let item = &self.state.items[index];
        let mut params = vec![("channel", item.channel.as_str()), ("text", text)];
        if let Some(thread_ts) = thread_ts {
            params.push(("thread_ts", thread_ts));
        }
        let posted = self.client.call("chat.postMessage", &params)?;
        if thread_ts.is_none()
            && let Some(ts) = posted.get("ts").and_then(Value::as_str)
        {
            // Best effort: the reply is sent either way.
            let _ = self.client.call(
                "conversations.mark",
                &[("channel", &item.channel), ("ts", ts)],
            );
            let _ = lastview::forget(self.team_id, std::slice::from_ref(&item.channel));
        }
        Ok(item.title.clone())
    }

    /// After a reply: inside an open thread the thread is reloaded to show
    /// it; an opened conversation is reloaded the same way; from the list
    /// the item is done with and leaves the list.
    fn after_send(&mut self, item: usize, title: &str) -> Result<()> {
        if let Some(pane) = self.state.thread.take() {
            return match self.reload_thread(pane) {
                Ok(pane) => {
                    self.state.thread = Some(pane);
                    self.state.status = Some("Sent".to_string());
                    Ok(())
                }
                Err(err) => self.fail(err),
            };
        }
        if self.state.items[item].section == Section::Opened {
            let channel = self.state.items[item].channel.clone();
            return match self.load_opened(channel, None) {
                Ok(reloaded) => {
                    self.state.open_item(reloaded);
                    self.state.status = Some("Sent".to_string());
                    Ok(())
                }
                Err(err) => self.fail(err),
            };
        }
        self.state.remove(item);
        self.state.status = Some(format!("Sent to {title}"));
        Ok(())
    }

    fn open_thread(&mut self, item: usize, message: usize) -> Result<ThreadPane> {
        let it = &self.state.items[item];
        let msg = it
            .messages
            .get(message)
            .ok_or_else(|| anyhow!("no message is highlighted"))?;
        let thread_ts = msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone());
        let channel = it.channel.clone();
        let fetched = replies::fetch(self.client, &channel, &thread_ts)?;
        self.resolve_people(std::iter::once(&fetched.root).chain(&fetched.replies))?;
        let user = |id: &str| self.names.user_name(id);
        let chan = |id: &str| self.names.channel_name(id);
        let root = mrkdwn::render(&fetched.root.text, &user, &chan, &Style::plain());
        let title = format!(
            "{} › {}",
            self.names.channel_label(&channel),
            view::excerpt(&root, 40)
        );
        let mut messages = vec![fetched.root];
        messages.extend(fetched.replies);
        Ok(ThreadPane {
            channel,
            thread_ts,
            title,
            messages,
            has_more: fetched.has_more,
            from_message: message,
        })
    }

    fn reload_thread(&mut self, pane: ThreadPane) -> Result<ThreadPane> {
        let fetched = replies::fetch(self.client, &pane.channel, &pane.thread_ts)?;
        self.resolve_people(std::iter::once(&fetched.root).chain(&fetched.replies))?;
        let mut messages = vec![fetched.root];
        messages.extend(fetched.replies);
        Ok(ThreadPane {
            messages,
            has_more: fetched.has_more,
            ..pane
        })
    }

    /// Every conversation you are in, then everyone else in the
    /// workspace, for the picker.
    fn choices(&mut self) -> Result<Vec<Choice>> {
        let listed = directory::conversations(self.client, self.names)?;
        if !self.directory_walked {
            self.names.walk_users(self.client)?;
            self.directory_walked = true;
        }
        let mut choices = Vec::new();
        let mut with_dm = BTreeSet::new();
        for conversation in &listed {
            let kind = match conversation.kind {
                Kind::Channel => ChoiceKind::Channel,
                Kind::Mpim => ChoiceKind::Group,
                Kind::Im => ChoiceKind::Dm,
            };
            let person = conversation
                .user
                .as_deref()
                .and_then(|id| self.names.user(id));
            if let Some(id) = &conversation.user {
                with_dm.insert(id.clone());
            }
            choices.push(Choice {
                kind,
                id: conversation.id.clone(),
                label: conversation.name.clone(),
                search: person
                    .map(|p| format!("{} {} {}", p.name, p.real, p.title))
                    .unwrap_or_default(),
                note: conversation.notes().join(", "),
                unread_from: conversation.unread_from().map(str::to_string),
            });
        }
        let me = self.state.me.as_deref();
        for person in directory::everyone(self.names) {
            if with_dm.contains(&person.id) || Some(person.id.as_str()) == me {
                continue;
            }
            let note = if person.title.is_empty() {
                person.real.clone()
            } else {
                person.title.clone()
            };
            choices.push(Choice {
                kind: ChoiceKind::Person,
                label: format!("@{}", person.display),
                search: person.search(),
                note,
                id: person.id,
                unread_from: None,
            });
        }
        Ok(choices)
    }

    fn show(&mut self, choice: &Choice) -> Result<Item> {
        let channel = match choice.kind {
            ChoiceKind::Person => target::open_dm(self.client, &choice.id)?,
            _ => choice.id.clone(),
        };
        self.load_opened(channel, choice.unread_from.clone())
    }

    /// The recent history of a conversation, as an item of the list.
    fn load_opened(&mut self, channel: String, unread_from: Option<String>) -> Result<Item> {
        let page = history::fetch(self.client, &channel, None, view::RECENT_LIMIT)?;
        self.names
            .resolve_channels(self.client, [channel.clone()])?;
        self.resolve_people(page.messages.iter())?;
        let title = self.names.channel_label(&channel);
        Ok(Item::opened(channel, title, page, unread_from))
    }

    fn resolve_people<'m>(&mut self, messages: impl Iterator<Item = &'m Message>) -> Result<()> {
        let mut people = BTreeSet::new();
        for message in messages {
            people.extend(message.user.clone());
            people.extend(view::mentioned_users(&message.text));
        }
        self.names.resolve_users(self.client, people)?;
        Ok(())
    }

    fn open(&self, index: usize) -> Result<()> {
        let item = &self.state.items[index];
        let url = match &item.thread_ts {
            Some(thread_ts) => format!(
                "{}/archives/{}/p{}",
                self.workspace_url,
                item.channel,
                thread_ts.replace('.', "")
            ),
            None => format!("{}/archives/{}", self.workspace_url, item.channel),
        };
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let status = Command::new(opener)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(anyhow!("{opener} could not open {url}"));
        }
        Ok(())
    }
}
