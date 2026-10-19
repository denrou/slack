# slack

Slack from the terminal, in the spirit of [gh-prs](https://github.com/denrou/gh-prs): `slack` shows what needs you, `slack --count` gives a status bar one number, `slack send` answers. No Slack app and no admin approval: the CLI reuses the session of a browser you sign into once, the way [slackcli](https://github.com/shaharia-lab/slackcli) does.

Early days. Today `login`, `logout`, `whoami` and `--count` work; the unread view, `send` and `read` come next ([PLAN.md](PLAN.md) has the plan and the steps).

## Install

```bash
cargo install --git https://github.com/denrou/slack
```

Needs Rust 1.85+ and Google Chrome (Chromium, Edge or Brave also work; `$SLACK_BROWSER` points at an executable the lookup does not know).

## Quickstart

```bash
slack login     # a Chrome window opens; sign in to Slack once
slack whoami    # which workspace and user the CLI acts as, verified
slack --count   # conversations that need you, as one number
slack           # soon: the unread DMs, mentions and threads
```

## How login works

`slack login` starts your Chrome against a profile of its own (`~/.config/slack/browser-profile`), opens Slack, and waits for you to sign in. It then reads the two values the Slack web client itself uses, the session cookie and the per-workspace token, over the DevTools Protocol, checks them against Slack, and saves every workspace you are signed into. The browser closes by itself.

The profile is separate from your everyday browsing because Chrome refuses remote debugging on the default profile. It persists, so only the first login needs you: `slack login --headless` refreshes the tokens afterwards with no window, which is what a cron or launchd job can call when a session expires.

Security notes, the short version:

- The CLI acts as you, with exactly your permissions, and every request is attributed to you like a click in the app.
- `~/.config/slack/workspaces.json` (mode `0600`) holds the credentials. The browser profile next to it is a credential store too: while it exists, `login --headless` can mint working tokens with no prompt. `slack logout` deletes both; `--keep-browser-session` keeps the profile.
- Token values are never printed, logged or included in `--json`.
- Only `https://*.slack.com` origins are ever paired with the session cookie.

## Status bar

`slack --count` makes one API call, prints one number and exits non-zero when the fetch fails, so a poller can tell "nothing" from "broken". The number is the conversations that need you (DMs with unreads, channels where you were mentioned) plus one when followed threads have unread replies. `slack --count --all` counts every conversation with unreads instead.

## Files

`$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`): `workspaces.json` and `browser-profile/`. `$SLACK_BROWSER_PROFILE` overrides the profile location; `logout` only ever deletes a profile the CLI created.

## License

MIT
