# slack — plan

A Rust CLI to read and answer Slack from the terminal, in the spirit of
`gh-prs`: one command shows what needs you, one option prints a count for a
status bar, one subcommand closes the loop. Authentication is borrowed from
[slackcli](https://github.com/shaharia-lab/slackcli): reuse the signed-in
browser session, no Slack app, no admin.

This file is the plan agreed before the first line of code. Once the project
has a `CLAUDE.md` and a `README.md` it can be deleted.

## Situation

- slackcli does the job but carries ~70 commands (canvases, emoji, drafts,
  scheduling, Claude plugin, update checker, redacted logs). The three things
  wanted here are a small subset: show unread, send, count.
- It is TypeScript on Bun. Trying Rust is part of the goal: see whether the
  feel of use (startup, single binary) and of development (with Claude)
  changes.
- slackcli 0.16.0 is installed and logged in on this machine (Centreon,
  browser auth, keychain backend). It stays the reference to compare output
  and to check Slack's undocumented endpoints when one misbehaves. The new
  tool keeps its own config and store; nothing is shared.
- Chrome is installed, Rust 1.99 toolchain is installed, no `slack` binary on
  the PATH yet.

## What is kept from slackcli, and what is not

Kept (the "I like how it handles authentication" part):

- **Browser session credentials**: the `d` cookie (`xoxd-…`) plus the
  per-workspace client token (`xoxc-…`). Every request is then a plain
  `POST https://<team>.slack.com/api/<method>` with
  `Cookie: d=<urlencoded xoxd>`, `Origin: https://app.slack.com`, a
  `User-Agent`, and `application/x-www-form-urlencoded` body with
  `token=<xoxc>` plus the method params. Response is JSON with `ok`.
- **Capture over the Chrome DevTools Protocol**: launch the user's Chrome
  with `--remote-debugging-port=0 --user-data-dir=<own profile>
--no-first-run --no-default-browser-check
--disable-blink-features=AutomationControlled`, read the port from
  `DevToolsActivePort`, `GET /json/list` for the `page` target, open the
  WebSocket, `Network.enable` + `Runtime.enable` + `Page.navigate` to
  `https://app.slack.com/client` (not the root: it redirects to marketing).
  Then two sources, polled every 500 ms until one hits or the timeout
  (5 min) elapses:
  - `Network.requestWillBeSent` events: an `xoxc-…` token in the body of any
    `https://<team>.slack.com/api/*` request (urlencoded, JSON, or
    multipart);
  - `Runtime.evaluate("localStorage.getItem('localConfig_v2')")`: the
    `teams` map lists every signed-in workspace with its `token`, `url`,
    `id`, `name`.
    Finally `Storage.getCookies` (fallback `Network.getCookies`) for the `d`
    cookie on a `slack.com` domain, then `Browser.close`.
- **The dedicated browser profile is the long-lived credential**: Chrome
  136+ refuses remote debugging on the default profile, so the first login
  needs a human in the window; afterwards `--headless` re-mints tokens
  silently. The profile directory is marked with a sentinel file so logout
  only ever deletes a directory we created.
- **Security gates**: only `https://*.slack.com` origins (never
  `app.slack.com`) may be paired with the cookie; a start URL must not start
  with `-`; tokens never appear in output, logs, or `--json`; config
  directory `0700`, files `0600`.
- **Multi-workspace store shape**, keyed by team id, with a default. The
  capture gives every workspace for free; the CLI only uses the default for
  now.

Deliberately not kept, at least in v1: app tokens (`xoxb`/`xoxp`), the
`parse-curl` and manual-token paths, drafts, scheduling, canvases, emoji,
files, saved items, search, user groups, update checker, log files,
Windows. If one of these becomes needed, it is a feature, not a port.

## Commands (v1)

```
slack                      # what needs you: DMs, mentions, unread threads
slack -a / --all           # ...plus every other conversation with unreads
slack --count              # one number for the status bar (attention count)
slack --count --all        # every conversation with unreads
slack --json               # the same data, for scripts
slack --no-color

slack send <target> [text] # post a message; text from args or stdin
slack send <target> --thread <ts|permalink> [text]
slack read [<target>...]   # move the read cursor; no args = everything shown last time

slack login [--headless]   # browser capture (see above)
slack logout [--keep-browser-session]
slack whoami               # workspace, user, and whether the token still works
```

`<target>` is `#channel`, `@user` (handle or display name), a conversation id
(`C…`, `D…`, `G…`), or a Slack permalink.

### The default view

One `client.counts` call gives, per conversation, `has_unreads`,
`mention_count`, `last_read`, `latest`, plus a `threads` block
(`has_unreads`, `mention_count`). That drives both the count and the view.

Sections, in the order a person would answer them:

1. **Direct messages** — DMs and group DMs with unreads. Someone wrote to
   you.
2. **Mentions** — channels where `mention_count > 0`.
3. **Threads** — followed threads with unread replies
   (`subscriptions.thread.getView`, paginated on `current_ts`, 10 per page,
   capped at 5 pages; undocumented endpoint, so a shape change degrades to a
   one-line "unread thread activity" instead of failing the run).
4. **Channels** — every other conversation with unreads. Hidden by default
   (`-a` shows it); the default view ends with a dim "N channels with
   unreads hidden" line on stderr, so hiding is visible, never silent.

For sections 1–3 the unread messages themselves are fetched
(`conversations.history` with `oldest=last_read`, oldest first, capped at 20
per conversation, "N more" when capped) and rendered:

```
Direct messages
  @jane  (2 unread)
    14:02  can you look at the 4.6 deploy?
    14:05  no rush, SRE deployed yesterday
  @bob and @alice  (1 unread)
    09:41  @bob: merged, thanks

Mentions
  #sre-alerts  (3 unread, 1 mention)
    13:10  @denis the predictions job is 24h late again
    ...

Threads
  #data-science › "monthly update draft"  (2 new replies)
    11:20  @claire: added the Q4 numbers
    11:32  @marc: lgtm
```

Rendering rules: `<@U…>` → `@name`, `<#C…|name>` → `#name`,
`<url|text>` → `text (url)`, HTML entities decoded, `*bold*`, `_italic_`,
`~strike~`, `` `code` `` mapped to ANSI, files as `[file: name]`, bot
messages by bot name. Times are local `HH:MM`, with the day prefixed when
it is not today. Names come from a local cache (below), never from one
`users.info` call per message.

### The count

`slack --count` prints the number of conversations in sections 1 and 2 plus
1 if the threads block has unreads, on one line, and exits non-zero when
the fetch fails, so a status bar can tell "nothing" from "broken" (the
`gh-prs --count` contract). It makes exactly one API call and no name
resolution, so it is cheap enough to poll every few seconds.

`slack --count --all` counts every conversation with unreads.

### Marking as read

The default view is read-only. `slack read` writes `conversations.mark`
with each conversation's `latest` from the last view, so the Slack app, the
footer count and the CLI agree afterwards. `slack read #sre-alerts @jane`
marks only those. The last view is remembered in a cache file for this
(see Storage); if the cache is missing, `slack read` with no argument says
so and does nothing.

### Sending

`slack send #channel "text"`, `slack send @jane < note.md`, `slack send
#channel --thread <permalink> "reply"`. Resolution: `#name` and `@name`
through the name cache, filling it from `conversations.list` /
`users.list` on a miss (public + private channels, archived excluded);
`@user` opens the DM with `conversations.open`. A permalink gives the
channel and the thread ts directly. `chat.postMessage` with `as_user`
semantics of the browser session, `link_names` off, text sent as-is
(mrkdwn). Prints the permalink of the posted message.

## Storage

All under `$XDG_CONFIG_HOME/slack/` (default `~/.config/slack/`), directory
`0700`:

- `workspaces.json` (`0600`) — `{ "default": "T…", "workspaces": { "T…": {
"name", "url", "user_id", "xoxc", "xoxd" } } }`. Credentials included, a
  plain file like slackcli's default backend. The macOS Keychain was
  considered and dropped for now: the footer polls `slack --count` from a
  launchd job, and the Keychain prompts whenever the binary changes, which
  is every rebuild during development. The browser profile next to it is a
  plaintext credential store anyway. A keychain backend can come later as
  an opt-in.
- `browser-profile/` (`0700`) — the Chrome profile, with a
  `.slack-browser-profile` sentinel holding a format version.

Under `$XDG_CACHE_HOME/slack/` (default `~/.cache/slack/`):

- `names.json` — `{ users: { "U…": { name, display, real } }, channels: {
"C…": { name, is_private, is_im, is_mpim, user } } }`. Filled on demand,
  refreshed per id when a name is missing. Safe to delete at any time.
- `last-view.json` — `{ "T…": { "C…": "<latest ts shown>" } }`, written by
  the default view, read by `slack read`.

No `config.json` in v1: there is no setting yet. One appears when a real
one is needed (a mute list for noisy channels is the likely first).

## Architecture

One crate, binary `slack`, modules grouped by what they know:

```
src/
  main.rs        clap definitions, dispatch, exit codes (1 error, 130 Ctrl-C)
  api.rs         the one transport: POST form + cookie, ok/error envelope,
                 retry on 429 honouring Retry-After, auth errors mapped to
                 "run `slack login`"
  auth/
    store.rs     workspaces.json (metadata and credentials), atomic writes
    chrome.rs    find the browser, launch with its own profile, read the
                 DevTools port, stop it (SIGTERM, then SIGKILL, then a
                 pkill on --user-data-dir), sentinel ownership
    cdp.rs       minimal CDP client over a WebSocket: send(method, params)
                 with ids and a 30 s timeout, events by method name
    capture.rs   the capture policy (two sources, poll, settle, cookie);
                 pure extractors for xoxc-in-body, d-cookie, localConfig_v2
  counts.rs      client.counts → Vec<Conversation> (pure, fixture-tested)
  threads.rs     subscriptions.thread.getView pages → Vec<Thread> (pure
                 normalisation + the paging loop)
  history.rs     unread messages of a conversation from its cursor
  names.rs       id↔name cache and on-miss resolution
  mrkdwn.rs      Slack markup → ANSI / plain (pure)
  view.rs        sections, grouping, rendering, --json
  send.rs        target resolution, chat.postMessage
  read.rs        conversations.mark from last-view.json
```

Pure modules (`counts`, `threads`, `mrkdwn`, `capture` extractors, the
section logic in `view`) take JSON values or structs and are unit-tested
against fixtures captured from real responses with tokens and names
redacted. The transport is one function; the commands never see the
headers.

Error direction, as in gh-prs: an error must never look like "nothing to
do". A failed `client.counts` fails the run; a failed thread view degrades
to the summary line with a warning on stderr; an unknown shape in a
fixture-tested module is a test, not a guess.

## Crates

`clap` (derive), `serde` + `serde_json`, `ureq` (blocking HTTP, rustls),
`tungstenite` (blocking WebSocket for CDP, no TLS feature: CDP is loopback
`ws://`), `percent-encoding` (the cookie value), `owo-colors` (respects
`NO_COLOR`), `jiff` (local time), `anyhow` at the edges, `thiserror` for
the few typed errors (`AuthError` → "run slack login"). No `dirs`: on
macOS it answers `~/Library/Application Support`, while gh-prs and
slackcli live in `~/.config`, so the XDG lookup is ten lines by hand.

Blocking rather than async: the whole tool is a handful of sequential HTTP
calls, the CDP loop is one socket, and the parallel part (resolving ~20
names on a cold cache) is a `std::thread::scope`. No tokio keeps the
binary small, the build fast, and the code readable in one pass.

## Steps

Each step is a commit on `main` that leaves the tool usable, as in gh-prs.

1. **Skeleton** (done 2026-10-19) — `cargo init`, clap with the commands above as stubs,
   `--version`, MIT licence, `.gitignore`, `README.md` with the four-line
   quickstart, `CLAUDE.md` with the personal-project workflow (commit on
   main, push is the user's move, offer a release). Repo `denrou/slack`,
   public, created with `gh repo create`; the first push is yours.
2. **Transport + store + whoami** (done 2026-10-19) — `api.rs`, `auth/store.rs`,
   `slack whoami` against credentials pasted by hand into the store (so the
   transport is proven before the browser work starts). `auth.test` is the
   check.
3. **Login** (done 2026-10-19) — `chrome.rs`, `cdp.rs`, `capture.rs`, `slack login`,
   `slack logout`. Extractors fixture-tested first. Compared against
   slackcli's capture on the same profile semantics (separate profile dir).
4. **Count** (done 2026-10-19) — `counts.rs` and `slack --count`. Fixture from a real
   `client.counts`. This is the first thing wired into the status line.
5. **View** (done 2026-10-19) — `names.rs`, `history.rs`, `mrkdwn.rs`, `view.rs`, the
   default view and `-a`, `--json`, `--no-color`, `last-view.json`.
6. **Threads** (done 2026-10-19) — `threads.rs`, section 3.
7. **Send and read** (done 2026-10-19) — `send.rs`, `read.rs`.
8. **Release** — tag-driven GitHub Release with a macOS arm64 binary
   (`cargo build --release`, strip), `cargo install --git` documented;
   a Homebrew tap (`denrou/homebrew-slack`) only once the tool has settled.

Steps 2–4 are where Rust's feel will show first (an HTTP call, a JSON
envelope, a process launch, a WebSocket). If it is unpleasant
there, that is the moment to reconsider, before the view work.

## Decisions (2026-10-19)

1. **Blocking, not async.** `ureq` + `tungstenite`. Switching later would
   cost the transport and the CDP module, not the pure modules.
2. **Credentials in `workspaces.json`, mode `0600`.** See Storage for why
   the Keychain waits.
3. **Mark-as-read is explicit** (`slack read`), never a side effect of the
   view, so the footer count keeps agreeing with the Slack app.
4. **Threads in v1**, as step 6.
5. **Binary name `slack`**, kept because it is fast to type. Slack's own
   platform CLI (`brew install slack-cli`) also installs a `slack` binary;
   it is not on this machine.
6. **Public repo under `denrou`, MIT**, like gh-prs.

The footer is a launchd job running `gh-prs --count` on an interval, and
`slack --count` will join it: one call, one number, non-zero exit on
failure, and never a prompt.

Later, not planned: a TUI to move through and answer the messages
(`ratatui` on top of the same view model, which is why `view.rs` keeps the
model apart from the rendering), numbered replies (`slack send 3 "ok"`
answering item 3 of the last view), `--workspace`, Linux browser paths, a
`watch` mode. A local mute list is not needed: the channels muted in Slack
itself are honoured (added 2026-10-19).
