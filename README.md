# slack

Slack from the terminal, in the spirit of [gh-prs](https://github.com/denrou/gh-prs): `slack` shows what needs you, `slack --count` gives a status bar one number, `slack send` answers. No Slack app and no admin approval: the CLI reuses the session of a browser you sign into once, the way [slackcli](https://github.com/shaharia-lab/slackcli) does.

Early days: every command below works, a release workflow is the next step ([PLAN.md](PLAN.md) has the plan).

## Install

```bash
cargo install --git https://github.com/denrou/slack
```

Needs Rust 1.85+ and Google Chrome (Chromium, Edge or Brave also work; `$SLACK_BROWSER` points at an executable the lookup does not know).

## Quickstart

```bash
slack login     # a Chrome window opens; sign in to Slack once
slack whoami    # which workspace and user the CLI acts as, verified
slack           # what needs you: DMs, mentions, unread threads
slack -a        # ...plus every other channel with unreads
slack --count   # the same, as one number for a status bar
slack --json    # the same data, for scripts

slack send '#sre-alerts' "on it"                    # post to a channel
slack send @jane "merci !"                          # ...or a DM
slack send <permalink> "answering here"             # ...or a thread
slack send @jane < note.md                          # text from stdin
slack read                                          # mark as read the DMs and mentions the last view showed
slack read '#sre-alerts' @jane                      # ...only these
slack read --all                                    # ...also the channels the last `slack -a` listed
```

## The view

`slack` shows what needs you, in the order a person answers it: **Direct messages** (DMs and group DMs with unreads), **Mentions** (channels where you were named), **Threads** (followed threads with new replies). Under each conversation come the unread messages themselves, oldest first, with the author, the local time, and notes for files, attachments and replies. Channel chatter without a mention is counted but hidden; a line on stderr says how many channels were held back, and `slack -a` lists them with their messages. Channels you muted in Slack are left out altogether, as the app does; a mention in a muted channel still counts.

Reading in the terminal never moves Slack's read cursor: the view is read-only, and `slack read` marks as read the DMs and mentions the last view showed (no more: a message that arrived since stays unread). The channels a `slack -a` listed are a wider net, so they take `slack read --all` or their name. Names are cached in `~/.cache/slack/names.json`, safe to delete.

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

`slack --count` makes one API call, prints one number and exits non-zero when the fetch fails, so a poller can tell "nothing" from "broken". The number is the conversations that need you (DMs with unreads, channels where you were mentioned) plus one when followed threads have unread replies. `slack --count --all` counts every conversation with unreads instead, muted ones excepted.

## Files

`$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`): `workspaces.json` and `browser-profile/`. `$SLACK_BROWSER_PROFILE` overrides the profile location; `logout` only ever deletes a profile the CLI created.

## License

MIT
