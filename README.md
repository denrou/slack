# slack

Slack from the terminal: `slack` opens what needs you in an interactive view, `slack unreads` prints it, `slack --count` gives a status bar one number, `slack send` answers, `slack react` puts an emoji on a message, `slack read` marks as read, `slack show` reads any conversation, `slack search` finds a message. No Slack app and no admin approval: the CLI reuses the session of a browser you sign into once, the way [slackcli](https://github.com/shaharia-lab/slackcli) does.

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
slack --count --json  # that number and the messages aimed at you
slack thread 2        # the thread numbered 2 in the last output, or a permalink

slack channels                  # the channels and group messages you are in
slack channels dev              # ...whose name contains "dev"
slack people                    # the people you have a direct message with
slack people jane               # ...or anyone in the workspace matching "jane"
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

`slack unreads` prints what is unread, in the order a person answers it: **Direct messages** (DMs and group DMs with unreads), **Mentions** (channels where you were named), **Channels** (every other channel with new messages), **Threads** (followed threads with new replies). Under each conversation come the unread messages themselves, oldest first, with the author, the local time, and notes for files, attachments, replies and reactions (`[2 replies, 👍 3  👀 1]`). Messages with replies are numbered, as in `[thread 2: 5 replies]`: `slack thread 2` prints the whole thread, and `slack send 2 "text"` answers in it (a permalink works in both places). Conversations you muted in Slack are left out, as the app does, and a line on stderr says how many; a mention in a muted channel still counts. There is no local list: muting a channel in Slack is how to keep it out of the view.

Reading in the terminal never moves Slack's read cursor: the view is read-only, and `slack read` marks as read what the last view showed (no more: a message that arrived since stays unread). A conversation `slack show` printed is marked by its name only. Names are cached in `~/.cache/slack/names.json`, safe to delete.

## Any conversation

The view stops at what is unread. `slack show '#dev-team'` prints the last 20 messages of a conversation whether you read them or not (`-n 50` for more), with a `── new ──` line where the unread part starts and the threads numbered as in the view, so `slack thread 1` and `slack send 1 "…"` work from that output too. The target is a `#channel`, an `@person` or a conversation id.

To find the name, `slack channels` lists the channels and group messages you are in, with `private`, `muted` and `unread` where they apply, and `slack people` lists the people you have a direct message with. Both take words to match: `slack channels sre`, `slack people jane doe`. For people, a filter searches the whole workspace directory (handle, name, title), not only your direct messages. `--json` on any of the three gives the data.

## Search

`slack search <query>` asks Slack's own search and prints the newest matches first, 20 of them (`-n 100` at most), each under its conversation with its author, its time and its permalink, which `slack thread` and `slack send` take. The query is Slack's: `in:#channel`, `from:@person`, `before:`, `after:`, `on:`, `has:link` work as in the app. A line on stderr says how many matched beyond those shown, so a pipe gets the matches alone; `--json` gives `query`, `total` and `matches` (each a message with `channel`, `name` and `permalink`).

## The interactive view

`slack` with no command puts the same view in a two-pane screen: the list of what is unread on the left, the messages of the selected item on the right, a reply box at the bottom when you press `r`. Under the unread part, **Recent channels** and **Recent people** list the conversations you visited last in Slack, from any client (Slack's own recent list, 8 of each, the unread ones being listed above), so the screen never opens empty; Tab and Shift-Tab jump from section to section. A recent conversation's messages are read once the cursor rests on it, so moving past one with `j` costs nothing, and kept until a new message comes in there. The letters are Gmail's where Gmail has the action (`e` marks read, `x` selects, `r` replies, `c` composes, `o` opens, Enter goes in) and Slack's `R` for a reaction. A conversation shows today's messages and everything unread (at least the last 50, at most 200) and a thread the whole of it, the ones already read greyed out and a `── new ──` line where the unread part starts; the pane opens there (a thread keeps the first line of its root on top), and `l` or Enter lands on the first new message. `u` opens a link of the highlighted message in the browser and `y` copies it; with several links, a box lists them and a digit picks one. `u` opens a message's files too: an image (PNG, JPEG, WebP) shows in place of the messages, in the terminal's own graphics where it has them (kitty, iTerm2 or sixel: WezTerm and Ghostty do) and in coarse blocks elsewhere, where `u` again opens it full size in the system viewer and any other key closes it; any other file, a GIF included, goes straight to the application that opens its kind. Files are downloaded to `~/.cache/slack/files/`, readable by you alone, and dropped a week later, the next time the screen opens, or by `slack logout`. Reactions sit on a line under their message, yours in blue. `R` reacts to the highlighted message: a box lists the emoji already on it, then the ones you use most in Slack; typing filters (`check` finds ✅), Enter puts the highlighted one on, or takes it back when it is yours, the way a click on a reaction works in the app. Someone else's reaction shows as it is put on. `o` opens a picker over the channels and group messages you are in and every person in the workspace: type to filter, the arrows move, and Enter loads the highlighted one into an **Opened** section at the end of the list, where it stays through refreshes and replies until `e` closes it. Enter with nothing typed, or with nothing matching, hands the keys to the list instead (`j`/`k` move, `/` goes back to the filter). To write to several people, Tab marks the highlighted person as you type and clears the filter for the next name (Space does the same in the list), and Enter opens their group message, with the person typed last if there is one. Conversations you muted in Slack stay out of the picker until `m` shows them, from the list. `c` is the same picker followed by the reply box: a new message to whoever you picked. `/` searches Slack: type a query (Slack's operators work, `in:#dev from:@jane`), Enter lists the 50 newest matches in the list's place with the highlighted one whole on the right, and Enter on one opens its conversation in **Opened**, read around the match with the cursor on it, and a reply in its thread on top. `O` opens a match in Slack and `y` copies its link. `/` brings the last search back with its query, where Enter goes back to the matches and Ctrl-U clears the line for a new one. `r` answers where you are: the conversation from the list, the highlighted message in its thread from the pane, the thread once one is open. `s` writes to the conversation itself from anywhere in it.

`e` marks the selected conversation as read and takes it off the list; on an opened conversation it only closes it, since closing is not reading (an unread one is also listed in its section, where `e` marks it). To mark several, `x` selects them one at a time (a ✓ shows, `x` again unselects, Esc unselects them all) and `e` marks them together. Each is marked up to the newest message it had when you selected it: one that came in since was never on your screen, so it stays unread.

The screen keeps itself current: it listens to Slack's real-time socket, as the app does, and reloads when a message, a reply, an edit or a read moved what it shows, leaving you where you were (the selection, the highlighted message, the open thread). Someone typing puts a `✎` after the conversation in the list and their name under the messages. It waits while the reply box, the picker or a prompt is open, so nothing moves under what you are typing. Should the socket drop, the screen asks Slack every 15 seconds whether anything moved until it is back, and every 2 minutes beside it in case it stops sending something.

In WezTerm and Ghostty the screen also notifies you when a message aimed at you arrives while the terminal is in the background: a DM or a group message, unless muted, or a mention of you, `@here` and `@channel` included outside a muted channel. The terminal posts the notification (`OSC 777`), and clicking it brings you back. Nothing is posted while you are looking at the screen, which already shows the message, nor while the socket is down. Other terminals get no notification: they would drop the sequence without a word.

| Key               | Action                                                                                                                                        |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `j`/`k`, arrows   | Move through the list, or through the messages                                                                                                |
| `l`, Enter, Right | Deeper: the messages, then the highlighted message's thread                                                                                   |
| `h`, Left, `Esc`  | Back: close the thread, leave the messages; `Esc` at the list unselects                                                                       |
| `gg`, `G`         | The first, the last (Home, End too)                                                                                                           |
| Tab, Shift-Tab    | The next section of the list, the previous one                                                                                                |
| Ctrl-D, Ctrl-U    | Half a screen down, up; Ctrl-F, Ctrl-B, PgDn, PgUp a whole one                                                                                |
| `r`               | Reply: to the conversation, to the highlighted message, or in the open thread                                                                 |
| Ctrl-J            | A new line in the reply box (Ctrl-Enter and Shift-Enter too, where the terminal tells them from Enter)                                        |
| Arrows            | Move the cursor in the reply box; with Option a word, with Command to the start or end of the line (Home, End, Ctrl-A, Ctrl-E too)            |
| Ctrl-W, Ctrl-U    | Take the word, the line before the cursor back in the reply box (Option-Backspace and Command-Backspace too, where the terminal reports them) |
| Ctrl-G            | Correct the accents, typos and grammar of the reply box with your `fix_command`; again, your own text back                                    |
| `s`               | A message to the conversation, wherever you are in it                                                                                         |
| `c`               | Compose: pick a channel or people (Tab marks several as you type), then write                                                                 |
| `o`               | Open a channel or group you are in, or a person (type to filter, `m` shows the muted)                                                         |
| `/`               | Search Slack, then open a match in its conversation; `/` again brings the last search back                                                    |
| `e`               | Mark it as read, or close it, unmarked, if you opened it; with a selection, the whole selection                                               |
| `x`               | Select the conversation for `e`, or unselect it                                                                                               |
| `u`, `y`          | Open a link or a file of the highlighted message (an image shows in the pane), or copy its link                                               |
| `R`               | React to the highlighted message, or take your reaction back                                                                                  |
| `O`               | Open it in Slack, at the highlighted message from the pane                                                                                    |
| Ctrl-R            | Refresh now (it also happens on its own)                                                                                                      |
| `?`               | Keys                                                                                                                                          |
| `q`, Ctrl-C       | Quit                                                                                                                                          |

The keys follow three rules: `q` and Ctrl-C quit from anywhere, `Esc` and `h` back out of whatever is open (the picker, a prompt, the reply box, a thread, the pane; at the list, `Esc` alone also unselects) and never quit, and moving is vim's, `gg`, `G`, Ctrl-D and friends included. From the list the Ctrl keys scroll the pane; in the pane they move the highlighted message by as many messages as the screen shows.

A reply to a conversation marks it as read up to your message, as in the app, and it leaves the list; a reply inside a thread shows the thread again with your message, and a reply to a message from the pane leaves the conversation listed, since it stays unread in Slack. Enter sends a reply and Ctrl-J starts a new line in it. Ctrl-Enter and Shift-Enter (Slack's own key) do the same in a terminal that speaks the [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/), which `slack` asks for when the screen opens; in any other terminal both are plain Enter and send. The arrows move the cursor in the text, Up and Down from line to line; Option-Left and Option-Right move by word and Command-Left and Command-Right to the start or the end of the line where the terminal reports them (Home, End, Ctrl-A and Ctrl-E everywhere). Typing, a paste and a completion go in at the cursor. Backspace takes the character before the cursor back, Delete the one after it, Ctrl-W the word before it and Ctrl-U the line up to it; Option-Backspace and Command-Backspace do the same where the terminal reports them apart from Backspace, which takes the same protocol for the Command one. Pasted text keeps its line breaks. `slack send` with stdin takes a long message. In the box, `@` and `#` complete people and channels: a list opens above the box as you type, Up/Down move in it, Tab or Enter take the highlighted entry, Esc closes it. On sending, from the box or from `slack send`, an `@handle` or a `#channel` the name cache knows becomes a real mention; `@here` and `@channel` stay plain text.

Ctrl-G in the box corrects what you typed: the accents a QWERTY keyboard leaves out, the typos and the grammar, never the wording. The correction replaces the text in the box for you to read before Enter sends it, and Ctrl-G again, before any other key changes the text, brings back what you typed. The model is a command of your choice, set in `~/.config/slack/config.toml`; it reads the instructions and your text on stdin and prints the corrected text, and runs through `sh -c` from the temporary directory. With Claude Code:

```toml
fix_command = "claude -p --model haiku --tools '' --system-prompt 'You correct text and reply with the corrected text alone.' --no-session-persistence --strict-mcp-config"
```

The flags keep `claude` a plain model rather than an agent: no tools, no MCP servers, its own system prompt, no session saved. A correction takes two to four seconds with Haiku. Your draft goes to whoever runs the model, Anthropic here; `ollama run <model>` keeps it on your machine.

Without a terminal on both ends, as in a pipe or a script, `slack` prints what `slack unreads` prints instead of drawing.

## Sending

A target is `#channel`, `@person` (handle or display name), a conversation id, or a Slack permalink; a permalink means "answer in that message's thread", and `--thread <ts|permalink>` does the same for a channel target. The text is the argument, or stdin when there is none, and is sent as Slack markup (`*bold*`, `<@U123>`). The command prints where the message landed and its permalink. Quote `#channel` in zsh, or the shell reads it as a comment.

`slack react <message> <emoji>` puts an emoji on a message, and `--remove` takes yours back. The message is a permalink, a thread number (its first message), or a conversation with `--ts <ts>`; the emoji is its Slack name, with or without the colons (`+1`, `:white_check_mark:`, a custom one), or the character itself. Reacting never marks anything as read.

## How login works

`slack login` starts your Chrome against a profile of its own (`~/.config/slack/browser-profile`), opens Slack, and waits for you to sign in. It then reads the two values the Slack web client itself uses, the session cookie and the per-workspace token, over the DevTools Protocol, checks them against Slack, and saves every workspace you are signed into. The browser closes by itself.

The profile is separate from your everyday browsing because Chrome refuses remote debugging on the default profile. It persists, so only the first login needs you: `slack login --headless` refreshes the tokens afterwards with no window, which is what a cron or launchd job can call when a session expires.

Security notes, the short version:

- The CLI acts as you, with exactly your permissions, and every request is attributed to you like a click in the app.
- `~/.config/slack/workspaces.json` (mode `0600`) holds the credentials. The browser profile next to it is a credential store too: while it exists, `login --headless` can mint working tokens with no prompt. `slack logout` deletes both, and the files the screen downloaded; `--keep-browser-session` keeps the profile.
- Token values are never printed, logged or included in `--json`.
- Only `https://*.slack.com` origins are ever paired with the session cookie.

## Status bar

`slack --count` makes two API calls side by side, prints one number and exits non-zero when the fetch fails, so a poller can tell "nothing" from "broken". The number is the unread conversations (DMs, group messages and channels with something new, muted ones excepted unless you were mentioned) plus one when followed threads have unread replies.

`slack --count --json` prints `{"unread": 3, "me": 2, "notifying": false}` from the same two calls, laid out over lines like every `--json` output, so read it with `jq` rather than line by line: `unread` is that number, `me` the messages aimed at you (the unread messages of DMs and group messages, the mentions in channels and in followed threads), for a bar that shows the urgent part apart. `notifying` is true while a screen is open with its socket up and posts the notifications above, so a status bar that notifies on its own when `me` goes up can keep quiet meanwhile and take over once the screen is closed.

## For an agent

`skills/slack/SKILL.md` is the manual an LLM agent reads before using the CLI: what is free to run, what needs the user's word (`send`, `read`), how the numbers and the JSON work. For Claude Code, link it from a clone of the repository and `/slack` becomes a skill:

```bash
ln -s "$PWD/skills/slack" ~/.claude/skills/slack
```

`tools/draft-pane/` is a Claude Code mod that goes with it: a draft the skill writes opens in a pane beside the chat, where you edit it and press Send; Enter never sends. `/slack-draft` opens the pane again. Fix corrects the spelling, the accents and the grammar, Slack markup and mentions left alone, through a command of yours that reads the instructions and the draft on stdin and prints the correction (`claude -p --model haiku` will do); Enter in the field does the same, and Undo fix puts back what you typed. Set it as the `Fix command` row in `/config`, or under `pluginConfigs` in `~/.claude/settings.json`. Load it for one session, or for every session through the `env` block of `~/.claude/settings.json`:

```bash
claude --plugin-dir "$PWD/tools/draft-pane"
```

```json
{ "env": { "CLAUDE_CODE_PLUGIN_DIRS": "/path/to/slack/tools/draft-pane" } }
```

## Files

`$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`): `workspaces.json`, `browser-profile/`, and `config.toml` if you write one (its one setting is `fix_command`, above). `$XDG_CACHE_HOME/slack/` (default `~/.cache/slack/`) holds the caches, all safe to delete: names, the last view, and the files the screen opened. `$SLACK_BROWSER_PROFILE` overrides the profile location; `logout` only ever deletes a profile the CLI created.

## Releasing

A release is a pull request that bumps `version` in `Cargo.toml` (`cargo build` so `Cargo.lock` follows). Once it is merged, tag its merge commit with an annotated tag whose message is the tag name, a blank line and the release notes, and push the tag:

```bash
git fetch && git tag -a v0.8.0 --cleanup=whitespace -F notes.txt origin/main && git push origin v0.8.0
```

The [Release workflow](.github/workflows/release.yml) checks that the tag matches the version and carries notes, lints and tests, builds the macOS arm64 and Linux x86_64 binaries, publishes them as a GitHub Release with the tag's notes, the compare link to the previous release and a `checksums.txt`, then asks the [Homebrew tap](https://github.com/denrou/homebrew-slack) to pick the release up.

## License

MIT
