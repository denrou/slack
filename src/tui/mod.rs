//! `slack ui`: the view as something to move through and answer. One list
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

use self::model::{Action, Item, State, ThreadPane};
use crate::api::{self, Client};
use crate::names::Names;
use crate::style::Style;
use crate::{counts, lastview, mrkdwn, replies, view};

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    me: Option<&str>,
    workspace_url: &str,
    all: bool,
) -> Result<()> {
    let items = load(client, names, all)?;
    let mut app = App {
        client,
        names,
        team_id,
        workspace_url: workspace_url.trim_end_matches('/'),
        state: State::new(items, all, me.map(str::to_string)),
    };
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

fn load(client: &Client, names: &mut Names, all: bool) -> Result<Vec<Item>> {
    let counts = counts::load(client, names)?;
    let view = view::build(client, &counts, names, all)?;
    Ok(Item::from_view(view, names))
}

struct App<'a> {
    client: &'a Client,
    names: &'a mut Names,
    team_id: &'a str,
    workspace_url: &'a str,
    state: State,
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
        terminal.draw(|frame| ui::draw(frame, &self.state, self.names, &now))?;
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
            Action::ToggleAll => {
                self.state.all = !self.state.all;
                self.refresh(terminal)
            }
            Action::MarkRead(index) => {
                self.busy(terminal, "Marking as read…")?;
                match self.mark_read(index) {
                    Ok(title) => {
                        self.state.remove(index);
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
                            self.state.remove(index);
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
        }
    }

    fn refresh(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.busy(terminal, "Refreshing…")?;
        match load(self.client, self.names, self.state.all) {
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
    /// it; from the list the item is done with and leaves the list.
    fn after_send(&mut self, item: usize, title: &str) -> Result<()> {
        match self.state.thread.take() {
            Some(pane) => match self.reload_thread(pane) {
                Ok(pane) => {
                    self.state.thread = Some(pane);
                    self.state.status = Some("Sent".to_string());
                    Ok(())
                }
                Err(err) => self.fail(err),
            },
            None => {
                self.state.remove(item);
                self.state.status = Some(format!("Sent to {title}"));
                Ok(())
            }
        }
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
        self.resolve_people(&fetched)?;
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
        self.resolve_people(&fetched)?;
        let mut messages = vec![fetched.root];
        messages.extend(fetched.replies);
        Ok(ThreadPane {
            messages,
            has_more: fetched.has_more,
            ..pane
        })
    }

    fn resolve_people(&mut self, fetched: &replies::Replies) -> Result<()> {
        let mut people = BTreeSet::new();
        for message in std::iter::once(&fetched.root).chain(&fetched.replies) {
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
