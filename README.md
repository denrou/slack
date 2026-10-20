# slack

Slack from the terminal: `slack` opens what needs you in an interactive view, `slack unreads` prints it, `slack --count` gives a status bar one number, `slack send` answers, `slack read` marks as read, `slack show` reads any conversation. No Slack app and no admin approval: the CLI reuses the session of a browser you sign into once, the way [slackcli](https://github.com/shaharia-lab/slackcli) does.

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
slack                 # what is unread, interactive: move through it, reply, mark read
slack unreads         # the same, printed: DMs, mentions, channels, threads
slack unreads --json  # the same data, for scripts
slack --count         # the same, as one number for a status bar
slack thread 2        # the thread numbered 2 in the last output, or a permalink

slack channels                  # the channels and group messages you are in
slack channels dev              # ...whose name contains "dev"
slack people                    # the people you have a direct message with
slack people jane               # ...or anyone in the workspace matching "jane"
slack show '#dev-team'          # the last 20 messages of a channel, read or not
slack show @jane -n 50          # ...or of a direct message, 50 of them

slack send '#sre-alerts' "on it"                    # post to a channel
slack send @jane "merci !"                          # ...or a DM
slack send <permalink> "answering here"             # ...or a thread
slack send @jane < note.md                          # text from stdin
slack read                                          # mark as read what the last view showed
slack read '#sre-alerts' @jane                      # ...only these, or a conversation `slack show` printed
```

## Unreads

`slack unreads` prints what is unread, in the order a person answers it: **Direct messages** (DMs and group DMs with unreads), **Mentions** (channels where you were named), **Channels** (every other channel with new messages), **Threads** (followed threads with new replies). Under each conversation come the unread messages themselves, oldest first, with the author, the local time, and notes for files, attachments and replies. Messages with replies are numbered, as in `[thread 2: 5 replies]`: `slack thread 2` prints the whole thread, and `slack send 2 "text"` answers in it (a permalink works in both places). Conversations you muted in Slack are left out, as the app does, and a line on stderr says how many; a mention in a muted channel still counts. There is no local list: muting a channel in Slack is how to keep it out of the view.

Reading in the terminal never moves Slack's read cursor: the view is read-only, and `slack read` marks as read what the last view showed (no more: a message that arrived since stays unread). A conversation `slack show` printed is marked by its name only. Names are cached in `~/.cache/slack/names.json`, safe to delete.

## Any conversation

The view stops at what is unread. `slack show '#dev-team'` prints the last 20 messages of a conversation whether you read them or not (`-n 50` for more), with a `── new ──` line where the unread part starts and the threads numbered as in the view, so `slack thread 1` and `slack send 1 "…"` work from that output too. The target is a `#channel`, an `@person` or a conversation id.

To find the name, `slack channels` lists the channels and group messages you are in, with `private`, `muted` and `unread` where they apply, and `slack people` lists the people you have a direct message with. Both take words to match: `slack channels sre`, `slack people jane doe`. For people, a filter searches the whole workspace directory (handle, name, title), not only your direct messages. `--json` on any of the three gives the data.

## The interactive view

`slack` with no command puts the same view in a two-pane screen: the list of what is unread on the left, the messages of the selected item on the right, a reply box at the bottom when you press Enter. A conversation shows its last 50 messages, the ones already read greyed out and a `── new ──` line where the unread part starts; the pane opens there, and `l` lands on the first new message. `u` opens a link of the highlighted message in the browser and `y` copies it; with several links, a box lists them and a digit picks one. `c` opens a picker over every channel, group message and person in the workspace: `j`/`k` move, `/` filters (Enter returns to the narrowed list), Enter loads the one selected into an **Opened** section at the end of the list, where it stays through refreshes and replies until `x` closes it. Space marks people instead, and Enter then opens their group message. `n` is the same picker followed by the reply box: a new message to whoever you picked. Enter answers where you are: the conversation from the list, the highlighted message in its thread from the pane, the thread once one is open. `s` writes to the conversation itself from anywhere in it.

| Key                | Action                                                                        |
| ------------------ | ----------------------------------------------------------------------------- |
| `j`/`k`, arrows    | Move through the list, or through the messages                                |
| `l`, Right, Tab    | Deeper: the messages, then the highlighted message's thread                   |
| `h`, Left, `Esc`   | Back: close the thread, leave the messages, quit                              |
| `Enter`            | Reply: to the conversation, to the highlighted message, or in the open thread |
| `s`                | A message to the conversation, wherever you are in it                         |
| `n`                | New message: pick a channel or people, then write                             |
| `c`                | Open any channel, group or person (`/` filters, Space marks people)           |
| `x`                | Close an opened conversation                                                  |
| `u`, `y`           | Open a link of the highlighted message, or copy it                            |
| `J`/`K`, PgDn/PgUp | Scroll the messages, from the list                                            |
| `r`                | Mark it as read                                                               |
| `R`                | Mark everything listed as read (asks first)                                   |
| `o`                | Open it in Slack                                                              |
| `g`                | Refresh                                                                       |
| `?`                | Keys                                                                          |
| `q`                | Quit                                                                          |

A reply from the list marks the conversation as read up to your message, as in the app; a reply inside a thread leaves the list as it is and shows the thread again with your message, and a reply to a message from the pane leaves the conversation listed, since it stays unread in Slack. A reply is typed on one line and Enter sends it; pasted text keeps its line breaks. `slack send` with stdin takes a long message.

Without a terminal on both ends, as in a pipe or a script, `slack` prints what `slack unreads` prints instead of drawing.

## Sending

A target is `#channel`, `@person` (handle or display name), a conversation id, or a Slack permalink; a permalink means "answer in that message's thread", and `--thread <ts|permalink>` does the same for a channel target. The text is the argument, or stdin when there is none, and is sent as Slack markup (`*bold*`, `<@U123>`). The command prints where the message landed and its permalink. Quote `#channel` in zsh, or the shell reads it as a comment.

## How login works

`slack login` starts your Chrome against a profile of its own (`~/.config/slack/browser-profile`), opens Slack, and waits for you to sign in. It then reads the two values the Slack web client itself uses, the session cookie and the per-workspace token, over the DevTools Protocol, checks them against Slack, and saves every workspace you are signed into. The browser closes by itself.

The profile is separate from your everyday browsing because Chrome refuses remote debugging on the default profile. It persists, so only the first login needs you: `slack login --headless` refreshes the tokens afterwards with no window, which is what a cron or launchd job can call when a session expires.

Security notes, the short version:

- The CLI acts as you, with exactly your permissions, and every request is attributed to you like a click in the app.
- `~/.config/slack/workspaces.json` (mode `0600`) holds the credentials. The browser profile next to it is a credential store too: while it exists, `login --headless` can mint working tokens with no prompt. `slack logout` deletes both; `--keep-browser-session` keeps the profile.
- Token values are never printed, logged or included in `--json`.
- Only `https://*.slack.com` origins are ever paired with the session cookie.

## Status bar

`slack --count` makes one API call, prints one number and exits non-zero when the fetch fails, so a poller can tell "nothing" from "broken". The number is the unread conversations (DMs, group messages and channels with something new, muted ones excepted unless you were mentioned) plus one when followed threads have unread replies.

## Files

`$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`): `workspaces.json` and `browser-profile/`. `$SLACK_BROWSER_PROFILE` overrides the profile location; `logout` only ever deletes a profile the CLI created.

## Releasing

Bump `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock` follows, commit, then push a matching annotated tag:

```bash
git tag -a v0.2.0 -m "v0.2.0" && git push origin main v0.2.0
```

The [Release workflow](.github/workflows/release.yml) checks that the tag matches the version, lints and tests, builds the macOS arm64 and Linux x86_64 binaries, and publishes them as a GitHub Release with generated notes and a `checksums.txt`.

## License

MIT
