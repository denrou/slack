# Architecture

How `slack` is put together, for a first read of the code. README.md says what the tool does; CLAUDE.md says how it is worked on.

## Shape

One crate, one binary, blocking I/O throughout: a run is a handful of HTTP calls (`ureq`), the login holds one WebSocket (`tungstenite`), and the few lookups that run side by side go through `std::thread::scope` (`par.rs`, eight threads at most). No async runtime, deliberately: switching later would cost `api.rs` and `auth/cdp.rs`, not the pure modules.

Each module knows one thing, and the dependencies point down this list:

1. **Entry points.** `main.rs` parses the command line (clap) and dispatches. `commands.rs` has one function per command and prints the output (`send.rs` and `read.rs` print their own outcome).
2. **One module per thing asked of Slack.** `counts`, `prefs`, `history`, `threads`, `replies`, `directory`, `presence`, `target`, `search`, `send`, `react`, `read`. Each turns one or two API methods into a typed value and knows nothing about printing.
3. **The shared model.** `view.rs` builds a `View` from the counts; the text rendering and `--json` both read that same value. `mrkdwn.rs` and `style.rs` turn Slack markup into terminal text, `emoji.rs` an emoji name into its character.
4. **Files on disk.** `store.rs` (credentials), `names.rs` (the id-to-name cache), `lastview.rs` (what the last output showed), all through `paths.rs`.
5. **Transport.** `api.rs`: `Client::call(method, params)` and nothing else. No module above it sees a header or a token.
6. **Login.** `auth/`, used by `login` and `logout` only: `chrome.rs` runs the browser, `cdp.rs` talks to it, `capture.rs` picks the credentials out of what it sees.

## A run, step by step

**`slack unreads`, the view.** `Workspaces::load` picks the default workspace and builds its `Client`. `counts::fetch` calls `client.counts` and `users.prefs.get` in two threads and flags the muted conversations. `view::build` sorts the unread conversations into direct messages, mentions and channels, reads the unread part of each (`history::fetch`, `conversations.history` from the read cursor, through `par::map`) and the followed threads (`threads::fetch`), then fills the names it needs from the cache (`users.info` and `conversations.info` on a miss). `lastview::remember` records what was shown; a failure there is a warning, the view still prints. Then `--json` serialises the `View`, or `view::render` prints it.

**`slack --count`.** `counts::fetch` and `Counts::unread_count` (with `--json`, `Counts::me_count` beside it), nothing else: no names, no cache file, two calls side by side. Any failure exits non-zero with a message on stderr, so a status bar can tell "nothing" from "broken".

**`slack send`.** `target::resolve` turns the argument into a channel id and maybe a thread: a number from the last output (`lastview::thread_ref`), a permalink, a bare id, `#channel` or `@person` through the names cache with a `conversations.list` or `users.list` walk on a miss (`@person` opens the DM with `conversations.open`). Then `mention::link` on the text, `chat.postMessage`, and `chat.getPermalink` best effort for the line printed.

**`slack react`.** A permalink names the message itself (not its thread, as for `send`); anything else goes through `target::resolve`, with `--ts` for the message, or a thread number's root. `emoji::name_of` turns `:+1:` or 👍 into Slack's name, then `reactions.add` or `reactions.remove` (`react::set`).

**`slack read`.** `lastview::recall` gives the conversations of the last output with the newest message each had. Without arguments the ones the view listed are taken; with arguments each is resolved and must be in the record. A fresh `client.counts` gives the current cursors: one already past the recorded message is left alone (a cursor only moves forward), the others get `conversations.mark` at the recorded message. The marked ones are dropped from the record.

**`slack show`, `slack thread`.** `show` resolves the target, reads the newest page (`history::fetch` without a cursor) and `conversations.info` for the read cursor side by side, records the conversation and the threads it numbered (`lastview::remember_conversation`) and prints with a `── new ──` line at the cursor. `thread` takes a number or a permalink, reads `conversations.replies` (`replies::fetch`) and prints the root, then the replies.

**`slack search`.** `search::fetch` calls `search.messages` (newest first, one page of `-n`, 100 at most) and takes each match's thread from its permalink, since a match carries no `thread_ts`; `search::name` fills the names the matches need. Each prints under its conversation through `view::render_messages`, its permalink under it; how many matched beyond the page goes to stderr.

**`slack channels`, `slack people`.** `directory.rs`. The conversations you are in are the `client.counts` ids, named from the cache (one `users.conversations` walk fills it). The people are those you have a DM with, or the whole directory (`users.list`) when a filter is given.

**`slack who`.** The people named (`@person` through the names cache), the members of a `#channel` (`directory::members`, one page of `conversations.members`, refused past 100; bots and you dropped, then refused past 50), else the people of the DMs in `Counts::recent`. Then `presence::profiles` (`users.info` per person, a few at a time, and one `dnd.teamInfo` for all) and `presence::presences` (`users.getPresence` per person); the ones there first. `Person::notes` writes what follows each name.

**`slack login`.** `chrome::launch` starts the browser on the CLI's own profile with remote debugging on a free port; `cdp::Session` connects to the page; `capture::capture` loads Slack, waits for the sign-in, and collects the `d` cookie, the `xoxc` token from the web client's own requests and the workspace list from `localConfig_v2`; `auth.test` checks each workspace and `Workspaces::upsert` saves them. `logout` deletes the store and the profile when the sentinel says the CLI created it.

## Module by module

- `main.rs`: clap definitions and dispatch. No subcommand is `slack unreads`; `--count` is the status-bar path.
- `commands.rs`: one function per command, where the output is printed. `show` reads a conversation's recent history (`conversations.history` without a cursor, `conversations.info` for the read cursor that places the `── new ──` line) and records it in `lastview` so `slack read <name>` and the thread numbers work from that output.
- `api.rs`: the one transport. A form POST to `<workspace url>/api/<method>` with `token=<xoxc>` in the body and `Cookie: d=<xoxd>`, the way the web client does it. Parses the `ok` envelope, maps `invalid_auth` and friends to `Error::Auth` ("run `slack login`"), retries HTTP 429 twice honouring `Retry-After` (capped at 30 s). Commands never see headers or tokens.
- `store.rs`: `~/.config/slack/workspaces.json`, workspaces keyed by team id with their credentials, written atomically with mode `0600`. `Workspace` has no `Debug` so a token can never land in a log.
- `paths.rs`: XDG lookup (`~/.config/slack`, not `~/Library/Application Support`) and owner-only file writes.
- `auth/chrome.rs`: find Chrome, launch it with `--remote-debugging-port=0` on our own profile, read `DevToolsActivePort`, find the page target, shut down (SIGTERM, SIGKILL, then `pkill -f user-data-dir=<profile>` for the helpers Chrome leaves behind). The profile is owned through a sentinel file; `logout` deletes only an owned profile.
- `auth/cdp.rs`: a hand-rolled CDP session over one WebSocket: numbered commands, buffered events, read timeouts on the socket so a poll never blocks.
- `auth/capture.rs`: the capture policy and its pure extractors (`xoxc` from a request body in the three encodings Slack uses, the `d` cookie, the workspace list in `localConfig_v2`, the origin gate). Ported from slackcli's `browser-auth.ts`; the tests pin the parsing rules.
- `counts.rs`: `client.counts` into `Conversation`s and the `Threads` block. Every conversation with something new counts, plus one for threads; a muted one counts for a channel mention only, like the app's badges. `fetch` also reads the notification preferences (`prefs.rs`, `users.prefs.get`, the `muted` flag inside the `all_notifications_prefs` JSON string) in a second thread, and the conversations visited last (`recents`, a JSON string whose `navigation` list is the app's own recent list from every client, into `Counts::recent` for `slack who`). When they cannot be read, a warning says so, nothing reads as muted and `Counts::prefs_missing` is set. No local lists: Slack's own mute list is the source, so the way to keep a channel out of the view is to mute it in Slack.
- `view.rs`: the model (`View`: direct messages, mentions, channels, threads) built from the counts, then rendered as text or `--json`, both from the same model. `shown()` is what `slack read` may mark.
- `history.rs`: the unread part of a conversation (`conversations.history` from `last_read`, newest page, oldest first in the output, joins and leaves dropped). A `Message` carries its `reactions` from the same answer (name, count, the users Slack lists).
- `threads.rs`: `subscriptions.thread.getView`, paginated on `current_ts`, unread threads first; a read thread ends the walk, a shape change degrades to the summary line with a warning.
- `names.rs`: the id-to-name cache (`~/.cache/slack/names.json`), filled per id on a miss through `users.info` and `conversations.info`, a few lookups at a time, or in one walk (`users.conversations` for everything you are in, `users.list` for the directory) when the directory needs it whole.
- `directory.rs`: every conversation you are in (`client.counts` ids, named from the cache) and everyone in the workspace, with the word filter: behind `slack channels` and `slack people`. `members` is a channel's members for `slack who`, or None past a maximum, since which ones a page holds says nothing.
- `presence.rs`: what a person is up to now. `presences` reads `users.getPresence`, `profiles` reads `users.info` (the status, with Slack's own character for its emoji, `status_emoji_display_info`, since some names are not gemoji's, and the IANA timezone; read fresh, the name cache keeps a profile for good) and one `dnd.teamInfo` for the do-not-disturb windows. A person whose lookup fails is left without that part; the windows failing fails the whole read, since a missing window would read as notifications not paused, and so does a dead session. `Person` holds the three parts, each None until read; `status` drops an expired status, `paused_until` says whether now falls in the window, `local` is their clock, and `notes` the words beside a name (the status, their clock when its offset is not yours, the pause).
- `emoji.rs`: Slack's emoji names to characters through the `emojis` crate (gemoji's shortcodes), falling back to the Unicode name (`thinking_face`), Slack's hyphenated gendered names (`man-shrugging`) and a short alias list; skin tones follow `::skin-tone-N`. A workspace's custom emoji have no character and stay `:name:`. `other_names` gives the spellings to try when Slack refuses gemoji's (its alias, the Unicode name, the other shortcodes).
- `mrkdwn.rs`: Slack markup to terminal text. `pieces` reads a message into `Piece`s, text with its `Look` (bold, italic, strike, code; a name, a mention or a link) and markers with what was typed and what the styled text view shows instead (a `▎` bar for a quote, and after each line break of a `>>>` quote, which runs to the end of the message; a line break that sets a code block apart; else nothing). Mentions, channel links and URLs (a label that only repeats the URL dropped) are expanded first and stay whole, so markup goes around them and never into them; entities are decoded; code shields its contents; bold, italic and strike nest and need a word boundary. `render` paints the pieces for the text view, or with colours off writes them back as typed so a pipe loses nothing. Pure, with the parsing rules pinned by tests.
- `lastview.rs`: `~/.cache/slack/last-view.json`, the conversations the last view showed with the newest message each had, and the threads it numbered (`View::thread_refs`, reading order), which `slack thread 2` and `slack send 2` resolve. `slack show` adds its conversation and replaces the numbering: a number refers to the last output that printed one.
- `search.rs`: `search.messages` into `Hit`s (the channel, the message, its permalink).
- `replies.rs`: `conversations.replies`, root first then every reply, up to five pages; behind `slack thread`.
- `target.rs`: what a conversation argument means (`#channel`, `@person`, an id, a permalink, a thread number); the name walks (`conversations.list`, `users.list`) live in `names.rs` and fill the cache.
- `send.rs`, `react.rs` and `read.rs`: the commands that write to Slack (`chat.postMessage`, `reactions.add` and `reactions.remove`, `conversations.mark`); `react::set` takes `already_reacted` and `no_reaction` as done, since the message ends up as asked, tries `emoji::other_names` on `invalid_name` and returns the name Slack took, and `target::open_dm` opens a direct message (`conversations.open`) for an `@person` target. `read` only ever marks what `lastview` recorded, and never moves a cursor back (`read::already_read`).
- `mention.rs`: the `@handle` and `#channel` words of a message become `<@U…>` and `<#C…>` when the name cache knows them, from the cache alone (never a call; `@here` and `@channel` stay text), for `slack send`.
- `style.rs`: ANSI styling, off under `--no-color`, `NO_COLOR` or a non-terminal stdout. `par.rs`: a small ordered parallel map over a few threads for the lookups. `warn.rs`: warnings on the way to a result, on stderr.

## Files on disk

| File                                     | Written by                          | Read by                      | Deleting it                                    |
| ---------------------------------------- | ----------------------------------- | ---------------------------- | ---------------------------------------------- |
| `~/.config/slack/workspaces.json` (0600) | `login`, `logout`                   | every command (`store.rs`)   | logs you out                                   |
| `~/.config/slack/browser-profile/`       | `login` (Chrome)                    | `login --headless`           | the next login needs the window again          |
| `~/.cache/slack/names.json`              | every command that names something  | the same                     | safe, refilled on demand                       |
| `~/.cache/slack/last-view.json`          | the view, `show` and `read` (drops) | `read`, `thread N`, `send N` | safe; `read` and the numbers need a new output |

`$XDG_CONFIG_HOME` and `$XDG_CACHE_HOME` move the two directories; `$SLACK_BROWSER_PROFILE` moves the profile alone.

## Contracts between modules

- **The unread rule lives in one place.** `Conversation::counts_as_unread` in `counts.rs` decides what the view lists and what `--count` counts; the view and the count never re-decide.
- **One model for two outputs.** The text view and `--json` both come from `view::build`. A section or a field added to `View` shows up in both.
- **What `read` may mark is what was shown.** `View::shown` lists the conversations whose messages were printed; `lastview` records them as listed. `slack show` records its conversation too, but not as listed, so a bare `slack read` leaves it alone and `slack read <name>` takes it.
- **A number refers to the last output that printed one.** `View::thread_refs` numbers the threads in reading order; `show` replaces the numbering with its own; `last-view.json` keeps one numbering per workspace.
- **A missing name never fails a run.** `channel_label` and `user_label` fall back to the id, and `resolve_*`, which fetches per id on a miss, swallows a lookup that fails for one id. The directory walks (`walk_*`, when a directory is needed) do propagate their errors. The cache is written back at the end of the run.
- **Tokens stay below `api.rs`.** `Workspace` has no `Debug`, `api::Error` carries the method and Slack's error code but never the request, and nothing above the transport builds a header.
- **Only `https://*.slack.com` origins** are paired with the session cookie: `capture::is_slack_workspace_url` is the gate.

## Error direction

An error must never look like "nothing to do", above all for `--count` in a status bar. Any Slack or transport failure fails the run with a non-zero exit. Undocumented endpoints (`client.counts`, `subscriptions.thread.getView`, `localConfig_v2`) are read defensively: a missing or changed shape is an `Error::Shape`, or a warning where degrading is the better call (the thread view, the history of one conversation, the preferences), never silence. Warnings go through `warn.rs`, on stderr.

## Tests

`cargo test` never talks to Slack. The pure modules (the parsers, the view model, `mrkdwn`, the capture extractors, the target syntax) are tested in-module, with `tests/fixtures/` for the response shapes. The transport and the CDP session are checked by hand: the read-only commands against a live session.
