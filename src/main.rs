//! `slack`: read and answer Slack from the terminal. README.md has what it
//! does, ARCHITECTURE.md how it is put together.

mod api;
mod auth;
mod commands;
mod counts;
mod directory;
mod emoji;
mod history;
mod lastview;
mod mention;
mod mrkdwn;
mod names;
mod par;
mod paths;
mod prefs;
mod presence;
mod react;
mod read;
mod replies;
mod rtm;
mod search;
mod send;
mod status;
mod store;
mod style;
mod target;
mod threads;
mod view;
mod warn;
mod watch;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Read and answer Slack from the terminal. With no command, what is unread,
/// as `slack unreads` prints it.
#[derive(Parser)]
#[command(name = "slack", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print only the number of conversations, for a status bar
    #[arg(long)]
    count: bool,
    /// With --count: the conversations and the messages aimed at you, as JSON
    #[arg(long, requires = "count")]
    json: bool,
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
        /// `#channel`, `@user`, a thread number from the last output, a conversation id or a permalink
        target: String,
        /// The message; read from stdin when omitted
        text: Option<String>,
        /// Reply in this thread (its ts or permalink)
        #[arg(long)]
        thread: Option<String>,
    },
    /// Put an emoji on a message, or take yours back
    React {
        /// A permalink, a thread number from the last output (its first message), or `#channel`, `@person`, a conversation id with --ts
        message: String,
        /// The emoji: its name (`+1`, `:white_check_mark:`) or the character
        #[arg(allow_hyphen_values = true)]
        emoji: String,
        /// The message's ts, in the conversation given
        #[arg(long)]
        ts: Option<String>,
        /// Take your reaction back instead
        #[arg(long)]
        remove: bool,
    },
    /// Set your status, clear it, or show it when given nothing
    Status {
        /// The text, 100 characters at most
        text: Vec<String>,
        /// The emoji: its name (`crescent_moon`, `:palm_tree:`) or the character
        #[arg(short, long)]
        emoji: Option<String>,
        /// When Slack clears it: `45m`, `2h`, `07:30` (the next one), `mon 07:30`, `2026-11-02 07:30`; never without it
        #[arg(long)]
        until: Option<String>,
        /// Clear your status
        #[arg(long, conflicts_with_all = ["text", "emoji", "until", "if_unset"])]
        clear: bool,
        /// Leave a status already set alone, for a scheduled job
        #[arg(long)]
        if_unset: bool,
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
    /// Show who is around: presence, status, their clock, paused notifications
    Who {
        /// `@person`s, or a `#channel` for its members; the people of the DMs you visited last when omitted
        targets: Vec<String>,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
    },
    /// Search the messages, newest first; Slack's operators work (`in:#dev from:@jane after:2026-10-01`)
    Search {
        /// What to look for, Slack's search syntax
        #[arg(required = true)]
        query: Vec<String>,
        /// How many matches, the newest ones
        #[arg(short = 'n', long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
        /// Print the data as JSON
        #[arg(long)]
        json: bool,
        /// Disable colors
        #[arg(long)]
        no_color: bool,
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
    /// Listen for DMs and mentions of you, hand each to a command, log what it would answer; sends nothing
    Watch {
        /// Seconds without a new message from the sender before the exchange is handed over
        #[arg(long, default_value_t = 60)]
        settle: u64,
        /// Seconds the command may take before it is stopped
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// The program to run for each, with its arguments (after `--`); it reads the exchange as JSON on stdin and prints the reply, or nothing. Without one, the events are only logged
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None if cli.count => commands::count(cli.json),
        None => commands::unreads(false, false),
        Some(Command::Unreads { json, no_color }) => commands::unreads(json, no_color),
        Some(Command::Send {
            target,
            text,
            thread,
        }) => commands::send(&target, text.as_deref(), thread.as_deref()),
        Some(Command::React {
            message,
            emoji,
            ts,
            remove,
        }) => commands::react(&message, &emoji, ts.as_deref(), remove),
        Some(Command::Status {
            text,
            emoji,
            until,
            clear,
            if_unset,
        }) => commands::status(
            &text.join(" "),
            emoji.as_deref(),
            until.as_deref(),
            clear,
            if_unset,
        ),
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
        Some(Command::Who { targets, json }) => commands::who(&targets, json),
        Some(Command::Search {
            query,
            limit,
            json,
            no_color,
        }) => commands::search(&query.join(" "), usize::from(limit), json, no_color),
        Some(Command::Show {
            target,
            limit,
            json,
            no_color,
        }) => commands::show(&target, usize::from(limit), json, no_color),
        Some(Command::Watch {
            settle,
            timeout,
            command,
        }) => commands::watch(settle, timeout, command),
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
    fn no_command_is_unreads() {
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

    #[test]
    fn watch_takes_the_command_with_its_flags() {
        let cli = Cli::parse_from([
            "slack", "watch", "--settle", "30", "--", "answer", "--judge", "-v",
        ]);
        assert!(matches!(
            cli.command,
            Some(Command::Watch { settle: 30, timeout: 600, command }) if command == ["answer", "--judge", "-v"]
        ));
        let cli = Cli::parse_from(["slack", "watch"]);
        assert!(matches!(cli.command, Some(Command::Watch { command, .. }) if command.is_empty()));
    }

    #[test]
    fn status_takes_its_words_and_refuses_clear_with_them() {
        let cli = Cli::parse_from([
            "slack",
            "status",
            "Off",
            "for",
            "the",
            "day",
            "-e",
            "🌙",
            "--until",
            "mon 07:30",
        ]);
        assert!(matches!(
            cli.command,
            Some(Command::Status { text, emoji: Some(emoji), until: Some(until), clear: false, if_unset: false })
                if text == ["Off", "for", "the", "day"] && emoji == "🌙" && until == "mon 07:30"
        ));
        assert!(Cli::try_parse_from(["slack", "status", "--clear"]).is_ok());
        assert!(Cli::try_parse_from(["slack", "status", "Off", "--clear"]).is_err());
        assert!(Cli::try_parse_from(["slack", "status", "--clear", "--until", "2h"]).is_err());
    }

    #[test]
    fn react_takes_a_thumbs_down_as_an_emoji() {
        let cli = Cli::parse_from(["slack", "react", "2", "-1", "--remove"]);
        assert!(matches!(
            cli.command,
            Some(Command::React { emoji, remove: true, .. }) if emoji == "-1"
        ));
    }
}
