# slack

Slack from the terminal: `slack` prints what needs you, `slack --count` gives a status bar one number, `slack send` answers, `slack react` puts an emoji on a message, `slack read` marks as read, `slack show` reads any conversation, `slack search` finds a message, `slack who` says who is around. No Slack app and no admin approval: the CLI reuses the session of a browser you sign into once, the way [slackcli](https://github.com/shaharia-lab/slackcli) does.

Early days, but every command below works.

## Install

With [Homebrew](https://brew.sh/), on macOS (Apple Silicon) or Linux (x86_64):

```bash
brew install denrou/slack/slack
```

Without it: download the archive for your platform from the [latest release](https://github.com/denrou/slack/releases/latest), check it against `checksums.txt`, and put `slack` on your `PATH`.

From source, with Rust 1.88+:

```bash
cargo install --git https://github.com/denrou/slack
```

That is the tip of `main`, which can be ahead of the last release; `--tag vX.Y.Z` pins one.

Login needs Google Chrome (Chromium, Edge or Brave also work; `$SLACK_BROWSER` points at an executable the lookup does not know).

## Quickstart

```bash
slack login           # a Chrome window opens; sign in to Slack once
slack whoami          # which workspace and user the CLI acts as, verified
slack                 # what is unread: DMs, mentions, channels, threads
slack unreads         # the same
slack unreads --json  # the same data, for scripts
slack --count         # the same, as one number for a status bar
slack --count --json  # that number and the messages aimed at you
slack thread 2        # the thread numbered 2 in the last output, or a permalink

slack channels                  # the channels and group messages you are in
slack channels dev              # ...whose name contains "dev"
slack people                    # the people you have a direct message with
slack people jane               # ...or anyone in the workspace matching "jane"
slack who                       # the people of your recent DMs: there or away, status, their clock
slack who @jane '#sre'          # ...or these people, or a channel's members
slack show '#dev-team'          # the last 20 messages of a channel, read or not
slack show @jane -n 50          # ...or of a direct message, 50 of them
slack search deploy failed      # the 20 newest messages that match, anywhere
slack search 'in:#sre from:@jane after:2026-10-01' --json   # Slack's operators, as data

slack send '#sre-alerts' "on it"                    # post to a channel
slack send @jane "merci !"                          # ...or a DM
slack send <permalink> "answering here"             # ...or a thread
slack send @jane < note.md                          # text from stdin
slack react <permalink> +1                          # react to a message
slack react <permalink> +1 --remove                 # ...or take your reaction back
slack read                                          # mark as read what the last view showed
slack read '#sre-alerts' @jane                      # ...only these, or a conversation `slack show` printed
```

## Unreads

`slack unreads`, or `slack` alone, prints what is unread, in the order a person answers it: **Direct messages** (DMs and group DMs with unreads), **Mentions** (channels where you were named), **Channels** (every other channel with new messages), **Threads** (followed threads with new replies). Under each conversation come the unread messages themselves, oldest first, with the author, the local time, and notes for files, attachments, replies and reactions (`[2 replies, 👍 3  👀 1]`). Messages with replies are numbered, as in `[thread 2: 5 replies]`: `slack thread 2` prints the whole thread, and `slack send 2 "text"` answers in it (a permalink works in both places). Conversations you muted in Slack are left out, as the app does, and a line on stderr says how many; a mention in a muted channel still counts. There is no local list: muting a channel in Slack is how to keep it out of the view.

Reading in the terminal never moves Slack's read cursor: the view is read-only, and `slack read` marks as read what the last view showed (no more: a message that arrived since stays unread). A conversation `slack show` printed is marked by its name only. Names are cached in `~/.cache/slack/names.json`, safe to delete.

## Any conversation

The view stops at what is unread. `slack show '#dev-team'` prints the last 20 messages of a conversation whether you read them or not (`-n 50` for more), with a `── new ──` line where the unread part starts and the threads numbered as in the view, so `slack thread 1` and `slack send 1 "…"` work from that output too. The target is a `#channel`, an `@person` or a conversation id.

To find the name, `slack channels` lists the channels and group messages you are in, with `private`, `muted` and `unread` where they apply, and `slack people` lists the people you have a direct message with. Both take words to match: `slack channels sre`, `slack people jane doe`. For people, a filter searches the whole workspace directory (handle, name, title), not only your direct messages. `--json` on any of the three gives the data.

## Who is around

`slack who @jane @bob` says, for each person, whether they are at their screen (`●` active, `○` away, Slack's green dot), their status with its emoji and when it ends, their clock when it is not yours (`16:00 in Brussels`), and until when their notifications are paused, so you know before you write whether they will see it now. `slack who '#sre'` does the same for a channel's members, bots left out, up to 50; without arguments, it is the people of the direct messages you visited last. The ones there come first. Slack keeps no history of this, so it is now or nothing. `--json` gives `presence`, `status` (`emoji`, `text`, `expires`), `tz`, `local_time` and `notifications_paused_until` for each.

## Search

`slack search <query>` asks Slack's own search and prints the newest matches first, 20 of them (`-n 100` at most), each under its conversation with its author, its time and its permalink, which `slack thread` and `slack send` take. The query is Slack's: `in:#channel`, `from:@person`, `before:`, `after:`, `on:`, `has:link` work as in the app. A line on stderr says how many matched beyond those shown, so a pipe gets the matches alone; `--json` gives `query`, `total` and `matches` (each a message with `channel`, `name` and `permalink`).

## Sending

A target is `#channel`, `@person` (handle or display name), a conversation id, or a Slack permalink; a permalink means "answer in that message's thread", and `--thread <ts|permalink>` does the same for a channel target. The text is the argument, or stdin when there is none, and is sent as Slack markup (`*bold*`, `<@U123>`). The command prints where the message landed and its permalink. Quote `#channel` in zsh, or the shell reads it as a comment.

`slack react <message> <emoji>` puts an emoji on a message, and `--remove` takes yours back. The message is a permalink, a thread number (its first message), or a conversation with `--ts <ts>`; the emoji is its Slack name, with or without the colons (`+1`, `:white_check_mark:`, a custom one), or the character itself. Reacting never marks anything as read.

## How login works

`slack login` starts your Chrome against a profile of its own (`~/.config/slack/browser-profile`), opens Slack, and waits for you to sign in. It then reads the two values the Slack web client itself uses, the session cookie and the per-workspace token, over the DevTools Protocol, checks them against Slack, and saves every workspace you are signed into. The browser closes by itself.

The profile is separate from your everyday browsing because Chrome refuses remote debugging on the default profile. It persists, so only the first login needs you: `slack login --headless` refreshes the tokens afterwards with no window, which is what a cron or launchd job can call when a session expires.

Security notes, the short version:

- The CLI acts as you, with exactly your permissions, and every request is attributed to you like a click in the app.
- `~/.config/slack/workspaces.json` (mode `0600`) holds the credentials. The browser profile next to it is a credential store too: while it exists, `login --headless` can mint working tokens with no prompt. `slack logout` deletes both; `--keep-browser-session` keeps the profile.
- Token values are never printed, logged or included in `--json`.
- Only `https://*.slack.com` origins are ever paired with the session cookie.

## Status bar

`slack --count` makes two API calls side by side, prints one number and exits non-zero when the fetch fails, so a poller can tell "nothing" from "broken". The number is the unread conversations (DMs, group messages and channels with something new, muted ones excepted unless you were mentioned) plus one when followed threads have unread replies.

`slack --count --json` prints `{"unread": 3, "me": 2}` from the same two calls, laid out over lines like every `--json` output, so read it with `jq` rather than line by line: `unread` is that number, `me` the messages aimed at you (the unread messages of DMs and group messages, the mentions in channels and in followed threads), for a bar that shows the urgent part apart.

## For an agent

`skills/slack/SKILL.md` is the manual an LLM agent reads before using the CLI: what is free to run, what needs the user's word (`send`, `read`), how the numbers and the JSON work. For Claude Code, link it from a clone of the repository and `/slack` becomes a skill:

```bash
ln -s "$PWD/skills/slack" ~/.claude/skills/slack
```

## Files

`$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`): `workspaces.json` and `browser-profile/`. `$XDG_CACHE_HOME/slack/` (default `~/.cache/slack/`) holds the caches, both safe to delete: names and the last view. `$SLACK_BROWSER_PROFILE` overrides the profile location; `logout` only ever deletes a profile the CLI created.

## Releasing

Bump `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock` follows, commit, then push a matching annotated tag whose message is the tag name, a blank line and the release notes:

```bash
git tag -a v0.8.0 --cleanup=whitespace -F notes.txt && git push origin main v0.8.0
```

The [Release workflow](.github/workflows/release.yml) checks that the tag matches the version and carries notes, lints and tests, builds the macOS arm64 and Linux x86_64 binaries, publishes them as a GitHub Release with the tag's notes, the compare link to the previous release and a `checksums.txt`, then asks the [Homebrew tap](https://github.com/denrou/homebrew-slack) to pick the release up.

## License

MIT
