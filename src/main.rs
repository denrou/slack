//! `slack`: read and answer Slack from the terminal. PLAN.md has the shape
//! of the tool, CLAUDE.md how it is built.

mod api;
mod auth;
mod commands;
mod counts;
mod history;
mod lastview;
mod mrkdwn;
mod names;
mod par;
mod paths;
mod prefs;
mod read;
mod send;
mod store;
mod style;
mod target;
mod threads;
mod tui;
mod view;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Read and answer Slack from the terminal: what needs you, a count for the
/// status bar, send, mark read.
#[derive(Parser)]
#[command(name = "slack", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Show every conversation with unreads, not only what needs you
    #[arg(short, long)]
    all: bool,
    /// Print only the number of conversations, for a status bar
    #[arg(long)]
    count: bool,
    /// Print the data as JSON
    #[arg(long)]
    json: bool,
    /// Disable colors
    #[arg(long)]
    no_color: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Post a message to a channel, a person or a thread
    Send {
        /// `#channel`, `@user`, a conversation id or a permalink
        target: String,
        /// The message; read from stdin when omitted
        text: Option<String>,
        /// Reply in this thread (its ts or permalink)
        #[arg(long)]
        thread: Option<String>,
    },
    /// Mark as read what the last view showed (DMs and mentions)
    Read {
        /// Only these conversations
        targets: Vec<String>,
        /// Also the channels the last `slack -a` listed
        #[arg(short, long)]
        all: bool,
    },
    /// Sign in through a browser window and save the session
    Login {
        /// No window; works once a previous login saved the browser session
        #[arg(long)]
        headless: bool,
        /// Seconds to wait for the sign-in
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Forget the saved session and delete the browser profile
    Logout {
        /// Keep the browser profile, so `login --headless` still works
        #[arg(long)]
        keep_browser_session: bool,
    },
    /// Show the workspace and user the CLI acts as, verified against Slack
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Move through what needs you and answer it, interactively
    Ui {
        /// Also list the channels without a mention
        #[arg(short, long)]
        all: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None if cli.count => commands::count(cli.all),
        None => commands::view(cli.all, cli.json, cli.no_color),
        Some(Command::Send {
            target,
            text,
            thread,
        }) => commands::send(&target, text.as_deref(), thread.as_deref()),
        Some(Command::Read { targets, all }) => commands::read(&targets, all),
        Some(Command::Login { headless, timeout }) => commands::login(headless, timeout),
        Some(Command::Logout {
            keep_browser_session,
        }) => commands::logout(keep_browser_session),
        Some(Command::Whoami { json }) => commands::whoami(json),
        Some(Command::Ui { all }) => commands::ui(all),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("slack: {err:#}");
            ExitCode::from(1)
        }
    }
}
