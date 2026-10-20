//! The glue between the command line and the modules: one function per
//! command, each returning an error the main function prints.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::api::{self, Client, Credentials, Identity};
use crate::auth::chrome::{self, ClearOutcome};
use crate::auth::{capture, cdp};
use crate::names::Names;
use crate::store::{Workspace, Workspaces};
use crate::style::Style;
use crate::{counts, lastview, read, send, tui, view};

pub fn view(all: bool, json: bool, no_color: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let client = workspace.client();
    let counts = counts::fetch(&client)?;
    let mut names = Names::load();
    let view = view::build(&client, &counts, &mut names, all)?;
    names.save();
    if let Err(err) = lastview::remember(team_id, &view) {
        eprintln!("warning: could not record the view for `slack read`: {err:#}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    let style = Style::detect(no_color);
    let stdout = io::stdout();
    match view::render(
        &view,
        &names,
        &style,
        workspace.user_id.as_deref(),
        &mut stdout.lock(),
    ) {
        // `slack | head` closes the pipe early; that is not an error.
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

pub fn count(all: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (_, workspace) = store.default_workspace()?;
    let counts = counts::fetch(&workspace.client())?;
    println!(
        "{}",
        if all {
            counts.unread_count()
        } else {
            counts.attention_count()
        }
    );
    Ok(())
}

pub fn ui(all: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let result = tui::run(
        &workspace.client(),
        &mut names,
        team_id,
        workspace.user_id.as_deref(),
        &workspace.url,
        all,
    );
    names.save();
    result
}

pub fn send(target: &str, text: Option<&str>, thread: Option<&str>) -> Result<()> {
    let store = Workspaces::load()?;
    let (_, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let result = send::run(&workspace.client(), &mut names, target, text, thread);
    names.save();
    result
}

pub fn read(targets: &[String], all: bool) -> Result<()> {
    let store = Workspaces::load()?;
    let (team_id, workspace) = store.default_workspace()?;
    let mut names = Names::load();
    let result = read::run(&workspace.client(), &mut names, team_id, targets, all);
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
