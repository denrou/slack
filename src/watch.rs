//! `slack watch`: listen on Slack's socket and, for each direct message
//! or mention of you, hand the exchange to a command as JSON on stdin and
//! log what it would answer. A dry run only: nothing is ever sent, the
//! command gets no access to Slack, and what it prints is logged as the
//! reply that would have gone out, with the marker the send would add.
//! Your own messages in that place afterwards are logged beside it, so
//! the log pairs what the command proposed with what you wrote.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, Zoned};
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{self, Client, Identity};
use crate::directory::SLACKBOT;
use crate::history::{self, Message};
use crate::names::Names;
use crate::rtm::{self, Posted, Signal};
use crate::style::Style;
use crate::{mrkdwn, paths, prefs, presence, replies, view};

/// What every reply the watch would send starts with: who reads it must
/// see it is not you, and the prompt cannot leave it out.
pub const MARKER: &str = "🤖 Réponse automatique :";
/// Messages of the conversation or thread handed to the command, the
/// triggering ones last.
const CONTEXT: usize = 20;
/// Your messages in a place are logged as your reply for this long after
/// the event.
const FOLLOW: Duration = Duration::from_secs(12 * 3600);
/// A reply longer than this is refused: an answer is a few lines, and a
/// long one is more likely a prompt gone wrong than a good answer.
const MAX_REPLY: usize = 2000;
/// How much of the command's stderr the log keeps, its end.
const STDERR_KEPT: usize = 2000;

pub struct Options {
    /// Quiet time after the last message of a burst before it is handed
    /// over, so three lines typed in a row are one event.
    pub settle: Duration,
    pub timeout: Duration,
    /// The program and its arguments; none only logs the events.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    DirectMessage,
    Mention,
}

/// Why this message is one for the command: a direct message, or your
/// name in it (`<@you>`, never `@here`, `@channel` or a group). Your own
/// messages and Slackbot's never are.
pub fn reason(posted: &Posted, me: &str) -> Option<Reason> {
    if posted.user == me || posted.user == SLACKBOT {
        return None;
    }
    // A direct message's id starts with D, a group message's does not.
    if posted.channel.starts_with('D') {
        return Some(Reason::DirectMessage);
    }
    history::mentions(&posted.text, me).then_some(Reason::Mention)
}

/// Where an exchange happens: a conversation, and the thread when in one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub channel: String,
    pub thread_ts: Option<String>,
}

impl Place {
    fn of(posted: &Posted) -> Place {
        Place {
            channel: posted.channel.clone(),
            thread_ts: posted.reply_to().map(str::to_string),
        }
    }
}

/// A burst from one person in one place, waiting for them to stop typing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub place: Place,
    pub sender: String,
    pub reason: Reason,
    pub team: Option<String>,
    /// The messages that triggered it, oldest first.
    pub triggers: Vec<String>,
    /// Its newest message, where the context stops: a follow-up without
    /// your name is not a trigger but is part of what they asked.
    pub latest: String,
    last: Instant,
}

/// Whether your message answers an exchange in this place: in its
/// thread, or beside it in a direct or group message.
fn answers(place: &Place, triggers: &[String], conversational: bool, posted: &Posted) -> bool {
    let reply_to = posted.reply_to();
    place.channel == posted.channel
        && (reply_to.is_some_and(|root| triggers.iter().any(|ts| ts == root))
            || (reply_to == place.thread_ts.as_deref() && (conversational || reply_to.is_some())))
}

/// An event handed over, followed for your replies.
struct Fired {
    id: String,
    place: Place,
    triggers: Vec<String>,
    /// A direct or group message, where a reply outside the thread is
    /// still a reply; in a channel only the thread is.
    conversational: bool,
    at: Instant,
}

/// One of your messages after an event, in its place.
#[derive(Debug, PartialEq, Eq)]
pub struct YourReply {
    pub event: String,
    pub ts: String,
    pub text: String,
}

/// What one of your messages is to the watch.
#[derive(Debug, PartialEq, Eq)]
pub enum Heard {
    /// Your reply to an event handed over there.
    Reply(YourReply),
    /// You answered these bursts before they settled, so they are not
    /// handed over: you were there.
    First {
        bursts: Vec<Pending>,
        ts: String,
        text: String,
    },
}

/// What the socket said, sorted into bursts and replies. No I/O.
pub struct Watcher {
    me: String,
    settle: Duration,
    pending: Vec<Pending>,
    fired: Vec<Fired>,
}

impl Watcher {
    pub fn new(me: &str, settle: Duration) -> Watcher {
        Watcher {
            me: me.to_string(),
            settle,
            pending: Vec::new(),
            fired: Vec::new(),
        }
    }

    /// Take a message in: a trigger starts or extends a burst, a message
    /// from the same person in the same place extends one, and yours
    /// answers the bursts still waiting there or, failing that, is your
    /// reply to an event there, if one was handed over lately.
    pub fn hear(&mut self, posted: &Posted, now: Instant) -> Option<Heard> {
        if posted.user == self.me {
            // A group message is only known as one once its names are read:
            // until then, only a DM's main line counts as beside a burst.
            let (answered, waiting): (Vec<Pending>, Vec<Pending>) =
                std::mem::take(&mut self.pending)
                    .into_iter()
                    .partition(|pending| {
                        answers(
                            &pending.place,
                            &pending.triggers,
                            pending.place.channel.starts_with('D'),
                            posted,
                        )
                    });
            self.pending = waiting;
            if !answered.is_empty() {
                return Some(Heard::First {
                    bursts: answered,
                    ts: posted.ts.clone(),
                    text: posted.text.clone(),
                });
            }
            self.fired
                .retain(|fired| now.duration_since(fired.at) < FOLLOW);
            let fired = self.fired.iter().rev().find(|fired| {
                answers(&fired.place, &fired.triggers, fired.conversational, posted)
            })?;
            return Some(Heard::Reply(YourReply {
                event: fired.id.clone(),
                ts: posted.ts.clone(),
                text: posted.text.clone(),
            }));
        }
        let place = Place::of(posted);
        let burst = self
            .pending
            .iter_mut()
            .find(|pending| pending.place == place && pending.sender == posted.user);
        match (burst, reason(posted, &self.me)) {
            (Some(burst), reason) => {
                burst.last = now;
                burst.latest = posted.ts.clone();
                if reason.is_some() {
                    burst.triggers.push(posted.ts.clone());
                }
            }
            (None, Some(reason)) => self.pending.push(Pending {
                place,
                sender: posted.user.clone(),
                reason,
                team: posted.team.clone(),
                triggers: vec![posted.ts.clone()],
                latest: posted.ts.clone(),
                last: now,
            }),
            (None, None) => {}
        }
        None
    }

    /// The bursts quiet for `settle`, taken out, oldest first.
    pub fn due(&mut self, now: Instant) -> Vec<Pending> {
        let (due, waiting) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| now.duration_since(pending.last) >= self.settle);
        self.pending = waiting;
        due
    }

    /// Follow this event for your replies.
    fn handed_over(&mut self, id: &str, pending: &Pending, conversational: bool, now: Instant) {
        // A newer event in the same place takes the replies over.
        self.fired.retain(|fired| fired.place != pending.place);
        self.fired.push(Fired {
            id: id.to_string(),
            place: pending.place.clone(),
            triggers: pending.triggers.clone(),
            conversational,
            at: now,
        });
    }
}

/// Listen until the session dies or the process is stopped.
pub fn run(client: &Client, identity: &Identity, options: &Options) -> Result<()> {
    let log_path = log_path()?;
    let mut log = Log::open(&log_path)?;
    eprintln!(
        "slack watch: listening as {} on {}, dry run (nothing is sent), log in {}",
        identity.user,
        identity.team,
        log_path.display()
    );
    let signals = rtm::listen(client.clone());
    let mut watcher = Watcher::new(&identity.user_id, options.settle);
    loop {
        match signals.recv_timeout(Duration::from_secs(1)) {
            Ok(Signal::Message(posted)) => match watcher.hear(&posted, Instant::now()) {
                Some(Heard::Reply(reply)) => log.write(&json!({
                    "kind": "your_reply",
                    "event": reply.event,
                    "ts": reply.ts,
                    "text": reply.text,
                })),
                Some(Heard::First { bursts, ts, text }) => {
                    for burst in bursts {
                        println!(
                            "{} answered by you before it settled, in {}",
                            Zoned::now().strftime("%H:%M"),
                            burst.place.channel
                        );
                        log.write(&json!({
                            "kind": "answered_first",
                            "channel": burst.place.channel,
                            "thread_ts": burst.place.thread_ts,
                            "sender": burst.sender,
                            "triggers": burst.triggers,
                            "ts": ts,
                            "text": text,
                        }));
                    }
                }
                None => {}
            },
            Ok(Signal::Up { again: false }) => eprintln!("slack watch: connected"),
            Ok(Signal::Up { again: true }) => {
                eprintln!("slack watch: connected again; what came meanwhile was not heard");
                log.write(&json!({
                    "kind": "gap",
                    "reason": "the socket dropped; what came while it was down was not heard",
                }));
            }
            Ok(Signal::Down) => eprintln!("slack watch: the connection dropped, opening again"),
            Ok(Signal::Off(reason)) => {
                eprintln!("slack watch: cannot connect ({reason}), trying on")
            }
            Ok(Signal::Dead(reason)) => bail!("{reason}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the socket listener stopped"),
        }
        for pending in watcher.due(Instant::now()) {
            match hand_over(client, identity, options, &pending) {
                Ok(Some(handed)) => {
                    watcher.handed_over(
                        &handed.id,
                        &pending,
                        handed.conversational,
                        Instant::now(),
                    );
                    println!("{}", handed.summary);
                    log.write(&handed.record);
                }
                Ok(None) => {}
                Err(err @ api::Error::Auth { .. }) => return Err(err.into()),
                Err(err) => {
                    eprintln!("slack watch: could not read the exchange: {err}");
                    log.write(&json!({
                        "kind": "error",
                        "channel": pending.place.channel,
                        "triggers": pending.triggers,
                        "error": err.to_string(),
                    }));
                }
            }
        }
    }
}

/// An event handed to the command, and what came of it.
struct Handed {
    id: String,
    conversational: bool,
    record: Value,
    summary: String,
}

/// Read the exchange around a burst, hand it to the command, and say
/// what came of it; None when it is not one to hand over after all (a
/// muted DM, an app writing as a user).
fn hand_over(
    client: &Client,
    identity: &Identity,
    options: &Options,
    pending: &Pending,
) -> Result<Option<Handed>, api::Error> {
    let channel = &pending.place.channel;
    if pending.reason == Reason::DirectMessage
        && prefs::fetch(client)?.is_some_and(|prefs| prefs.muted.contains(channel))
    {
        return Ok(None);
    }
    let mut names = Names::load();
    let built = build_event(client, identity, &mut names, pending);
    names.save();
    let Some((event, conversational)) = built? else {
        return Ok(None);
    };
    let started = Instant::now();
    let outcome = if options.command.is_empty() {
        None
    } else {
        Some(match serde_json::to_vec(&event) {
            Ok(input) => run_command(&options.command, &input, options.timeout),
            Err(err) => Outcome::Failed {
                error: format!("could not write the event: {err}"),
                stderr: String::new(),
            },
        })
    };
    let mut record = json!({
        "kind": "event",
        "id": event["id"],
        "event": event,
    });
    let said = match &outcome {
        None => "logged".to_string(),
        Some(outcome) => {
            record["seconds"] = json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
            outcome.record(&mut record);
            outcome.summary()
        }
    };
    let id = event["id"].as_str().unwrap_or_default().to_string();
    let summary = format!(
        "{} {} from {} in {}: {said}",
        Zoned::now().strftime("%H:%M"),
        match pending.reason {
            Reason::DirectMessage => "message",
            Reason::Mention => "mention",
        },
        event["sender"]["name"].as_str().unwrap_or(&pending.sender),
        event["conversation"]["name"].as_str().unwrap_or(channel),
    );
    Ok(Some(Handed {
        id,
        conversational,
        record,
        summary,
    }))
}

/// The JSON the command reads, and whether the place is a direct or
/// group message. None when the sender turns out to be an app.
fn build_event(
    client: &Client,
    identity: &Identity,
    names: &mut Names,
    pending: &Pending,
) -> Result<Option<(Value, bool)>, api::Error> {
    let channel = &pending.place.channel;
    let me = &identity.user_id;
    let last = pending
        .triggers
        .last()
        .map(String::as_str)
        .unwrap_or_default();
    let messages = context(client, &pending.place, &pending.latest)?;
    let mut people = BTreeSet::from([pending.sender.clone(), me.clone()]);
    let mut places = BTreeSet::from([channel.clone()]);
    for message in &messages {
        view::collect_names(&mut people, &mut places, message);
    }
    names.resolve_channels(client, places)?;
    names.resolve_users(client, people)?;
    if names.user(&pending.sender).is_some_and(|user| user.is_bot) {
        return Ok(None);
    }
    let now = Zoned::now();
    // Deliberate: a failed read fails the event, logged as an error, since
    // an empty profile would read as notifications not paused and no clock.
    let people = presence::people(client, &[pending.sender.clone(), me.clone()])?;
    let describe = |id: &str| {
        let (handle, name) = names.user(id).map_or_else(
            || (id.to_string(), id.to_string()),
            |user| (user.name.clone(), user.display.clone()),
        );
        let person = people.get(id).cloned().unwrap_or_default();
        let mut value =
            serde_json::to_value(person.described(id, &handle, &name, &now)).unwrap_or_default();
        if let Some(real) = names
            .user(id)
            .map(|user| &user.real)
            .filter(|r| !r.is_empty())
        {
            value["real_name"] = json!(real);
        }
        value
    };
    let mut sender = describe(&pending.sender);
    sender["external"] = json!(external(pending.team.as_deref(), &identity.team_id));
    let found = names.channel(channel);
    let conversational = channel.starts_with('D') || found.is_some_and(|c| c.is_mpim);
    let kind = match found {
        _ if channel.starts_with('D') => "direct_message",
        Some(c) if c.is_mpim => "group_message",
        Some(c) if c.is_private => "private_channel",
        _ => "channel",
    };
    let permalink = client
        .call(
            "chat.getPermalink",
            &[("channel", channel.as_str()), ("message_ts", last)],
        )
        .ok()
        .and_then(|body| body.get("permalink")?.as_str().map(str::to_string));
    let event = json!({
        "id": format!("{channel}-{last}"),
        "reason": pending.reason,
        "workspace": identity.team,
        "conversation": {
            "id": channel,
            "kind": kind,
            "name": names.channel_label(channel),
        },
        "thread_ts": pending.place.thread_ts,
        "permalink": permalink,
        "sender": sender,
        "me": describe(me),
        "messages": shown_messages(&messages, &pending.triggers, me, names, &now),
    });
    Ok(Some((event, conversational)))
}

/// Whether the sender is from another organisation. A frame without a
/// team says so too: what is not known to be a colleague is not one.
fn external(team: Option<&str>, mine: &str) -> bool {
    team != Some(mine)
}

/// The conversation or thread up to the burst's newest message: what the
/// sender saw when they wrote, never what came after, your answer
/// included.
fn context(client: &Client, place: &Place, last: &str) -> Result<Vec<Message>, api::Error> {
    let mut messages = match &place.thread_ts {
        Some(root) => {
            let thread = replies::fetch(client, &place.channel, root)?;
            let mut messages = vec![thread.root];
            messages.extend(thread.replies);
            messages
        }
        None => {
            let limit = CONTEXT.to_string();
            let body = client.call(
                "conversations.history",
                &[
                    ("channel", place.channel.as_str()),
                    ("latest", last),
                    ("inclusive", "true"),
                    ("limit", &limit),
                ],
            )?;
            history::parse(&body, None)?.messages
        }
    };
    messages.retain(|message| history::compare_ts(&message.ts, last).is_le());
    if messages.len() > CONTEXT {
        // A thread keeps its root, the question it answers.
        let keep_root = usize::from(place.thread_ts.is_some());
        messages.drain(keep_root..messages.len() - (CONTEXT - keep_root));
    }
    Ok(messages)
}

/// The messages as the command reads them: names instead of ids, plain
/// text with the mentions spelled out, your clock, the triggers flagged.
fn shown_messages(
    messages: &[Message],
    triggers: &[String],
    me: &str,
    names: &Names,
    now: &Zoned,
) -> Vec<Value> {
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    messages
        .iter()
        .map(|message| {
            let author = match (&message.user, &message.bot) {
                (Some(id), _) => names.user_name(id).unwrap_or_else(|| id.clone()),
                (None, Some(bot)) => bot.clone(),
                (None, None) => "unknown".to_string(),
            };
            let mut value = json!({
                "ts": message.ts,
                "time": local_time(&message.ts, now),
                "from": author,
                "text": mrkdwn::render(&message.text, &user, &channel, &Style::plain()),
            });
            if let Some(id) = &message.user {
                value["user"] = json!(id);
            }
            if message.user.as_deref() == Some(me) {
                value["me"] = json!(true);
            }
            if message.bot.is_some() {
                value["bot"] = json!(true);
            }
            if triggers.contains(&message.ts) {
                value["trigger"] = json!(true);
            }
            if !message.files.is_empty() {
                value["files"] = json!(message.files);
            }
            value
        })
        .collect()
}

/// A message's time on your clock, with its offset.
fn local_time(ts: &str, now: &Zoned) -> Option<String> {
    let seconds = ts.split('.').next()?.parse().ok()?;
    let stamp = Timestamp::from_second(seconds).ok()?;
    Some(
        stamp
            .to_zoned(now.time_zone().clone())
            .strftime("%Y-%m-%dT%H:%M:%S%:z")
            .to_string(),
    )
}

/// What the command made of an event.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Reply(String),
    /// It printed nothing: no answer for this one.
    Nothing,
    Failed {
        error: String,
        stderr: String,
    },
}

impl Outcome {
    fn record(&self, record: &mut Value) {
        match self {
            Outcome::Reply(reply) => {
                record["outcome"] = json!("reply");
                record["reply"] = json!(reply);
                record["would_send"] = json!(format!("{MARKER} {reply}"));
            }
            Outcome::Nothing => record["outcome"] = json!("no_reply"),
            Outcome::Failed { error, stderr } => {
                record["outcome"] = json!("failed");
                record["error"] = json!(error);
                if !stderr.is_empty() {
                    record["stderr"] = json!(stderr);
                }
            }
        }
    }

    fn summary(&self) -> String {
        match self {
            Outcome::Reply(reply) => format!("would reply ({} characters)", reply.chars().count()),
            Outcome::Nothing => "no reply".to_string(),
            Outcome::Failed { error, .. } => format!("failed: {error}"),
        }
    }
}

/// Run the command with the event on stdin, and read its answer from
/// stdout. It runs in a process group of its own, so its end, or a
/// timeout, stops what it started too (a `claude` it spawned would hold
/// the pipes open).
pub fn run_command(command: &[String], input: &[u8], timeout: Duration) -> Outcome {
    use std::os::unix::process::CommandExt;

    let failed = |error: String, stderr: String| Outcome::Failed { error, stderr };
    let Some((program, args)) = command.split_first() else {
        return failed("no command".to_string(), String::new());
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => return failed(format!("could not start {program}: {err}"), String::new()),
    };
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return failed(
            "the command's pipes did not open".to_string(),
            String::new(),
        );
    };
    let input = input.to_vec();
    // A command that never reads its stdin is not an error.
    thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let read_all = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let out = read_all(Box::new(stdout));
    let err = read_all(Box::new(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() >= timeout => break Err(()),
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(err) => return failed(format!("could not wait for it: {err}"), String::new()),
        }
    };
    // The whole group, out of time or done: what the command started must
    // not outlive it holding the pipes open, or the reads below never end.
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stderr(Stdio::null())
        .status();
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let stderr = tail(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    match status {
        Err(()) => failed(format!("timed out after {}s", timeout.as_secs()), stderr),
        Ok(status) if !status.success() => failed(format!("it exited with {status}"), stderr),
        Ok(_) => {
            let reply = stdout.trim();
            match reply.chars().count() {
                0 => Outcome::Nothing,
                n if n > MAX_REPLY => failed(
                    format!("a reply of {n} characters, more than {MAX_REPLY}"),
                    stderr,
                ),
                _ => Outcome::Reply(reply.to_string()),
            }
        }
    }
}

/// The end of a text, where the error usually is.
fn tail(text: &str) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= STDERR_KEPT {
        return text.to_string();
    }
    text.chars().skip(count - STDERR_KEPT).collect()
}

pub fn log_path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("watch.jsonl"))
}

/// The log, one JSON record a line, each with the time it was written.
struct Log {
    file: fs::File,
}

impl Log {
    fn open(path: &Path) -> Result<Log> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = paths::append_private(path)
            .with_context(|| format!("opening the watch log {}", path.display()))?;
        Ok(Log { file })
    }

    /// Best effort: a full disk must not stop the watch.
    fn write(&mut self, record: &Value) {
        let mut record = record.clone();
        record["at"] = json!(Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%:z").to_string());
        let mut line = record.to_string();
        line.push('\n');
        if let Err(err) = self.file.write_all(line.as_bytes()) {
            crate::warn::warn(format!("could not write to the watch log: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::{Channel, User};

    const ME: &str = "UME";

    fn posted(channel: &str, user: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> Posted {
        Posted {
            channel: channel.to_string(),
            ts: ts.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            user: user.to_string(),
            team: Some("T1".to_string()),
            text: text.to_string(),
        }
    }

    #[test]
    fn a_dm_or_your_name_is_a_reason_a_broadcast_is_not() {
        let dm = posted("D1", "UJANE", "1.0", None, "Hey Denis, une PR à revoir ?");
        assert_eq!(reason(&dm, ME), Some(Reason::DirectMessage));
        let named = posted("C1", "UJANE", "1.0", None, "<@UME> tu peux regarder ?");
        assert_eq!(reason(&named, ME), Some(Reason::Mention));
        let labelled = posted("C1", "UJANE", "1.0", None, "<@UME|denis> ?");
        assert_eq!(reason(&labelled, ME), Some(Reason::Mention));
        for text in [
            "<!here> deploy at 3",
            "<!channel>",
            "<!subteam^S1|@dev>",
            "<@UMEX> hi",
            "Denis?",
        ] {
            let message = posted("C1", "UJANE", "1.0", None, text);
            assert_eq!(reason(&message, ME), None, "{text}");
        }
        let group = posted("G1", "UJANE", "1.0", None, "salut tout le monde");
        assert_eq!(reason(&group, ME), None, "a group message needs your name");
        let mine = posted("D1", ME, "1.0", None, "<@UME>");
        assert_eq!(reason(&mine, ME), None);
        let slackbot = posted("D1", SLACKBOT, "1.0", None, "reminder");
        assert_eq!(reason(&slackbot, ME), None);
    }

    #[test]
    fn a_burst_is_one_event_once_quiet() {
        let start = Instant::now();
        let settle = Duration::from_secs(60);
        let mut watcher = Watcher::new(ME, settle);
        watcher.hear(&posted("D1", "UJANE", "1.0", None, "Hey"), start);
        watcher.hear(
            &posted("D1", "UJANE", "2.0", None, "tu peux revoir ?"),
            start + Duration::from_secs(30),
        );
        assert!(
            watcher.due(start + Duration::from_secs(80)).is_empty(),
            "still typing"
        );
        let due = watcher.due(start + Duration::from_secs(90));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].triggers, ["1.0", "2.0"]);
        assert!(
            watcher.due(start + Duration::from_secs(200)).is_empty(),
            "taken out"
        );
    }

    #[test]
    fn a_follow_up_without_your_name_extends_the_burst() {
        let start = Instant::now();
        let mut watcher = Watcher::new(ME, Duration::from_secs(60));
        watcher.hear(
            &posted("C1", "UJANE", "1.0", None, "<@UME> une question"),
            start,
        );
        watcher.hear(
            &posted("C1", "UJANE", "2.0", None, "c'est pour la release"),
            start + Duration::from_secs(50),
        );
        watcher.hear(
            &posted("C1", "UBOB", "3.0", None, "unrelated"),
            start + Duration::from_secs(55),
        );
        assert!(watcher.due(start + Duration::from_secs(100)).is_empty());
        let due = watcher.due(start + Duration::from_secs(110));
        assert_eq!(due.len(), 1, "Bob's message is not one");
        assert_eq!(
            due[0].triggers,
            ["1.0"],
            "the follow-up is context, not a trigger"
        );
        assert_eq!(due[0].latest, "2.0", "the context reaches the follow-up");
    }

    #[test]
    fn your_answer_before_a_burst_settles_drops_it() {
        let start = Instant::now();
        let mut watcher = Watcher::new(ME, Duration::from_secs(60));
        watcher.hear(&posted("D1", "UJANE", "1.0", None, "tu peux ?"), start);
        watcher.hear(&posted("C1", "UBOB", "2.0", None, "<@UME> ?"), start);
        assert_eq!(
            watcher.hear(&posted("C1", ME, "2.5", None, "beside, not in it"), start),
            None,
            "in a channel, only the thread answers"
        );
        let heard = watcher.hear(&posted("D1", ME, "3.0", None, "oui je regarde"), start);
        let Some(Heard::First { bursts, ts, .. }) = heard else {
            panic!("{heard:?}");
        };
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].sender, "UJANE");
        assert_eq!(ts, "3.0");
        let in_thread = watcher.hear(&posted("C1", ME, "4.0", Some("2.0"), "oui"), start);
        assert!(matches!(in_thread, Some(Heard::First { .. })));
        assert!(watcher.due(start + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn people_and_threads_are_separate_bursts() {
        let start = Instant::now();
        let mut watcher = Watcher::new(ME, Duration::from_secs(1));
        watcher.hear(&posted("C1", "UJANE", "1.0", None, "<@UME>"), start);
        watcher.hear(&posted("C1", "UBOB", "2.0", None, "<@UME>"), start);
        watcher.hear(&posted("C1", "UJANE", "3.0", Some("1.0"), "<@UME>"), start);
        let due = watcher.due(start + Duration::from_secs(1));
        assert_eq!(due.len(), 3);
        assert_eq!(due[2].place.thread_ts.as_deref(), Some("1.0"));
    }

    #[test]
    fn your_messages_after_an_event_are_your_reply() {
        let start = Instant::now();
        let mut watcher = Watcher::new(ME, Duration::from_secs(1));
        assert_eq!(
            watcher.hear(
                &posted("D1", ME, "0.5", None, "nothing handed over yet"),
                start
            ),
            None
        );

        watcher.hear(&posted("D1", "UJANE", "1.0", None, "Hey"), start);
        let due = watcher.due(start + Duration::from_secs(1));
        watcher.handed_over("D1-1.0", &due[0], true, start);
        let reply = watcher.hear(&posted("D1", ME, "5.0", None, "je regarde"), start);
        assert_eq!(
            reply,
            Some(Heard::Reply(YourReply {
                event: "D1-1.0".to_string(),
                ts: "5.0".to_string(),
                text: "je regarde".to_string(),
            }))
        );

        watcher.hear(&posted("C1", "UJANE", "7.0", None, "<@UME> ?"), start);
        let due = watcher.due(start + Duration::from_secs(1));
        watcher.handed_over("C1-7.0", &due[0], false, start);
        assert_eq!(
            watcher.hear(&posted("C1", ME, "8.0", None, "something else"), start),
            None,
            "in a channel, only the thread is a reply"
        );
        let in_thread = watcher.hear(&posted("C1", ME, "9.0", Some("7.0"), "oui"), start);
        assert!(matches!(in_thread, Some(Heard::Reply(r)) if r.event == "C1-7.0"));

        let late = start + FOLLOW + Duration::from_secs(1);
        assert_eq!(
            watcher.hear(&posted("D1", ME, "10.0", None, "later"), late),
            None
        );
    }

    #[test]
    fn the_command_reads_the_event_and_its_stdout_is_the_reply() {
        let sh = |script: &str| vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        let timeout = Duration::from_secs(5);
        assert_eq!(
            run_command(&sh("read line; echo \"got $line\""), b"{}\n", timeout),
            Outcome::Reply("got {}".to_string())
        );
        assert_eq!(
            run_command(&sh("echo '  '"), b"", timeout),
            Outcome::Nothing
        );
        assert!(matches!(
            run_command(&sh("echo oops >&2; exit 3"), b"", timeout),
            Outcome::Failed { stderr, .. } if stderr == "oops"
        ));
        let long = format!("head -c {} /dev/zero | tr '\\0' x", MAX_REPLY + 1);
        assert!(matches!(
            run_command(&sh(&long), b"", timeout),
            Outcome::Failed { error, .. } if error.contains("more than")
        ));
        assert!(matches!(
            run_command(&["/nonexistent/answer".to_string()], b"", timeout),
            Outcome::Failed { error, .. } if error.starts_with("could not start")
        ));
    }

    #[test]
    fn a_reply_goes_out_behind_the_marker_and_a_failure_prints_none() {
        let mut record = json!({});
        Outcome::Reply("Il est absent.".to_string()).record(&mut record);
        assert_eq!(record["would_send"], format!("{MARKER} Il est absent."));

        let sh = |script: &str| vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        assert!(matches!(
            run_command(
                &sh("echo half a reply; exit 1"),
                b"",
                Duration::from_secs(5)
            ),
            Outcome::Failed { .. }
        ));
    }

    #[test]
    fn what_the_command_leaves_behind_does_not_hold_the_watch() {
        let started = Instant::now();
        // The background `sleep` keeps stdout open after `sh` is done.
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "sleep 30 & echo done".to_string(),
            ],
            b"",
            Duration::from_secs(60),
        );
        assert_eq!(outcome, Outcome::Reply("done".to_string()));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_sender_without_a_team_counts_as_from_elsewhere() {
        assert!(!external(Some("T1"), "T1"));
        assert!(external(Some("T9"), "T1"));
        assert!(external(None, "T1"));
    }

    #[test]
    fn a_timeout_stops_what_the_command_started() {
        let started = Instant::now();
        // The background `sleep` keeps stdout open: only a group kill ends it.
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "sleep 30 & wait".to_string(),
            ],
            b"",
            Duration::from_millis(300),
        );
        assert!(matches!(outcome, Outcome::Failed { error, .. } if error.starts_with("timed out")));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_messages_read_with_names_and_flags() {
        let names = Names::with(
            vec![
                (
                    "UJANE",
                    User {
                        name: "jane".to_string(),
                        display: "Jane".to_string(),
                        ..User::default()
                    },
                ),
                (
                    ME,
                    User {
                        name: "denis".to_string(),
                        display: "Denis".to_string(),
                        ..User::default()
                    },
                ),
            ],
            vec![(
                "C1",
                Channel {
                    name: "dev".to_string(),
                    is_private: false,
                    is_im: false,
                    is_mpim: false,
                    user: None,
                },
            )],
        );
        let message = |ts: &str, user: &str, text: &str| Message {
            ts: ts.to_string(),
            user: Some(user.to_string()),
            bot: None,
            text: text.to_string(),
            subtype: None,
            thread_ts: None,
            reply_count: 0,
            files: Vec::new(),
            attachments: Vec::new(),
            reactions: Vec::new(),
        };
        let messages = [
            message("1700000000.000100", ME, "on est bon pour <#C1>"),
            message(
                "1700000060.000200",
                "UJANE",
                "<@UME> tu peux revoir *cette* PR ?",
            ),
        ];
        let now = Zoned::now();
        let shown = shown_messages(
            &messages,
            &["1700000060.000200".to_string()],
            ME,
            &names,
            &now,
        );
        assert_eq!(shown[0]["from"], "Denis");
        assert_eq!(shown[0]["me"], true);
        assert_eq!(shown[0]["text"], "on est bon pour #dev");
        assert!(shown[0].get("trigger").is_none());
        assert_eq!(shown[1]["text"], "@Denis tu peux revoir *cette* PR ?");
        assert_eq!(shown[1]["trigger"], true);
        assert_eq!(shown[1]["user"], "UJANE");
        assert!(
            shown[1]["time"]
                .as_str()
                .is_some_and(|t| t.starts_with("2023-11-1"))
        );
    }
}
