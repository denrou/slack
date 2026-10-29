# slack

Slack from the terminal: `slack` prints what needs you, `slack --count` gives a status bar one number, `slack send` answers, `slack react` puts an emoji on a message, `slack read` marks as read, `slack status` sets your status, `slack show` reads any conversation, `slack search` finds a message, `slack who` says who is around, `slack watch` hands the messages aimed at you to a command of yours. No Slack app and no admin approval: the CLI reuses the session of a browser you sign into once, the way [slackcli](https://github.com/shaharia-lab/slackcli) does.

Early days, but every command below works.

## Install

With [Homebrew](https://brew.sh/), on macOS (Apple Silicon) or Linux (x86_64):

```bash
brew install denrou/tap/slack
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
slack status Lunch -e 🍔 --until 1h                 # set your status, cleared by Slack in an hour
slack status --clear                                # ...or clear it; bare, it prints the current one

slack watch -- ~/bin/answer                         # hand each DM and mention of you to a command, log what it would answer
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

`slack status <text> -e <emoji> --until <when>` sets your status. `--until` is when Slack clears it, on your clock: `45m`, `2h`, `07:30` (the next one), `mon 07:30` (the next Monday), `2026-11-02 07:30`; without it the status stays. Slack clears it itself, so a scheduled job only has to set it, and a Mac asleep at that hour changes nothing. `--if-unset` leaves a status already set alone, so such a job never replaces one you set by hand. `--clear` clears it, and `slack status` alone prints it (`--json`: `emoji`, `text`, `expires`, or `null`).

## Watch

`slack watch -- <command> [args]` stays connected to Slack's real-time socket and, for each direct message and each message that names you (`@you`, not `@here`, `@channel` or a group), runs the command with the exchange as JSON on stdin. Whatever the command prints is the reply it proposes; printing nothing means no reply. It is a dry run: **nothing is ever sent**, and the command gets no access to Slack, only the text. Everything goes to `~/.local/state/slack/watch.jsonl` (mode `0600`), one JSON record a line:

- `event`: the exchange handed over, the command's `outcome` (`reply`, `no_reply`, `failed`), the `reply`, and `would_send`, the reply as it would go out, behind the marker `🤖 Réponse automatique :` the CLI adds itself;
- `your_reply`: what you wrote in that place in the 12 hours after, to compare with what the command proposed;
- `answered_first`: you wrote there before the sender's burst settled, so it was not handed over;
- `gap`: the socket dropped, and what was posted meanwhile was not heard;
- `error`: the exchange could not be read.

A burst is one event: the command runs once the sender has been quiet for `--settle` seconds (60), and is stopped after `--timeout` (600) with what it started. A reply over 2,000 characters is logged as a failure. Your own messages, Slackbot, bots and apps, and DMs you muted in Slack never trigger it. Without a command, the events are only logged.

The JSON on stdin is `id`, `reason` (`direct_message` or `mention`), `workspace`, `conversation` (`id`, `kind`: `direct_message`, `group_message`, `channel` or `private_channel`, `name`), `thread_ts`, `permalink`, `sender` and `me` (as `slack who --json` describes a person: `presence`, `status`, `tz`, `local_time`, `notifications_paused_until`; `sender` adds `real_name` and `external`, true for someone from another organisation or whose team Slack did not give), and `messages`: up to 20 of the conversation or thread, oldest first, ending with the sender's last message of the burst, so the command never sees what came after, your own answer included. Each message is `ts`, `time` (your clock), `from`, `user`, `text` (mentions spelled out as names), and `me`, `bot`, `trigger`, `files` where they apply.

`tools/answer/answer` is such a command, the author's. It answers only when the reply does something: a pull request to review that has not been through the Centreon automated review gets asked for it first, an urgent message gets the time Slack will notify you, and someone playing with the bot gets a joke, once; the rest waits for you. Three `claude -p` calls (a judge, a responder, a reviewer that can only reject) write it, with what the reply rests on (the pull requests, the time) and the rules that should not rest on a model kept in code. It needs [Claude Code](https://claude.com/claude-code), `gh` and `jq`, and a list of what a reply must never share in `~/.config/slack/answer/private.md` (start from `tools/answer/private.example.md`); `answer.jsonl`, next to the watch log, keeps what each step decided.

Under launchd, a `KeepAlive` job runs it at login; the Mac has to stay awake for it to hear anything.

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

`tools/draft-pane/` is a Claude Code mod that goes with it: a draft the skill writes opens in a pane beside the chat, where you edit it and press Send; Enter never sends. `/slack-draft` opens the pane again. Fix corrects the spelling, the accents and the grammar, Slack markup and mentions left alone, through a command of yours that reads the instructions and the draft on stdin and prints the correction (`claude -p --model haiku` will do); Enter in the field does the same, and Undo fix puts back what you typed. Set it as the `Fix command` row in `/config`, or under `pluginConfigs` in `~/.claude/settings.json`. Load it for one session, or for every session through the `env` block of `~/.claude/settings.json`:

```bash
claude --plugin-dir "$PWD/tools/draft-pane"
```

```json
{ "env": { "CLAUDE_CODE_PLUGIN_DIRS": "/path/to/slack/tools/draft-pane" } }
```

## Files

`$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`): `workspaces.json` and `browser-profile/`. `$XDG_CACHE_HOME/slack/` (default `~/.cache/slack/`) holds the caches, both safe to delete: names and the last view. `$XDG_STATE_HOME/slack/` (default `~/.local/state/slack/`) holds the watch log, and the answer command's next to it. `$SLACK_BROWSER_PROFILE` overrides the profile location; `logout` only ever deletes a profile the CLI created.

## Releasing

A release is a pull request that bumps `version` in `Cargo.toml` (`cargo build` so `Cargo.lock` follows). Once it is merged, tag its merge commit with an annotated tag whose message is the tag name, a blank line and the release notes, and push the tag:

```bash
git fetch && git tag -a v0.8.0 --cleanup=whitespace -F notes.txt origin/main && git push origin v0.8.0
```

The [Release workflow](.github/workflows/release.yml) checks that the tag matches the version and carries notes, lints and tests, builds the macOS arm64 and Linux x86_64 binaries, publishes them as a GitHub Release with the tag's notes, the compare link to the previous release and a `checksums.txt`, then asks the [Homebrew tap](https://github.com/denrou/homebrew-tap) to pick the release up.

## License

MIT
