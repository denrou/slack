//! `slack` with no command: the view as something to move through and
//! answer. One list of what needs you on the left, the messages of the
//! selected item on the right (or the thread opened from one of them), a
//! reply box at the bottom. Same model as the text view; this module adds
//! state and keys.
//! API calls stay blocking: each takes a few hundred milliseconds, and the
//! status line says so meanwhile.

mod model;
mod ui;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use jiff::Zoned;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use serde_json::Value;

use self::model::{
    Action, Candidate, Choice, ChoiceKind, Draft, Item, Mode, Picker, Section, State, ThreadPane,
    Viewport,
};
use crate::api::{self, Client};
use crate::counts::{Counts, Kind};
use crate::directory::{self, Listed};
use crate::history::Message;
use crate::names::Names;
use crate::style::Style;
use crate::{
    counts, emoji, history, lastview, mention, mrkdwn, react, read, replies, target, view, warn,
};

pub fn run(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    me: Option<&str>,
    workspace_url: &str,
) -> Result<()> {
    // Warnings would draw over the screen: they wait for the status line,
    // and whatever is left goes to stderr once the screen is gone.
    warn::hold();
    let result = screen(client, names, team_id, me, workspace_url);
    for message in warn::release() {
        eprintln!("warning: {message}");
    }
    result
}

fn screen(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    me: Option<&str>,
    workspace_url: &str,
) -> Result<()> {
    let counts = counts::fetch(client)?;
    let items = load(client, names, &counts)?;
    let mut app = App {
        client,
        names,
        team_id,
        workspace_url: workspace_url.trim_end_matches('/'),
        state: State::new(items, me.map(str::to_string)),
        directory_walked: false,
        pulse: Pulse::of(&counts),
        polls: poll(client.clone()),
        written: Instant::now(),
    };
    app.state.frequent_emoji.clone_from(&counts.frequent_emoji);
    let mut terminal = ratatui::init();
    let modes = Modes::ask();
    let result = app.run(&mut terminal);
    drop(modes);
    ratatui::restore();
    result
}

/// What the screen asks of the terminal beyond ratatui's raw mode and
/// alternate screen, undone on drop: on a panic too, since ratatui's hook
/// restores its own only, and some terminals keep the keyboard flags
/// across screens.
struct Modes {
    enhanced: bool,
}

impl Modes {
    fn ask() -> Modes {
        // A paste then arrives as one event, line breaks included, instead
        // of as keystrokes that would send the first line and act on the
        // rest. A terminal without the feature ignores the sequence.
        let _ = execute!(io::stdout(), EnableBracketedPaste);
        // Ctrl-Enter and Shift-Enter are plain Enter to a terminal that
        // does not speak the kitty keyboard protocol, so it is asked for
        // where spoken; a terminal that answers the query with nothing
        // costs two seconds here.
        let enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
        if enhanced {
            let _ = execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        Modes { enhanced }
    }
}

impl Drop for Modes {
    fn drop(&mut self) {
        if self.enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(io::stdout(), DisableBracketedPaste);
    }
}

fn is_dead_session(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<api::Error>(),
        Some(api::Error::Auth { .. })
    )
}

/// What became of a mark, with the item's title.
enum Marked {
    Done(String),
    /// Slack already read past it: left alone.
    Already(String),
    /// Nothing to mark at: an opened conversation with no message yet.
    Nothing(String),
}

fn load(client: &Client, names: &mut Names, counts: &Counts) -> Result<Vec<Item>> {
    // The screen reads with context: the recent history, the new part
    // told apart. The text view reads the new part alone.
    let view = view::build(client, counts, names, view::Reach::Recent)?;
    Ok(Item::from_view(view, names))
}

/// How often the screen asks Slack whether anything moved.
const POLL_EVERY: Duration = Duration::from_secs(15);

/// The counts every `POLL_EVERY` on a thread of its own, so the screen
/// never stalls on them: `client.counts` and the preferences side by side,
/// where a reload is one call per unread conversation. Each result carries
/// when its fetch began. Detached, so a quit never waits on a call in
/// flight; it stops once the screen drops the receiver.
fn poll(client: Client) -> Receiver<Polled> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            thread::sleep(POLL_EVERY);
            let began = Instant::now();
            if sender.send((began, counts::fetch(&client))).is_err() {
                return;
            }
        }
    });
    receiver
}

type Polled = (Instant, Result<Counts, api::Error>);

/// What a poll compares to decide on a reload: per unread conversation
/// its newest message, read cursor and mentions, and the threads block.
/// A new reply in a thread already unread moves none of them; Ctrl-R
/// picks it up.
#[derive(PartialEq, Eq)]
struct Pulse {
    conversations: Vec<(String, Option<String>, Option<String>, u64)>,
    threads: Option<(bool, u64)>,
}

impl Pulse {
    fn of(counts: &Counts) -> Pulse {
        let mut conversations: Vec<_> = counts
            .unread()
            .map(|c| {
                let (id, latest, read) = (c.id.clone(), c.latest.clone(), c.last_read.clone());
                (id, latest, read, c.mention_count)
            })
            .collect();
        conversations.sort();
        Pulse {
            conversations,
            threads: counts
                .threads
                .map(|threads| (threads.has_unreads, threads.mention_count)),
        }
    }
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
    /// The counts the list was last loaded from.
    pulse: Pulse,
    polls: Receiver<Polled>,
    /// When the last mark or reply finished: a poll begun before it shows
    /// what was just read as unread, and is dropped.
    written: Instant,
}

impl App<'_> {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.draw(terminal)?;
            if !event::poll(Duration::from_millis(250))? {
                self.polled(terminal)?;
                continue;
            }
            let action = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.state.key(key),
                Event::Paste(text) => self.state.paste(&text),
                _ => continue, // a resize redraws on the next pass
            };
            if action == Action::Quit {
                return Ok(());
            }
            self.perform(action, terminal)?;
        }
    }

    fn draw(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let warnings = warn::drain();
        if !warnings.is_empty() {
            self.state.status = Some(format!("Warning: {}", warnings.join("; ")));
        }
        let now = Zoned::now();
        let mut viewport = Viewport::default();
        terminal.draw(|frame| viewport = ui::draw(frame, &self.state, self.names, &now))?;
        self.state.viewport = viewport;
        Ok(())
    }

    /// Show a status line before a call that takes a moment.
    fn busy(&mut self, terminal: &mut DefaultTerminal, text: &str) -> Result<()> {
        self.state.status = Some(text.to_string());
        self.draw(terminal)
    }

    fn perform(&mut self, action: Action, terminal: &mut DefaultTerminal) -> Result<()> {
        let writes = matches!(
            action,
            Action::MarkRead(_) | Action::MarkAllRead | Action::Send { .. }
        );
        let performed = self.act(action, terminal);
        if writes {
            self.written = Instant::now();
        }
        performed
    }

    fn act(&mut self, action: Action, terminal: &mut DefaultTerminal) -> Result<()> {
        match action {
            Action::None | Action::Quit => Ok(()),
            Action::Refresh => self.refresh(terminal),
            Action::MarkRead(index) => {
                self.busy(terminal, "Marking as read…")?;
                let marked = self
                    .cursors()
                    .and_then(|cursors| self.mark_read(index, &cursors));
                match marked {
                    Ok(Marked::Done(title)) => {
                        self.after_mark(index);
                        self.state.status = Some(format!("Marked {title} as read"));
                        Ok(())
                    }
                    Ok(Marked::Already(title)) => {
                        self.after_mark(index);
                        self.state.status = Some(format!("{title} was already read"));
                        Ok(())
                    }
                    Ok(Marked::Nothing(title)) => {
                        self.state.status = Some(format!("{title} has nothing to mark"));
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::MarkAllRead => {
                self.busy(terminal, "Marking everything as read…")?;
                let cursors = match self.cursors() {
                    Ok(cursors) => cursors,
                    Err(err) => return self.fail(err),
                };
                let mut marked = 0;
                // Last to first, so an index stays valid after a removal.
                for index in (0..self.state.items.len()).rev() {
                    match self.mark_read(index, &cursors) {
                        Ok(Marked::Done(_)) => {
                            self.after_mark(index);
                            marked += 1;
                        }
                        Ok(Marked::Already(_)) => self.after_mark(index),
                        Ok(Marked::Nothing(_)) => {}
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
                    Ok(title) => self.after_send(item, &title, thread_ts.as_deref()),
                    Err(err) => {
                        // The text stays in the box: a timeout must not cost it.
                        self.state.mode = Mode::Compose(Draft {
                            text,
                            thread_ts,
                            pick: None,
                        });
                        self.fail(err)
                    }
                }
            }
            Action::Open { item, message } => match self.open(item, message) {
                Ok(()) => {
                    self.state.status = Some("Opened in Slack".to_string());
                    Ok(())
                }
                Err(err) => self.fail(err),
            },
            Action::React {
                channel,
                ts,
                name,
                add,
            } => {
                self.busy(
                    terminal,
                    if add {
                        "Reacting…"
                    } else {
                        "Taking it back…"
                    },
                )?;
                match react::set(self.client, &channel, &ts, &name, add) {
                    Ok(()) => {
                        self.state.apply_reaction(&channel, &ts, &name, add);
                        let done = if add { "Reacted" } else { "Took back" };
                        self.state.status = Some(format!("{done} {}", emoji::label(&name)));
                        Ok(())
                    }
                    Err(err) => self.fail(err.into()),
                }
            }
            Action::OpenLink(url) => match launch(&url) {
                Ok(()) => {
                    self.state.status = Some(format!("Opened {url}"));
                    Ok(())
                }
                Err(err) => self.fail(err),
            },
            Action::CopyLink(url) => match copy(&url) {
                Ok(()) => {
                    self.state.status = Some(format!("Copied {url}"));
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
            Action::OpenPicker { compose } => {
                self.busy(terminal, "Loading the directory…")?;
                match self.directory() {
                    Ok(listed) => {
                        self.state.candidates = Some(self.candidates(&listed));
                        self.state.mode = Mode::Pick(Picker::new(self.choices(&listed), compose));
                        self.state.status = None;
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::LoadCandidates => {
                self.busy(terminal, "Loading the names…")?;
                match self.directory() {
                    Ok(listed) => {
                        self.state.candidates = Some(self.candidates(&listed));
                        self.state.status = None;
                        Ok(())
                    }
                    Err(err) => {
                        // Not again at every key: the box works without a list.
                        self.state.candidates = Some(Vec::new());
                        self.fail(err)
                    }
                }
            }
            Action::Show { choice, compose } => {
                self.busy(terminal, &format!("Loading {}…", choice.label))?;
                match self.show(&choice) {
                    Ok(item) => self.opened(item, compose),
                    Err(err) => self.fail(err),
                }
            }
            Action::ShowGroup { users, compose } => {
                self.busy(terminal, "Opening the group message…")?;
                match self.show_group(&users) {
                    Ok(item) => self.opened(item, compose),
                    Err(err) => self.fail(err),
                }
            }
        }
    }

    fn refresh(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.busy(terminal, "Refreshing…")?;
        let reloaded = counts::fetch(self.client)
            .map_err(anyhow::Error::from)
            .and_then(|counts| self.reload(&counts));
        match reloaded {
            Ok(()) => {
                self.state.status = None;
                Ok(())
            }
            Err(err) => self.fail(err),
        }
    }

    /// The latest poll, between keys and only while nothing is open over
    /// the list: a reload under the reply box would move the conversation
    /// it sends to, and one under the read-all prompt would add to what
    /// `y` marks. It waits in the channel until then.
    fn polled(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        if !matches!(self.state.mode, Mode::Browse | Mode::Help) {
            return Ok(());
        }
        let Some((began, polled)) = self.polls.try_iter().last() else {
            return Ok(());
        };
        if began < self.written {
            return Ok(());
        }
        let counts = match polled {
            Ok(counts) => counts,
            Err(err) => {
                let err = anyhow::Error::from(err).context("could not check for new messages");
                return self.fail(err);
            }
        };
        if Pulse::of(&counts) == self.pulse {
            return Ok(());
        }
        // The status line was for the last key, not for the refresh.
        let status = self.state.status.take();
        self.busy(terminal, "Refreshing…")?;
        match self.reload(&counts) {
            Ok(()) => {
                self.state.status = status;
                Ok(())
            }
            Err(err) => self.fail(err),
        }
    }

    /// The list from these counts, and the open thread with it.
    fn reload(&mut self, counts: &Counts) -> Result<()> {
        let items = load(self.client, self.names, counts)?;
        self.state.replace_items(items);
        self.state.frequent_emoji.clone_from(&counts.frequent_emoji);
        if let Some(pane) = self.state.thread.take() {
            let reloaded = self.reload_thread(&pane);
            // A failed reload keeps the thread as it was on the screen.
            let (pane, failed) = match reloaded {
                Ok(reloaded) => (reloaded, None),
                Err(err) => (pane, Some(err)),
            };
            self.state.thread = Some(pane);
            if let Some(err) = failed {
                return Err(err);
            }
        }
        self.pulse = Pulse::of(counts);
        Ok(())
    }

    /// A dead session ends the program (main says to log in again); any
    /// other failure is a status line, and the list stays as it was.
    fn fail(&mut self, err: anyhow::Error) -> Result<()> {
        if is_dead_session(&err) {
            return Err(err);
        }
        self.state.status = Some(format!("Error: {err:#}"));
        Ok(())
    }

    /// The read cursors as Slack has them now, so a mark never moves one
    /// back: what was read in the app since the load is left alone.
    fn cursors(&self) -> Result<BTreeMap<String, String>> {
        Ok(counts::cursors(self.client)?)
    }

    fn mark_read(&mut self, index: usize, cursors: &BTreeMap<String, String>) -> Result<Marked> {
        let item = &self.state.items[index];
        let title = item.title.clone();
        let Some(latest) = item.latest.as_deref() else {
            return Ok(Marked::Nothing(title));
        };
        match &item.thread_ts {
            None => {
                let cursor = cursors.get(&item.channel).map(String::as_str);
                if read::already_read(cursor, latest) {
                    return Ok(Marked::Already(title));
                }
                self.client.call(
                    "conversations.mark",
                    &[("channel", &item.channel), ("ts", latest)],
                )?;
                let _ = lastview::forget(self.team_id, std::slice::from_ref(&item.channel));
            }
            Some(thread_ts) => {
                // Written from the web client's traffic and not yet seen
                // working live (ARCHITECTURE.md).
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
        Ok(Marked::Done(title))
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

    /// Post the reply. A reply to the conversation itself, not to a thread,
    /// then reads it up to the reply in Slack, as the app does when you
    /// answer.
    fn send(&mut self, index: usize, text: &str, thread_ts: Option<&str>) -> Result<String> {
        let item = &self.state.items[index];
        let text = mention::link(text, self.names);
        let mut params = vec![("channel", item.channel.as_str()), ("text", text.as_str())];
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

    /// After a reply. To the conversation itself: Slack now reads it up to
    /// the reply, so a listed one leaves, a thread open from one of its
    /// messages with it, and an opened one is reloaded. In a thread: an
    /// open thread is reloaded to show the reply; a reply to a message from
    /// the pane leaves the conversation listed, since it stays unread in
    /// Slack, and reloads an opened one.
    fn after_send(&mut self, item: usize, title: &str, thread_ts: Option<&str>) -> Result<()> {
        let opened = self.state.items[item].section == Section::Opened;
        if thread_ts.is_none() {
            self.state.thread = None;
            if !opened {
                self.state.remove(item);
                self.state.status = Some(format!("Sent to {title}"));
                return Ok(());
            }
            return self.reload_opened(item);
        }
        if let Some(pane) = self.state.thread.take() {
            return match self.reload_thread(&pane) {
                Ok(reloaded) => {
                    self.state.thread = Some(reloaded);
                    self.state.status = Some("Sent".to_string());
                    Ok(())
                }
                Err(err) => {
                    self.state.thread = Some(pane);
                    self.sent_but(err)
                }
            };
        }
        if opened {
            return self.reload_opened(item);
        }
        if self.state.items[item].section != Section::Threads {
            self.state.status = Some("Sent in the thread".to_string());
            return Ok(());
        }
        self.state.remove(item);
        self.state.status = Some(format!("Sent to {title}"));
        Ok(())
    }

    fn reload_opened(&mut self, item: usize) -> Result<()> {
        let channel = self.state.items[item].channel.clone();
        match self.load_opened(channel, None) {
            Ok(reloaded) => {
                self.state.open_item(reloaded);
                self.state.status = Some("Sent".to_string());
                Ok(())
            }
            Err(err) => self.sent_but(err),
        }
    }

    /// The reply went through and the reload after it did not: say so,
    /// since "Error" alone invites sending it again.
    fn sent_but(&mut self, err: anyhow::Error) -> Result<()> {
        if is_dead_session(&err) {
            return Err(err);
        }
        self.state.status = Some(format!("Sent, but could not reload: {err:#}"));
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

    fn reload_thread(&mut self, pane: &ThreadPane) -> Result<ThreadPane> {
        let fetched = replies::fetch(self.client, &pane.channel, &pane.thread_ts)?;
        self.resolve_people(std::iter::once(&fetched.root).chain(&fetched.replies))?;
        let mut messages = vec![fetched.root];
        messages.extend(fetched.replies);
        Ok(ThreadPane {
            channel: pane.channel.clone(),
            thread_ts: pane.thread_ts.clone(),
            title: pane.title.clone(),
            messages,
            has_more: fetched.has_more,
            from_message: pane.from_message,
        })
    }

    /// Every conversation you are in, with the directory walked once per
    /// run so the people are current: what the picker and the reply
    /// box's completion are built from.
    fn directory(&mut self) -> Result<Vec<Listed>> {
        let listed = directory::conversations(self.client, self.names)?;
        if !self.directory_walked {
            self.names.walk_users(self.client)?;
            self.directory_walked = true;
        }
        Ok(listed)
    }

    /// What the reply box completes: the people, those you have a direct
    /// message with first, then the channels you are in.
    fn candidates(&self, listed: &[Listed]) -> Vec<Candidate> {
        let with_dm: BTreeSet<&str> = listed
            .iter()
            .filter_map(|conversation| conversation.user.as_deref())
            .collect();
        let mut people: Vec<(bool, Candidate)> = directory::everyone(self.names)
            .into_iter()
            .map(|person| {
                let name = if person.display == person.handle {
                    &person.real
                } else {
                    &person.display
                };
                let detail = [name.as_str(), person.title.as_str()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                let candidate = Candidate {
                    insert: format!("@{}", person.handle),
                    detail,
                    search: format!("{} {} {}", person.handle, person.display, person.real)
                        .to_lowercase(),
                };
                (!with_dm.contains(person.id.as_str()), candidate)
            })
            .collect();
        // A stable sort: the people you talk to come first, in name order.
        people.sort_by_key(|(no_dm, _)| *no_dm);
        let channels = listed
            .iter()
            .filter(|conversation| conversation.kind == Kind::Channel)
            .map(|conversation| {
                let name = conversation.name.trim_start_matches('#');
                Candidate {
                    insert: conversation.name.clone(),
                    detail: conversation.notes().join(", "),
                    search: format!("{name} {}", name.replace(['-', '_', '.'], " ")).to_lowercase(),
                }
            });
        people
            .into_iter()
            .map(|(_, candidate)| candidate)
            .chain(channels)
            .collect()
    }

    /// Every conversation you are in, then everyone else in the
    /// workspace, for the picker.
    fn choices(&self, listed: &[Listed]) -> Vec<Choice> {
        let mut choices = Vec::new();
        let mut with_dm = BTreeSet::new();
        for conversation in listed {
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
                muted: conversation.muted,
                unread_from: conversation.unread_from().map(str::to_string),
                user: conversation.user.clone(),
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
                muted: false,
                user: Some(person.id.clone()),
                id: person.id,
                unread_from: None,
            });
        }
        choices
    }

    fn show(&mut self, choice: &Choice) -> Result<Item> {
        let channel = match choice.kind {
            ChoiceKind::Person => target::open_dm(self.client, &choice.id)?,
            _ => choice.id.clone(),
        };
        self.load_opened(channel, choice.unread_from.clone())
    }

    /// The group message with the marked people, opened if need be.
    fn show_group(&mut self, users: &[String]) -> Result<Item> {
        let channel = target::open_group(self.client, users)?;
        self.load_opened(channel, None)
    }

    /// The conversation is in the list; `n` goes on to write to it.
    fn opened(&mut self, item: Item, compose: bool) -> Result<()> {
        self.state.open_item(item);
        self.state.status = None;
        if compose {
            self.state.compose_to_conversation();
        }
        Ok(())
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

    /// The conversation in Slack: at the highlighted message when there
    /// is one (a reply links into its thread), else at the thread a
    /// thread item stands for, else at its end.
    fn open(&self, item: usize, message: Option<usize>) -> Result<()> {
        let item = &self.state.items[item];
        let channel = &item.channel;
        let base = format!("{}/archives/{channel}", self.workspace_url);
        let url = match message.and_then(|index| self.state.pane_messages().get(index)) {
            Some(message) => {
                let mut url = permalink(&base, &message.ts);
                if let Some(thread_ts) = message
                    .thread_ts
                    .as_deref()
                    .filter(|thread_ts| *thread_ts != message.ts)
                {
                    url.push_str(&format!("?thread_ts={thread_ts}&cid={channel}"));
                }
                url
            }
            None => match &item.thread_ts {
                Some(thread_ts) => permalink(&base, thread_ts),
                None => base,
            },
        };
        launch(&url)
    }
}

fn permalink(base: &str, ts: &str) -> String {
    format!("{base}/p{}", ts.replace('.', ""))
}

/// Hand a URL to the system opener: the browser, or the mail client for
/// a mailto link.
fn launch(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(anyhow!("{opener} could not open {url}"));
    }
    Ok(())
}

/// Put text in the clipboard through the first tool found: pbcopy on
/// macOS, wl-copy or xclip elsewhere. Without one, the text is in the
/// error, so the status line shows it and the mouse can take it.
fn copy(text: &str) -> Result<()> {
    let tools: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"]]
    };
    for tool in tools {
        let mut child = match Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        if !child.wait()?.success() {
            return Err(anyhow!("{} failed", tool[0]));
        }
        return Ok(());
    }
    Err(anyhow!(
        "no clipboard tool (pbcopy, wl-copy, xclip); the link is {text}"
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn counts(channels: Value, threads: Value) -> Counts {
        Counts::parse(&json!({"ok": true, "channels": channels, "threads": threads})).unwrap()
    }

    fn channel(id: &str, latest: &str, last_read: &str, mentions: u64) -> Value {
        json!({"id": id, "latest": latest, "last_read": last_read,
               "has_unreads": latest != last_read, "mention_count": mentions})
    }

    #[test]
    fn a_poll_reloads_only_when_an_unread_conversation_or_the_threads_moved() {
        let threads = json!({"has_unreads": true, "mention_count": 0});
        let before = Pulse::of(&counts(
            json!([
                channel("C1", "1.3", "1.1", 0),
                channel("C2", "2.1", "2.1", 0)
            ]),
            threads.clone(),
        ));
        let same = |channels: Value, threads: &Value| {
            Pulse::of(&counts(channels, threads.clone())) == before
        };
        assert!(
            same(
                json!([
                    channel("C2", "2.1", "2.1", 0),
                    channel("C1", "1.3", "1.1", 0)
                ]),
                &threads
            ),
            "the order Slack sends them in is not a change"
        );
        assert!(
            same(json!([channel("C1", "1.3", "1.1", 0)]), &threads),
            "a read conversation is not watched"
        );
        for (moved, why) in [
            (channel("C1", "1.4", "1.1", 0), "a new message"),
            (channel("C1", "1.3", "1.2", 0), "read in the app"),
            (channel("C1", "1.3", "1.1", 1), "a new mention"),
        ] {
            assert!(!same(json!([moved]), &threads), "{why}");
        }
        assert!(
            !same(
                json!([
                    channel("C1", "1.3", "1.1", 0),
                    channel("C2", "2.2", "2.1", 0)
                ]),
                &threads
            ),
            "a conversation that became unread"
        );
        assert!(!same(json!([]), &threads), "a conversation read elsewhere");
        assert!(
            !same(
                json!([channel("C1", "1.3", "1.1", 0)]),
                &json!({"has_unreads": true, "mention_count": 1})
            ),
            "a mention in a thread"
        );
    }
}
