//! `slack`: read and answer Slack from the terminal. README.md has what it
//! does, ARCHITECTURE.md how it is put together.

mod api;
mod auth;
mod commands;
mod counts;
mod directory;
mod history;
mod lastview;
mod mrkdwn;
mod names;
mod par;
mod paths;
mod prefs;
mod read;
mod replies;
mod send;
mod store;
mod style;
mod target;
mod threads;
mod tui;
mod view;
mod warn;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Read and answer Slack from the terminal. With no command, what is unread
/// as an interactive view; `slack unreads` prints it instead.
#[derive(Parser)]
#[command(name = "slack", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print only the number of conversations, for a status bar
    #[arg(long)]
    count: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print what is unread: DMs, mentions, channels, threads
    Unreads {
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
        /// Disable colors
        #[arg(long)]
        no_color: bool,
    },
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
    /// Mark as read what the last view showed
    Read {
        /// Only these conversations
        targets: Vec<String>,
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
    /// Show a thread: its number in the last `slack unreads` output, or a permalink
    Thread {
        /// `2` as numbered by `slack unreads` or `slack show`, or a Slack permalink
        thread: String,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
        /// Disable colors
        #[arg(long)]
        no_color: bool,
    },
    /// List the channels and group messages you are in
    Channels {
        /// Only those whose name contains these words
        filter: Vec<String>,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
    },
    /// List the people you have a direct message with, or search the workspace
    People {
        /// Search the workspace directory for these words (name, handle, title)
        filter: Vec<String>,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the recent messages of a channel or a person
    Show {
        /// `#channel`, `@person` or a conversation id
        target: String,
        /// How many messages, the newest ones
        #[arg(short = 'n', long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
        /// Disable colors
        #[arg(long)]
        no_color: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None if cli.count => commands::count(),
        // A pipe or a script cannot drive a screen; it gets the text.
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => commands::ui(),
        None => commands::unreads(false, false),
        Some(Command::Unreads { json, no_color }) => commands::unreads(json, no_color),
        Some(Command::Send {
            target,
            text,
            thread,
        }) => commands::send(&target, text.as_deref(), thread.as_deref()),
        Some(Command::Read { targets }) => commands::read(&targets),
        Some(Command::Login { headless, timeout }) => commands::login(headless, timeout),
        Some(Command::Logout {
            keep_browser_session,
        }) => commands::logout(keep_browser_session),
        Some(Command::Whoami { json }) => commands::whoami(json),
        Some(Command::Thread {
            thread,
            json,
            no_color,
        }) => commands::thread(&thread, json, no_color),
        Some(Command::Channels { filter, json }) => commands::channels(&filter.join(" "), json),
        Some(Command::People { filter, json }) => commands::people(&filter.join(" "), json),
        Some(Command::Show {
            target,
            limit,
            json,
            no_color,
        }) => commands::show(&target, usize::from(limit), json, no_color),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("slack: {err:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_command_is_the_view_and_unreads_prints_it() {
        let cli = Cli::parse_from(["slack"]);
        assert!(cli.command.is_none() && !cli.count);
        let cli = Cli::parse_from(["slack", "unreads", "--json"]);
        assert!(matches!(
            cli.command,
            Some(Command::Unreads { json: true, .. })
        ));
        assert!(Cli::try_parse_from(["slack", "ui"]).is_err());
        assert!(Cli::try_parse_from(["slack", "--json"]).is_err());
    }
}
