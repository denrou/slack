//! The glue between the command line and the modules: one function per
//! command, each returning an error the main function prints.

use std::collections::{BTreeSet, HashSet};
use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::api::{self, Client, Credentials, Identity};
use crate::auth::chrome::{self, ClearOutcome};
use crate::auth::{capture, cdp};
use crate::counts::Kind;
use crate::directory::Listed;
use crate::names::Names;
use crate::store::{Workspace, Workspaces};
use crate::style::Style;
use crate::target::parse_permalink;
use crate::view::{MessageList, Refs, Scope};
use crate::{counts, directory, history, lastview, mrkdwn, read, replies, send, target, tui, view};

pub fn view(json: bool, no_color: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let client = workspace.client();
    let mut names = Names::load();
    let counts = counts::fetch(&client)?;
    let view = view::build(&client, &counts, &mut names)?;
    names.save();
    if let Err(err) = lastview::remember(team_id, &view) {
        eprintln!("warning: could not record the view for `slack read`: {err:#}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    let style = Style::detect(no_color);
    to_stdout(|out| view::render(&view, &names, &style, workspace.user_id.as_deref(), out))
}

/// Print through a locked stdout. `slack | head` closes the pipe early;
/// that is not an error.
fn to_stdout(print: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> Result<()> {
    let stdout = io::stdout();
    match print(&mut stdout.lock()) {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

pub fn count() -> Result<()> {
    let store = Workspaces::load()?;
    let (_, workspace) = store.default_workspace()?;
    let counts = counts::fetch(&workspace.client())?;
    println!("{}", counts.unread_count());
    Ok(())
}

pub fn ui() -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let result = tui::run(
        &workspace.client(),
        &mut names,
        team_id,
        workspace.user_id.as_deref(),
        &workspace.url,
    );
    names.save();
    result
}

pub fn send(target: &str, text: Option<&str>, thread: Option<&str>) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let result = send::run(
        &workspace.client(),
        &mut names,
        team_id,
        target,
        text,
        thread,
    );
    names.save();
    result
}

pub fn thread(spec: &str, json: bool, no_color: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let client = workspace.client();
    let mut names = Names::load();
    let (channel, thread_ts) = thread_target(team_id, spec)?;
    let fetched = replies::fetch(&client, &channel, &thread_ts)?;
    names.resolve_channels(&client, [channel.clone()])?;
    let mut people = BTreeSet::new();
    for message in std::iter::once(&fetched.root).chain(&fetched.replies) {
        people.extend(message.user.clone());
        people.extend(view::mentioned_users(&message.text));
    }
    names.resolve_users(&client, people)?;
    names.save();

    if json {
        let value = json!({
            "channel": channel,
            "thread_ts": thread_ts,
            "root": fetched.root,
            "replies": fetched.replies,
            "has_more": fetched.has_more,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let style = Style::detect(no_color);
    let user = |id: &str| names.user_name(id);
    let chan = |id: &str| names.channel_name(id);
    let root = mrkdwn::render(&fetched.root.text, &user, &chan, &Style::plain());
    let count = match fetched.replies.len() {
        1 => "1 reply".to_string(),
        n => format!("{n}{} replies", if fetched.has_more { "+" } else { "" }),
    };
    println!(
        "{} › {}  {}",
        style.bold(&names.channel_label(&channel)),
        view::excerpt(&root, 60),
        style.dim(&format!("({count})"))
    );
    let mut messages = vec![fetched.root];
    messages.extend(fetched.replies);
    let list = MessageList {
        messages: &messages,
        channel: &channel,
        has_more: fetched.has_more,
        show_author: true,
        scope: Scope::Thread,
        unread_from: None,
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let now = jiff::Zoned::now();
    let me = workspace.user_id.as_deref();
    view::render_messages(&list, &Refs::none(), &names, &style, me, &now, &mut out)?;
    let reference = if spec.chars().all(|c| c.is_ascii_digit()) {
        spec.to_string()
    } else {
        format!(
            "{}/archives/{}/p{}",
            workspace.url.trim_end_matches('/'),
            channel,
            thread_ts.replace('.', "")
        )
    };
    writeln!(
        out,
        "{}",
        style.dim(&format!("reply with: slack send {reference} \"…\""))
    )?;
    Ok(())
}

/// A thread number from the last view, or a permalink to any message of
/// the thread.
fn thread_target(team_id: &str, spec: &str) -> Result<(String, String)> {
    if !spec.is_empty() && spec.chars().all(|c| c.is_ascii_digit()) {
        let number: usize = spec.parse().context("not a thread number")?;
        let thread = lastview::thread_ref(team_id, number)?.ok_or_else(|| {
            anyhow::anyhow!("the last view numbered no thread {number}; run `slack` first")
        })?;
        return Ok((thread.channel, thread.ts));
    }
    if let Some(link) = parse_permalink(spec) {
        let thread_ts = link.thread();
        return Ok((link.channel, thread_ts));
    }
    bail!("`{spec}` is neither a thread number from the last `slack` output nor a Slack permalink")
}

pub fn channels(filter: &str, json: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (_, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let listed = directory::conversations(&workspace.client(), &mut names);
    names.save();
    let listed: Vec<Listed> = listed?
        .into_iter()
        .filter(|c| c.kind != Kind::Im && directory::matches(filter, &c.name))
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&listed)?);
        return Ok(());
    }
    if listed.is_empty() {
        if filter.is_empty() {
            println!("You are in no channel.");
        } else {
            println!("No channel matches `{filter}`.");
        }
        return Ok(());
    }
    let style = Style::detect(false);
    let (channels, groups): (Vec<&Listed>, Vec<&Listed>) =
        listed.iter().partition(|c| c.kind == Kind::Channel);
    to_stdout(|out| {
        let mut printed = false;
        for (title, entries) in [("Channels", channels), ("Group messages", groups)] {
            if entries.is_empty() {
                continue;
            }
            if printed {
                writeln!(out)?;
            }
            printed = true;
            writeln!(out, "{}", style.heading(title))?;
            for listed in entries {
                let notes = listed.notes();
                let notes = if notes.is_empty() {
                    String::new()
                } else {
                    format!("  {}", style.dim(&format!("({})", notes.join(", "))))
                };
                writeln!(out, "  {}{notes}", style.bold(&listed.name))?;
            }
        }
        Ok(())
    })
}

pub fn people(filter: &str, json: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (_, workspace) = store.default_workspace()?;
    let client = workspace.client();
    let mut names = Names::load();
    let people = if filter.is_empty() {
        directory::correspondents(&client, &mut names)
    } else {
        directory::people(&client, &mut names, filter)
    };
    names.save();
    let people = people?;
    if json {
        println!("{}", serde_json::to_string_pretty(&people)?);
        return Ok(());
    }
    if people.is_empty() {
        if filter.is_empty() {
            println!("No direct message yet; `slack people <name>` searches the workspace.");
        } else {
            println!("No one matches `{filter}`.");
        }
        return Ok(());
    }
    let style = Style::detect(false);
    let width = people
        .iter()
        .map(|p| p.display.chars().count() + 1)
        .max()
        .unwrap_or(0);
    to_stdout(|out| {
        for person in &people {
            let label = format!("@{}", person.display);
            let padding = " ".repeat(width - label.chars().count());
            let mut rest = Vec::new();
            if person.handle != person.display {
                rest.push(style.dim(&format!("@{}", person.handle)));
            }
            if !person.real.is_empty() && person.real != person.display {
                rest.push(person.real.clone());
            }
            if !person.title.is_empty() {
                rest.push(style.dim(&person.title));
            }
            writeln!(
                out,
                "  {}{padding}  {}",
                style.bold(&label),
                rest.join("  ")
            )?;
        }
        Ok(())
    })
}

pub fn show(spec: &str, limit: usize, json: bool, no_color: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let client = workspace.client();
    let mut names = Names::load();
    let loaded = load_conversation(&client, &mut names, team_id, spec, limit);
    names.save();
    let (channel, page, last_read) = loaded?;
    if json {
        let value = json!({
            "channel": channel,
            "name": names.channel_label(&channel),
            "messages": page.messages,
            "has_more": page.has_more,
            "last_read": last_read,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let refs = Refs::of_messages(&channel, &page.messages);
    let latest = page.messages.last().map(|m| m.ts.as_str());
    let threads = Refs::of_messages(&channel, &page.messages).into_threads();
    if let Err(err) = lastview::remember_conversation(team_id, &channel, latest, threads) {
        eprintln!(
            "warning: could not record the conversation for `slack read` and `slack thread`: {err:#}"
        );
    }
    let style = Style::detect(no_color);
    let new = page
        .messages
        .iter()
        .filter(|m| view::is_new(m, last_read.as_deref()))
        .count();
    let mut count = match page.messages.len() {
        0 => "no messages".to_string(),
        n if page.has_more => format!("last {n} messages"),
        1 => "1 message".to_string(),
        n => format!("{n} messages"),
    };
    if new > 0 {
        count.push_str(&format!(", {new} new"));
    }
    let list = MessageList {
        messages: &page.messages,
        channel: &channel,
        has_more: page.has_more,
        show_author: true,
        scope: Scope::History,
        unread_from: last_read.as_deref(),
    };
    let now = jiff::Zoned::now();
    let me = workspace.user_id.as_deref();
    to_stdout(|out| {
        writeln!(
            out,
            "{}  {}",
            style.bold(&names.channel_label(&channel)),
            style.dim(&format!("({count})"))
        )?;
        if page.messages.is_empty() {
            return Ok(());
        }
        view::render_messages(&list, &refs, &names, &style, me, &now, out)?;
        writeln!(
            out,
            "{}",
            style.dim(&format!(
                "reply with: slack send {} \"…\"",
                send_target(&names, &channel)
            ))
        )
    })
}

/// A conversation argument, its recent messages, and its read cursor
/// (the newest unread messages are the ones after it).
fn load_conversation(
    client: &Client,
    names: &mut Names,
    team_id: &str,
    spec: &str,
    limit: usize,
) -> Result<(String, history::Unread, Option<String>)> {
    let target = target::resolve(client, names, team_id, spec)?;
    if target.thread_ts.is_some() {
        bail!(
            "`slack show` takes a #channel, an @person or a conversation id; `slack thread` shows \
             a thread"
        );
    }
    let channel = target.channel;
    let (page, info) = std::thread::scope(|scope| {
        let info = scope.spawn(|| client.call("conversations.info", &[("channel", &channel)]));
        let page = history::fetch(client, &channel, None, limit);
        // A panic in the info thread is a bug, not a condition to handle.
        (page, info.join().expect("the info thread panicked"))
    });
    let page = page?;
    let last_read = info?
        .pointer("/channel/last_read")
        .and_then(Value::as_str)
        .filter(|cursor| history::is_usable_cursor(cursor))
        .map(str::to_string);
    names.resolve_channels(client, [channel.clone()])?;
    let mut people = BTreeSet::new();
    people.extend(names.channel(&channel).and_then(|c| c.user.clone()));
    for message in &page.messages {
        people.extend(message.user.clone());
        people.extend(view::mentioned_users(&message.text));
    }
    names.resolve_users(client, people)?;
    Ok((channel, page, last_read))
}

/// The conversation as a command-line target: `'#name'` (quoted, since
/// zsh reads `#` as a comment), `@person`, or the id of a group message,
/// which has no name to type.
fn send_target(names: &Names, channel: &str) -> String {
    match names.channel(channel) {
        Some(c) if c.is_im => names.channel_label(channel),
        Some(c) if !c.is_mpim && !c.name.is_empty() => format!("'#{}'", c.name),
        _ => channel.to_string(),
    }
}

pub fn read(targets: &[String]) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let result = read::run(&workspace.client(), &mut names, team_id, targets);
    names.save();
    result
}

pub fn whoami(json: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    match workspace.client().auth_test() {
        Ok(identity) => {
            if json {
                let mut value = serde_json::to_value(&identity)?;
                value["status"] = json!("ok");
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{} ({})", identity.team, identity.team_id);
                println!("  User: {} ({})", identity.user, identity.user_id);
                println!("  URL: {}", identity.url);
                println!("  Status: verified");
            }
            Ok(())
        }
        Err(api::Error::Auth { code }) => {
            if json {
                let value = json!({
                    "team_id": team_id,
                    "team": workspace.name,
                    "user_id": workspace.user_id,
                    "url": workspace.url,
                    "status": "auth_failed",
                    "error": code,
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{} ({})", workspace.name, team_id);
                if let Some(user_id) = &workspace.user_id {
                    println!("  User: {user_id} (as saved at login)");
                }
                println!("  URL: {}", workspace.url);
                println!("  Status: authentication failed ({code})");
            }
            Err(api::Error::Auth { code }.into())
        }
        Err(err) => Err(err.into()),
    }
}

pub fn login(headless: bool, timeout_secs: u64) -> Result<()> {
    let profile_dir = chrome::profile_dir()?;
    let mut browser = chrome::launch(&profile_dir, headless)?;
    let mut session = cdp::Session::connect(&browser.page_ws_url()?)
        .context("could not attach to the browser")?;

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut rejected = HashSet::new();
    let (verified, refused) = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("{}", timeout_advice(headless));
        }
        let options = capture::Options {
            timeout: remaining,
            headless,
            rejected: rejected.clone(),
        };
        let captured =
            match capture::capture(&mut session, &options, &mut |line| eprintln!("{line}")) {
                Ok(captured) => captured,
                Err(capture::Error::Timeout) => bail!("{}", timeout_advice(headless)),
                Err(err) => return Err(err.into()),
            };
        let (verified, refused) = verify(captured)?;
        rejected.extend(refused.iter().map(|(_, _, xoxc)| xoxc.clone()));
        if !verified.is_empty() {
            break (verified, refused);
        }
        // The browser profile outlives the Slack session it holds: Slack can
        // expire or revoke it while the profile still carries the old tokens.
        if headless {
            bail!(
                "the saved browser session has expired; run `slack login` without --headless and \
                 sign in again"
            );
        }
        eprintln!("The saved session has expired. Sign in again in the browser window…");
    };
    session.close_browser();
    browser.stop();

    let mut store = Workspaces::load()?;
    for (identity, workspace) in &verified {
        store.upsert(&identity.team_id, workspace.clone());
    }
    store.save()?;
    for (identity, _) in &verified {
        let marker = if store.default.as_deref() == Some(&identity.team_id) {
            " (default)"
        } else {
            ""
        };
        println!(
            "Signed in to {} as {} ({}){marker}",
            identity.team, identity.user, identity.user_id
        );
    }
    for (url, code, _) in &refused {
        eprintln!("warning: Slack refused the captured session for {url} ({code}); skipped");
    }
    Ok(())
}

pub fn logout(keep_browser_session: bool) -> Result<()> {
    if Workspaces::remove()? {
        println!("Removed the saved workspaces.");
    } else {
        println!("No saved workspaces.");
    }
    if keep_browser_session {
        println!(
            "Kept the browser profile: `slack login --headless` can still sign in without a prompt."
        );
        return Ok(());
    }
    let profile_dir = chrome::profile_dir()?;
    match chrome::clear_profile(&profile_dir)? {
        ClearOutcome::Cleared => println!("Deleted the browser profile."),
        ClearOutcome::Absent => println!("No browser profile to delete."),
        ClearOutcome::NotOurs => eprintln!(
            "warning: {} was not created by slack and was left in place",
            profile_dir.display()
        ),
    }
    Ok(())
}

type Verified = Vec<(Identity, Workspace)>;
/// Workspace URL, Slack's error code, the refused token.
type Refused = Vec<(String, String, String)>;

/// Check every captured workspace with `auth.test` before anything is
/// saved. Partial success is success: a stale token on one workspace must
/// not lose the others.
fn verify(captured: capture::Capture) -> Result<(Verified, Refused)> {
    let mut verified = Vec::new();
    let mut refused = Vec::new();
    for workspace in captured.workspaces {
        let client = Client::new(
            &workspace.url,
            &Credentials {
                xoxc: &workspace.xoxc,
                xoxd: &captured.xoxd,
            },
        );
        match client.auth_test() {
            Ok(identity) => {
                let saved = Workspace {
                    name: identity.team.clone(),
                    url: workspace.url,
                    user_id: Some(identity.user_id.clone()),
                    xoxc: workspace.xoxc,
                    xoxd: captured.xoxd.clone(),
                };
                verified.push((identity, saved));
            }
            Err(api::Error::Auth { code }) => refused.push((workspace.url, code, workspace.xoxc)),
            Err(err) => return Err(err).context("could not verify the captured session"),
        }
    }
    Ok((verified, refused))
}

fn timeout_advice(headless: bool) -> &'static str {
    if headless {
        "no Slack session was detected before the timeout; the saved browser session has probably \
         expired, run `slack login` without --headless and sign in again"
    } else {
        "no Slack session was detected before the timeout; make sure you completed the sign-in in \
         the browser window"
    }
}
