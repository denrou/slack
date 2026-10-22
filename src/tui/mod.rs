//! `slack` with no command: the view as something to move through and
//! answer. One list on the left, what needs you first and then the
//! conversations visited last in Slack, the messages of the selected item
//! on the right (or the thread opened from one of them), a reply box at
//! the bottom. Same model as the text view; this module adds state and
//! keys.
//! API calls stay blocking: each takes a few hundred milliseconds, and the
//! status line says so meanwhile.

mod model;
mod ui;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use jiff::Zoned;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use serde_json::Value;

use self::model::{
    Action, Candidate, Choice, ChoiceKind, Draft, Item, Mark, Mode, Picker, RECENT_ROWS, Section,
    State, ThreadPane, Viewport, Wake,
};
use crate::api::{self, Client};
use crate::config::Config;
use crate::counts::{Conversation, Counts, Kind};
use crate::directory::{self, Listed};
use crate::history::{self, Message};
use crate::names::Names;
use crate::rtm::{self, Posted, Signal};
use crate::search::{self, Found, Hit};
use crate::{
    counts, emoji, fix, lastview, mention, mrkdwn, notify, react, read, replies, target, view, warn,
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
    let recent = recent(client, names, &counts, 0)?;
    let mut state = State::new(Vec::new(), me.map(str::to_string));
    state.replace_items(Some(items), recent);
    // A config that does not read must not keep you from Slack: the box
    // just cannot correct.
    let fix_command = Config::load()
        .unwrap_or_else(|err| {
            warn::warn(format!("{err:#}"));
            Config::default()
        })
        .fix_command;
    state.fixes = fix_command.is_some();
    let live = Arc::new(AtomicBool::new(false));
    let (nudges, nudged) = mpsc::channel();
    let term_program = env::var("TERM_PROGRAM").ok();
    let mut app = App {
        client,
        names,
        team_id,
        workspace_url: workspace_url.trim_end_matches('/'),
        state,
        directory_walked: false,
        pulse: Pulse::of(&counts),
        visited: Visited::of(&counts),
        resting: None,
        polls: poll(client.clone(), Arc::clone(&live), nudged),
        signals: rtm::listen(client.clone(), Arc::clone(&live)),
        nudges,
        live,
        notifies: me.is_some() && notify::supported(term_program.as_deref()),
        focused: true,
        beat: None,
        muted: muted(&counts),
        written: Instant::now(),
        poll_error: None,
        owed: false,
        fix_command,
    };
    app.state.frequent_emoji.clone_from(&counts.frequent_emoji);
    let mut terminal = ratatui::init();
    let modes = Modes::ask();
    let result = app.run(&mut terminal);
    if app.beat.is_some() {
        notify::stop();
    }
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
        // Whether you are looking at the screen, which then notifies of
        // nothing: it shows the message already.
        let _ = execute!(io::stdout(), EnableFocusChange);
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
        let _ = execute!(io::stdout(), DisableFocusChange);
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
    /// Nothing to mark at: no message to mark up to.
    Nothing(String),
    /// An opened conversation, closed without a mark.
    Closed(String),
}

impl Marked {
    /// The status line: the sentence of a single one, the counts of
    /// several.
    fn summary(done: &[Marked]) -> String {
        if let [one] = done {
            return match one {
                Marked::Done(title) => format!("Marked {title} as read"),
                Marked::Already(title) => format!("{title} was already read"),
                Marked::Nothing(title) => format!("{title} has nothing to mark"),
                Marked::Closed(title) => format!("Closed {title}"),
            };
        }
        let read = done
            .iter()
            .filter(|marked| matches!(marked, Marked::Done(_) | Marked::Already(_)))
            .count();
        let closed = done
            .iter()
            .filter(|marked| matches!(marked, Marked::Closed(_)))
            .count();
        match (read, closed) {
            (read, 0) => format!("Marked {read} as read"),
            (0, closed) => format!("Closed {closed}"),
            (read, closed) => format!("Marked {read} as read, closed {closed}"),
        }
    }
}

fn load(client: &Client, names: &mut Names, counts: &Counts) -> Result<Vec<Item>> {
    // The screen reads with context: the recent history, the new part
    // told apart. The text view reads the new part alone.
    let view = view::build(client, counts, names, view::Reach::Recent)?;
    Ok(Item::from_view(view, names))
}

/// The conversations visited last in Slack, as items without their
/// history, which is read once the cursor rests on one. The unread ones
/// are listed above already.
fn recent(client: &Client, names: &mut Names, counts: &Counts, opened: usize) -> Result<Vec<Item>> {
    // Only what the sections can show is named, since Slack keeps dozens:
    // `replace_items` leaves out the opened ones and keeps `RECENT_ROWS`.
    let mut taken = [0; 2];
    let visited: Vec<&Conversation> = counts
        .recent
        .iter()
        .filter_map(|id| counts.conversations.iter().find(|c| &c.id == id))
        .filter(|c| !c.counts_as_unread())
        .filter(|c| {
            let taken = &mut taken[usize::from(c.kind != Kind::Channel)];
            *taken += 1;
            *taken <= RECENT_ROWS + opened
        })
        .collect();
    names.resolve_channels(client, visited.iter().map(|c| c.id.clone()))?;
    let people: Vec<String> = visited
        .iter()
        .filter_map(|c| names.channel(&c.id)?.user.clone())
        .collect();
    names.resolve_users(client, people)?;
    Ok(visited
        .into_iter()
        .map(|c| Item::recent(c, names.channel_label(&c.id)))
        .collect())
}

/// How long the cursor rests on a recent conversation before its history
/// is read: moving through the list with `j` reads none of them.
const PREVIEW_AFTER: Duration = Duration::from_millis(300);
/// The hits of one search: a screenful of lists, and one call.
const SEARCH_HITS: usize = 50;
/// The messages read on each side of a hit when it is opened.
const AROUND_HIT: usize = 25;

/// The longest wait for a key, so the screen redraws and polls.
const TICK: Duration = Duration::from_millis(250);

/// How often the screen asks Slack whether anything moved while the
/// socket is down.
const POLL_EVERY: Duration = Duration::from_secs(15);
/// The slow poll beside a live socket: the endpoint is Slack's legacy
/// one, and an event it stopped sending would otherwise go unnoticed.
const POLL_LIVE: Duration = Duration::from_secs(120);
/// After a nudge, the rest of a burst of events gathers.
const GATHER: Duration = Duration::from_secs(1);
/// The least time between two fetches, however busy the socket.
const MIN_GAP: Duration = Duration::from_secs(5);

/// The counts on a thread of its own, so the screen never stalls on them:
/// `client.counts` and the preferences side by side, where a reload is one
/// call per unread conversation. Fetched when the socket nudges (`true`
/// asks for a reload whatever the counts say), and every `POLL_EVERY`
/// without it. Detached, so a quit never waits on a call in flight; it
/// stops once the screen drops the receiver.
fn poll(client: Client, live: Arc<AtomicBool>, nudges: Receiver<bool>) -> Receiver<Polled> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut last = Instant::now();
        loop {
            let every = if live.load(Ordering::Relaxed) {
                POLL_LIVE
            } else {
                POLL_EVERY
            };
            let mut force = match nudges.recv_timeout(every) {
                Ok(force) => {
                    thread::sleep(GATHER.max(MIN_GAP.saturating_sub(last.elapsed())));
                    force
                }
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            force |= nudges.try_iter().fold(false, |any, force| any | force);
            let began = Instant::now();
            last = began;
            let polled = Polled {
                began,
                counts: counts::fetch(&client),
                force,
            };
            if sender.send(polled).is_err() {
                return;
            }
        }
    });
    receiver
}

struct Polled {
    began: Instant,
    counts: Result<Counts, api::Error>,
    /// Reload even if the counts did not move.
    force: bool,
}

/// What a poll compares to decide on a reload: per unread conversation
/// its newest message, read cursor and mentions, and the threads block.
/// A new reply in a thread already unread moves none of them; the socket
/// asks for that reload itself.
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

/// The conversations muted in Slack, for the notifications' rule.
fn muted(counts: &Counts) -> BTreeSet<String> {
    counts
        .conversations
        .iter()
        .filter(|c| c.muted)
        .map(|c| c.id.clone())
        .collect()
}

/// What the recent sections are built from: the conversations visited
/// last, in order, with what the list shows of each. Apart from `Pulse`,
/// since a change here rebuilds the sections without reading anything.
#[derive(PartialEq, Eq)]
struct Visited(Vec<(String, Option<String>, bool, bool)>);

impl Visited {
    fn of(counts: &Counts) -> Visited {
        Visited(
            counts
                .recent
                .iter()
                .filter_map(|id| counts.conversations.iter().find(|c| &c.id == id))
                .map(|c| (c.id.clone(), c.latest.clone(), c.muted, c.is_unread()))
                .collect(),
        )
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
    /// The recent conversations the list was last built from.
    visited: Visited,
    /// The recent conversation the cursor rests on, and since when.
    resting: Option<(String, Instant)>,
    polls: Receiver<Polled>,
    signals: Receiver<Signal>,
    /// To the poll: fetch now, and reload whatever the counts say if true.
    nudges: Sender<bool>,
    /// When the last mark or reply finished: a poll begun before it shows
    /// what was just read as unread, and is dropped.
    written: Instant,
    /// The status line a failed poll put up, for the next good one to clear.
    poll_error: Option<String>,
    /// A reload the socket asked for and no poll has done yet: a poll that
    /// fails would otherwise drop it, and the counts may never show why.
    owed: bool,
    /// The socket is up: what is aimed at you is heard as it comes.
    live: Arc<AtomicBool>,
    /// The terminal posts notifications and you are known, so the screen
    /// notifies while the socket is up.
    notifies: bool,
    /// The terminal has the focus; a terminal that never says assumes so.
    focused: bool,
    /// When the heartbeat was last written, None while there is none.
    beat: Option<Instant>,
    muted: BTreeSet<String>,
    /// What corrects a draft on Ctrl-G, from the config.
    fix_command: Option<String>,
}

impl App<'_> {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.keep_beat();
            self.heard();
            self.draw(terminal)?;
            let wait = match self.preview_wait() {
                Some(Duration::ZERO) => {
                    if let Some(item) = self.state.to_preview() {
                        self.preview(item, false)?;
                    }
                    continue;
                }
                Some(left) => left.min(TICK),
                None => TICK,
            };
            if !event::poll(wait)? {
                self.polled(terminal)?;
                continue;
            }
            let action = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.state.key(key),
                Event::Paste(text) => self.state.paste(&text),
                Event::FocusGained => {
                    self.focused = true;
                    continue;
                }
                Event::FocusLost => {
                    self.focused = false;
                    continue;
                }
                _ => continue, // a resize redraws on the next pass
            };
            if action == Action::Quit {
                return Ok(());
            }
            self.perform(action, terminal)?;
        }
    }

    /// How long until the history of the selected recent conversation is
    /// read, None when there is nothing to read or a prompt is open.
    fn preview_wait(&mut self) -> Option<Duration> {
        let Some(item) = self.state.to_preview() else {
            self.resting = None;
            return None;
        };
        let channel = &self.state.items[item].channel;
        match &self.resting {
            Some((resting, since)) if resting == channel => {
                let waiting = matches!(self.state.mode, Mode::Browse)
                    && self.state.preview_failed.as_ref() != Some(channel);
                waiting.then(|| PREVIEW_AFTER.saturating_sub(since.elapsed()))
            }
            _ => {
                self.resting = Some((channel.clone(), Instant::now()));
                self.state.preview_failed = None;
                Some(PREVIEW_AFTER)
            }
        }
    }

    /// The recent history of a conversation listed without it. No status
    /// line: the pane says it is loading, and the line keeps what the
    /// last key said.
    fn preview(&mut self, item: usize, enter: bool) -> Result<()> {
        let it = &self.state.items[item];
        let (channel, unread_from) = (it.channel.clone(), it.unread_from.clone());
        match self.history(&channel, unread_from.as_deref()) {
            Ok(page) => {
                self.state.preview(item, page, enter);
                Ok(())
            }
            Err(err) => {
                self.state.preview_failed = Some(channel);
                self.fail(err)
            }
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
        let writes = matches!(action, Action::MarkRead(_) | Action::Send { .. });
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
            Action::MarkRead(marks) => {
                self.busy(terminal, "Marking as read…")?;
                let cursors = match self.cursors() {
                    Ok(cursors) => cursors,
                    Err(err) => return self.fail(err),
                };
                let mut done = Vec::new();
                // Last to first, so an index stays valid after a removal.
                // Each one leaves the selection as it is done, so after a
                // failure the selection holds what is left.
                for mark in marks.iter().rev() {
                    let result = if self.state.items[mark.item].section == Section::Opened {
                        // Closing is not reading: an unread conversation is
                        // listed in its section too, where `e` marks it.
                        Marked::Closed(self.state.items[mark.item].title.clone())
                    } else {
                        match self.mark_read(mark.item, mark.upto.as_deref(), &cursors) {
                            Ok(result) => result,
                            Err(err) => {
                                self.fail(err)?;
                                if !done.is_empty()
                                    && let Some(error) = self.state.status.take()
                                {
                                    let progress = Marked::summary(&done);
                                    self.state.status = Some(format!("{progress}, then {error}"));
                                }
                                return Ok(());
                            }
                        }
                    };
                    self.after_mark(mark, &result);
                    done.push(result);
                }
                self.state.status = Some(Marked::summary(&done));
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
                            typed: None,
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
                    Ok(name) => {
                        if let Some(me) = self.state.me.clone() {
                            self.state.apply_reaction(&channel, &ts, &me, &name, add);
                        }
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
            Action::Preview { item, enter } => self.preview(item, enter),
            Action::Show { choice, compose } => {
                self.busy(terminal, &format!("Loading {}…", choice.label))?;
                match self.show(&choice) {
                    Ok(item) => self.opened(item, compose),
                    Err(err) => self.fail(err),
                }
            }
            Action::Fix { text } => {
                let Some(command) = self.fix_command.clone() else {
                    return Ok(());
                };
                self.busy(terminal, "Correcting…")?;
                match fix::run(&command, &text) {
                    Ok(fixed) => {
                        self.state.fixed(text, fixed);
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::Search { query } => {
                self.busy(terminal, "Searching…")?;
                match self.search(&query) {
                    Ok(found) => {
                        self.state.status = found.hits.is_empty().then(|| "Nothing matches".into());
                        self.state.found(query, found);
                        Ok(())
                    }
                    Err(err) => self.fail(err),
                }
            }
            Action::ShowHit(hit) => {
                let label = self.names.channel_label(&hit.channel);
                self.busy(terminal, &format!("Loading {label}…"))?;
                match self.show_hit(&hit) {
                    Ok(()) => {
                        self.state.status = None;
                        Ok(())
                    }
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

    /// What the socket said since the last pass: applied to the screen
    /// when it can be, else passed on to the poll as a nudge.
    fn heard(&mut self) {
        let now = Instant::now();
        self.state.forget_typing(now);
        let mut wake = Wake::Nothing;
        let signals: Vec<Signal> = self.signals.try_iter().collect();
        for signal in signals {
            let asked = match signal {
                Signal::Up { again: true } => Wake::Reload,
                Signal::Up { again: false } | Signal::Down => Wake::Check,
                Signal::Off(reason) => {
                    self.state.status = Some(format!(
                        "Live updates are off ({reason}); checking Slack every {} seconds",
                        POLL_EVERY.as_secs()
                    ));
                    Wake::Nothing
                }
                Signal::Event(event) => {
                    if let rtm::Event::Message(posted) = &event {
                        self.notify(posted);
                    }
                    self.state.hear(event, now)
                }
            };
            wake = wake.max(asked);
        }
        if wake != Wake::Nothing {
            // The poll outlives no screen: an error here means quitting.
            let _ = self.nudges.send(wake == Wake::Reload);
        }
    }

    /// The heartbeat follows the socket: written while it is up, removed
    /// once it drops, so the fallback takes over at once.
    fn keep_beat(&mut self) {
        if !self.notifies {
            return;
        }
        let live = self.live.load(Ordering::Relaxed);
        match self.beat {
            Some(at) if live && at.elapsed() < notify::BEAT_EVERY => {}
            // Deliberate: a heartbeat that cannot be written leaves the
            // fallback notifying too, and twice is better than never.
            _ if live => self.beat = notify::beat().ok().map(|()| Instant::now()),
            Some(_) => {
                notify::stop();
                self.beat = None;
            }
            None => {}
        }
    }

    /// A notification for a message aimed at you while you are elsewhere.
    /// The names it needs are looked up on a miss; a lookup that fails
    /// leaves an id, and the poll reports a dead session.
    fn notify(&mut self, posted: &Posted) {
        let Some(me) = self.state.me.as_deref() else {
            return;
        };
        if !self.notifies || self.focused || posted.changed || posted.user.as_deref() == Some(me) {
            return;
        }
        let _ = self
            .names
            .resolve_channels(self.client, [posted.channel.clone()]);
        let channel = self.names.channel(&posted.channel);
        let im = channel.is_some_and(|c| c.is_im);
        let direct = im || channel.is_some_and(|c| c.is_mpim);
        let muted = self.muted.contains(&posted.channel);
        if !notify::aimed(posted, me, direct, muted) {
            return;
        }
        let text = posted.text.as_deref().unwrap_or_default();
        let mut people = view::mentioned_users(text);
        people.extend(posted.user.clone());
        if let Some(channel) = channel.and_then(|c| c.user.clone()) {
            people.push(channel);
        }
        let _ = self.names.resolve_users(self.client, people);
        let names = &*self.names;
        let text = mrkdwn::strip(text, &|id| names.user_name(id), &|id| {
            names.channel_name(id)
        });
        let sender = posted
            .user
            .as_deref()
            .filter(|_| !im)
            .map(|user| names.user_name(user).unwrap_or_else(|| user.to_string()));
        let note = notify::Note::new(
            &names.channel_label(&posted.channel),
            sender.as_deref(),
            posted.reply_to().is_some(),
            &text,
        );
        let mut stdout = io::stdout();
        let _ = stdout
            .write_all(note.sequence().as_bytes())
            .and_then(|()| stdout.flush());
    }

    /// The latest poll, between keys and only while nothing is open over
    /// the list: a reload under the reply box or a prompt would move what
    /// it acts on. It waits in the channel until then.
    fn polled(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        if !matches!(self.state.mode, Mode::Browse | Mode::Help) {
            return Ok(());
        }
        let mut force = false;
        let mut latest = None;
        for polled in self.polls.try_iter() {
            force |= polled.force;
            latest = Some(polled);
        }
        let Some(Polled { began, counts, .. }) = latest else {
            return Ok(());
        };
        // Held until a reload does it, whichever way this poll ends.
        force |= self.owed;
        self.owed = force;
        if began < self.written {
            // What the socket asked for still holds.
            if force {
                let _ = self.nudges.send(true);
            }
            return Ok(());
        }
        let counts = match counts {
            Ok(counts) => counts,
            Err(err) => {
                let err = anyhow::Error::from(err).context("could not check for new messages");
                let failed = self.fail(err);
                self.poll_error.clone_from(&self.state.status);
                return failed;
            }
        };
        if let Some(error) = self.poll_error.take()
            && self.state.status.as_ref() == Some(&error)
        {
            self.state.status = None;
        }
        // Without the preferences every muted conversation would show for
        // one poll; the next one tries again.
        if counts.prefs_missing {
            return Ok(());
        }
        self.muted = muted(&counts);
        if !force && Pulse::of(&counts) == self.pulse {
            return self.revisited(&counts);
        }
        // The status line was for the last key, not for the refresh.
        let status = self.state.status.take();
        self.busy(terminal, "Refreshing…")?;
        match self.reload(&counts) {
            Ok(()) => {
                // Unless a warning took the line during the reload.
                if self.state.status.as_deref() == Some("Refreshing…") {
                    self.state.status = status;
                }
                Ok(())
            }
            Err(err) => self.fail(err),
        }
    }

    /// The recent sections rebuilt when they moved and nothing unread
    /// did: no history is read, only a name now and then.
    fn revisited(&mut self, counts: &Counts) -> Result<()> {
        let visited = Visited::of(counts);
        if visited == self.visited {
            return Ok(());
        }
        match recent(self.client, self.names, counts, self.state.opened()) {
            Ok(recent) => {
                self.state.replace_items(None, recent);
                self.visited = visited;
                Ok(())
            }
            Err(err) => self.fail(err),
        }
    }

    /// The list from these counts, and the open thread with it. The list
    /// is loaded once the thread fails: the next poll must not redo it.
    fn reload(&mut self, counts: &Counts) -> Result<()> {
        let items = load(self.client, self.names, counts)?;
        let recent = recent(self.client, self.names, counts, self.state.opened())?;
        self.state.replace_items(Some(items), recent);
        self.state.frequent_emoji.clone_from(&counts.frequent_emoji);
        self.pulse = Pulse::of(counts);
        self.visited = Visited::of(counts);
        if !counts.prefs_missing {
            self.muted = muted(counts);
        }
        self.owed = false;
        self.state.preview_failed = None;
        if let Some(pane) = self.state.thread.take() {
            let reloaded = self
                .reload_thread(&pane)
                .context("could not reload the open thread");
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

    fn mark_read(
        &mut self,
        index: usize,
        upto: Option<&str>,
        cursors: &BTreeMap<String, String>,
    ) -> Result<Marked> {
        let item = &self.state.items[index];
        let title = item.title.clone();
        let Some(latest) = upto else {
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

    /// What needed you leaves the list once read, and an opened
    /// conversation is closed. One with a message newer than the mark
    /// stays, for the next refresh to show what is left; one with
    /// nothing to mark stays as it was, and so does a recent one, read.
    fn after_mark(&mut self, mark: &Mark, result: &Marked) {
        self.state.uncheck(mark.item);
        let newer = self.state.items[mark.item].latest != mark.upto;
        let recent = self.state.items[mark.item].section.is_recent();
        match result {
            Marked::Nothing(_) => {}
            Marked::Done(_) | Marked::Already(_) if recent => self.state.read_in_place(mark.item),
            Marked::Done(_) | Marked::Already(_) if newer => {}
            _ => self.state.remove(mark.item),
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
    /// the reply, so an unread one leaves, a thread open from one of its
    /// messages with it, and an opened or recent one is reloaded. In a
    /// thread: an open thread is reloaded to show the reply; a reply to a
    /// message from the pane leaves the conversation listed, since it
    /// stays unread in Slack, and reloads an opened or recent one.
    fn after_send(&mut self, item: usize, title: &str, thread_ts: Option<&str>) -> Result<()> {
        let section = self.state.items[item].section;
        let browsed = section == Section::Opened || section.is_recent();
        if thread_ts.is_none() {
            self.state.thread = None;
            if !browsed {
                self.state.remove(item);
                self.state.status = Some(format!("Sent to {title}"));
                return Ok(());
            }
            self.state.items[item].unread_from = None;
            return self.reload_browsed(item);
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
        if browsed {
            return self.reload_browsed(item);
        }
        if self.state.items[item].section != Section::Threads {
            self.state.status = Some("Sent in the thread".to_string());
            return Ok(());
        }
        self.state.remove(item);
        self.state.status = Some(format!("Sent to {title}"));
        Ok(())
    }

    /// An opened or recent conversation read again after a reply, in
    /// place, landing on its newest message.
    fn reload_browsed(&mut self, item: usize) -> Result<()> {
        let it = &self.state.items[item];
        let (channel, unread_from) = (it.channel.clone(), it.unread_from.clone());
        match self.history(&channel, unread_from.as_deref()) {
            Ok(page) => {
                self.state.preview(item, page, true);
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
        self.thread_pane(channel, thread_ts, message)
    }

    /// A thread read whole for the pane; `from_message` is where `h` puts
    /// the cursor back in the conversation.
    fn thread_pane(
        &mut self,
        channel: String,
        thread_ts: String,
        from_message: usize,
    ) -> Result<ThreadPane> {
        let fetched = replies::fetch(self.client, &channel, &thread_ts)?;
        self.resolve_people(std::iter::once(&fetched.root).chain(&fetched.replies))?;
        let user = |id: &str| self.names.user_name(id);
        let chan = |id: &str| self.names.channel_name(id);
        let root = mrkdwn::strip(&fetched.root.text, &user, &chan);
        let title = format!(
            "{} › {}",
            self.names.channel_label(&channel),
            view::excerpt(&root, 40)
        );
        let has_more = fetched.has_more;
        Ok(ThreadPane {
            channel,
            thread_ts,
            title,
            messages: fetched.into_messages(),
            has_more,
            from_message,
        })
    }

    fn reload_thread(&mut self, pane: &ThreadPane) -> Result<ThreadPane> {
        let fetched = replies::fetch(self.client, &pane.channel, &pane.thread_ts)?;
        self.resolve_people(std::iter::once(&fetched.root).chain(&fetched.replies))?;
        Ok(ThreadPane {
            channel: pane.channel.clone(),
            thread_ts: pane.thread_ts.clone(),
            title: pane.title.clone(),
            has_more: fetched.has_more,
            messages: fetched.into_messages(),
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

    /// The conversation is in the list; `c` goes on to write to it.
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
        let page = self.history(&channel, unread_from.as_deref())?;
        self.names
            .resolve_channels(self.client, [channel.clone()])?;
        let title = self.names.channel_label(&channel);
        Ok(Item::opened(channel, title, page, unread_from))
    }

    /// The recent history of a conversation, its people named.
    fn history(&mut self, channel: &str, unread_from: Option<&str>) -> Result<history::Unread> {
        let page = view::recent(self.client, channel, unread_from)?;
        self.resolve_people(page.messages.iter())?;
        Ok(page)
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

    /// What Slack finds for the query, its places and people named.
    fn search(&mut self, query: &str) -> Result<Found> {
        let found = search::fetch(self.client, query, SEARCH_HITS)?;
        search::name(self.client, self.names, &found.hits)?;
        Ok(found)
    }

    /// The hit's conversation in the list, read around it with the cursor
    /// on it; a reply opens its thread on top, the cursor on the reply.
    fn show_hit(&mut self, hit: &Hit) -> Result<()> {
        let anchor = hit.anchor().to_string();
        let page = history::around(self.client, &hit.channel, &anchor, AROUND_HIT)?;
        self.resolve_people(page.messages.iter())?;
        self.names
            .resolve_channels(self.client, [hit.channel.clone()])?;
        let title = self.names.channel_label(&hit.channel);
        self.state
            .open_item(Item::around(hit.channel.clone(), title, page));
        self.state.highlight(&anchor);
        if hit.message.is_reply() {
            let pane = self.thread_pane(hit.channel.clone(), anchor, self.state.cursor)?;
            self.state.open_thread(pane);
            self.state.highlight(&hit.message.ts);
        }
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

    #[test]
    fn a_mark_says_what_became_of_it() {
        let done = |title: &str| Marked::Done(title.to_string());
        assert_eq!(
            Marked::summary(&[Marked::Closed("#ops".into())]),
            "Closed #ops"
        );
        assert_eq!(
            Marked::summary(&[
                done("a"),
                Marked::Already("b".into()),
                Marked::Closed("c".into())
            ]),
            "Marked 2 as read, closed 1",
            "what was already read left the list too"
        );
        assert_eq!(
            Marked::summary(&[done("a"), Marked::Nothing("b".into())]),
            "Marked 1 as read"
        );
    }

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
