# CLAUDE.md

Guidance for Claude Code in this repository. `ARCHITECTURE.md` has the module map, the run of each command, the files on disk and the contracts between modules: read it before touching `src/`. This file is the workflow and the decisions.

## Commands

```bash
cargo build                                   # debug binary in target/debug/slack
cargo run -- --count                          # run a command
cargo test                                    # unit tests (pure modules, fixtures)
cargo clippy --all-targets -- -D warnings     # lint, warnings are errors
cargo fmt                                     # format
prettier --write "**/*.md"                    # Markdown
```

## Workflow

Denis's personal project (`github.com/denrou/slack`), outside the Centreon organisation. The Centreon conventions (feature branch, pull request, `/create-pr`, `/check-pr`, Jira references) do not apply, and `/create-pr` must not be used.

Denis leads the project as product owner; Claude is the developer and owns the code, its architecture and its quality.

- **Commit straight on the local `main`**, one conventional commit per change (`feat:`, `fix:`, `docs:`, `chore: release X.Y.Z`). Format, lint and test before committing; run Prettier on any Markdown touched. `ARCHITECTURE.md` and the decisions below change in the same commit as the code they describe. So does `skills/slack/SKILL.md`, the manual an agent reads before using the CLI: a command, an output or a rule it states changes in the same commit as the code. From a worktree (background sessions), rebase its branch on the local `main`, commit there, and hand Denis the one line to run in his own terminal: `cd ~/code/denrou/slack && git merge --ff-only <branch>`. Such a session must not merge, and its isolation refuses any git command aimed at the main checkout, a `!` line typed into the session included (it failed that way on 2026-10-20).
- **A push is a release (since 2026-10-20).** Nothing is pushed between releases: commits pile up on the local `main`, and Denis's one push sends `main` and the new tag together. Never ask him to push anything else, never push a branch or open a pull request to get around it, and never bump the version or create a tag on your own. He tries the batch from the local checkout (`cargo install --path ~/code/denrou/slack` puts it in `~/.cargo/bin`) and says when it is ready.
- **A release, when Denis asks for it**:
  1. Review the batch: `/code-review` over everything since the last tag (`git log v<previous>..main`), plus the pr-review-toolkit agents when the batch is large. Fix what matters in their own commits; what is deferred on purpose goes to "Not planned yet" below with its reason.
  2. `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`, `cargo build --release`: CI only sees the code at release time, so it has to pass here first.
  3. Bump `version` in `Cargo.toml`, `cargo build` so `Cargo.lock` follows, commit `chore: release X.Y.Z`, create the annotated tag with the release notes as its message (`git tag -a vX.Y.Z --cleanup=whitespace -F <file>`), and hand Denis the one line: `! git push origin main vX.Y.Z` (`git push origin main` is denied by the global permission settings).

     The message is `vX.Y.Z`, a blank line, then the notes the GitHub Release shows (since 2026-10-21): a few Markdown bullets on what changed for whoever uses the tool, new commands and keys, a behaviour that moved, a fix that was felt, not the commit list, which the compare link GitHub adds below them already gives. Show them to Denis with the push line. `--cleanup=whitespace` keeps a line that starts with `#`, which git would otherwise strip as a comment.

  4. Once he has pushed, watch the release with `gh run watch` and the tap with `gh run watch --repo denrou/homebrew-slack`, then report what shipped.

  The tag push runs `.github/workflows/release.yml`: it checks the tag against the crate version and reads its notes (a tag without them fails there, before the builds), lints and tests, builds the macOS arm64 and Linux x86_64 binaries, and publishes the GitHub Release with the tag's notes and the compare link, the archives (`slack-vX.Y.Z-<target>.tar.gz`) and `checksums.txt`. Its last job asks the Homebrew tap (`denrou/homebrew-slack`, same shape as the gh-prs and ggit taps) to rewrite its formula from `checksums.txt`; that job needs the `GH_TOKEN` secret on the repository and is skipped without it, in which case run `gh workflow run update-formula.yml --repo denrou/homebrew-slack -f version=X.Y.Z` by hand and watch it with `gh run watch --repo denrou/homebrew-slack`. `.github/workflows/ci.yml` runs fmt, clippy and the tests on every push to `main`.

- **Live checks.** `slack login` needs a human in the Chrome window; once the session is saved, every read-only command (`whoami`, `--count`, `unreads`) can be run from here to check the output. **Never run `send`, `read` or anything in the screen (`slack` alone) that writes from here**, not even to see an error path: `read` acts on whatever `~/.cache/slack/last-view.json` holds, and on 2026-10-19 a run meant to show the "nothing to mark" message marked eight channels as read instead. Inspect the cache file; leave the writes to the user.

## Ideas

Denis asks for enhancements, fixes and features as they come to him in use, and wants a reaction, not an order taken. Give each one a moment of thought before building:

- When it is right as stated, build it, no question asked; one line on why it holds is enough.
- When it needs more thought, or another approach serves the same want better (a smaller change, another key, a different place in the flow, a cost it would carry later), say so first: the want as understood, the concern, the alternative with its trade-off, and a lean. Then build what he picks.

A concern about one of several asks never holds up the others. A reversal is fine: the picker opened on its list on request, then in its filter a day later, because use decides.

## Security rules

- Never print, log or serialise a token (`xoxc`, `xoxd`, the Cookie header), in output, errors or `--json`.
- Only `https://*.slack.com` origins, never `app.slack.com`, may be paired with the session cookie: `capture::is_slack_workspace_url` is the gate, and it is load-bearing. The socket `rtm.connect` names goes through its `wss://` twin, `is_slack_socket_url`, and no redirect is followed from it, since the cookie would go along. The same holds for a posted file (`Client::download`, on `files.slack.com` since 2026-10-22).
- A posted file is copied to disk owner-only (`~/.cache/slack/files/`, 0700 and 0600), marked as a download (macOS's quarantine flag) before it is opened, and dropped after a week, the next time the screen opens, and by `slack logout`: it may come from a private channel, or be something that runs.
- A profile path from the environment is user input: a directory without our sentinel is never reused and never deleted.

## Decisions (2026-10-19)

Written down because none of them can be read from the code.

- **Blocking I/O, no async runtime.** A handful of sequential HTTP calls, one WebSocket for the capture and one on a thread for the screen's socket, a `std::thread::scope` for the lookups. Switching later would cost `api.rs` and `auth/cdp.rs`, not the pure modules.
- **Credentials in `workspaces.json` (mode 0600), not the Keychain.** The footer polls `slack --count --json` from a launchd job (`~/.local/bin/update-dev-status`, its SLACK and @ME cells read `unread` and `me`), and the Keychain prompts whenever the binary changes, which is every rebuild. The browser profile next to it is a plaintext credential store anyway. A keychain backend can come later as an opt-in.
- **Mark-as-read is explicit** (`slack read`, `e` in the screen), never a side effect of reading, so the footer count keeps agreeing with the Slack app. The one exception is a reply to a conversation from the screen, which reads it up to the reply, as the app does. A mark never moves a cursor back: what the app read since is left alone.
- **Slack's own mute list is honoured; there is no local one.**
- **Every unread conversation is shown, muted ones excepted (2026-10-20).** The first rule hid public channels unless private or set to "All new messages" in Slack, and twice hid a channel Denis wanted to see (`#log-team-dev`, then `#proj-prediction-service`). He mutes what he does not want, so the rest is wanted: the view and the count follow Slack's sidebar, bold for anything unread, muted ones greyed out. The `-a` tier (`slack -a`, `--count --all`, `ui -a`, `read --all`, the `a` key) went with it.
- **The screen reads with context, the text view without (2026-10-20).** `slack` shows the recent history of each unread conversation, and each unread thread whole (2026-10-21, one `conversations.replies` per thread on each reload), with the read part dimmed and a `── new ──` line, because a message is understood with what came before it. The recent history is today's messages and the unread ones, never fewer than the last 50 nor more than 200 (2026-10-21): the last 50 alone cut the start of a busy day's new part, and today alone would leave a quiet channel's one message without context. `slack unreads` keeps printing the new part alone: a script or Claude Code wants what is new and nothing else.
- **`slack` alone is the interactive view (2026-10-20).** Denis lives in the TUI; the text view is `slack unreads`, for scripts and for Claude Code reading messages. Without a terminal on both ends, `slack` prints the text instead of drawing, so a pipe never hangs on a screen.
- **The screen refreshes itself (2026-10-21).** A full reload is one history call per unread conversation, so it never runs on a timer: `client.counts` is read on a detached thread so the screen never stalls, and the screen reloads only when a conversation's newest message, read cursor or mentions moved. A refresh, Ctrl-R included, keeps your place instead of sending you back to the list, and none runs while the reply box, the picker or a prompt is open: the reply goes to whatever is selected when Enter is pressed. A selection (`x`) does span refreshes, so `e` marks each conversation up to the newest message it had when selected, never one that came in since.
- **The screen listens to Slack's socket, the poll is its fallback (2026-10-21).** Polling every 15 seconds was the delay of a message, and missed what the counts do not show: a new reply in a thread already unread, someone else's reaction. Slack's real-time socket (`rtm.connect`) accepts the browser session (probed live on 2026-10-21: messages, read cursors and a `pong` to our `ping` came through). An event asks for the counts, at most every 5 seconds, or for a reload when it touches what the screen shows; a reaction and a reply's count are applied without a call. Who is typing shows too, on the pane's bottom border; yours is not sent, since it is a write others see and the socket was not probed for it. The endpoint answers as `LEGACY_BOT`, so Slack may close it one day: the poll stays, every 15 seconds while the socket is down and every 2 minutes beside it, in case it silently stops sending one kind of event. The footer's `--count` keeps its launchd job: the job runs for its other cells anyway, and a socket held around the clock would be a daemon to keep alive and might show Denis active at night.
- **The screen lands on the recent conversations (2026-10-21).** With nothing unread it opened on "All caught up." and reaching a channel took the picker. Under the unread sections, `Recent channels` and `Recent people` list the conversations visited last, 8 of each: Slack's own `recents` pref (read with the counts, no extra call), so it follows the app rather than this tool alone, and only those, since the whole directory would drown the list. One list with sections rather than three panes, so `j`/`k` and Tab cover it without a focus key or fixed heights. A recent conversation's history is read when the cursor rests on it (300 ms) and kept until a new message comes in there: reading all of them would be 16 history calls per refresh.
- **The screen notifies, the status bar is its fallback (2026-10-22).** Denis keeps the Slack app for now and means to drop it, so a message aimed at you is notified from the screen: the socket hears it as it is posted, and the terminal posts the notification (`OSC 777`, WezTerm and Ghostty) rather than SwiftBar, whose `notify` must name one of its plugins and would tie the CLI to a dotfile; clicking a terminal's notification brings you back to the terminal. The rule is `@ME`'s, so the notification and the badge agree, and nothing is posted while the terminal has the focus, since the screen shows the message. With the screen closed or its socket down, the SwiftBar plugin of the dotfiles notifies from the 5-minute `--count --json`, as before, and keeps quiet while that says `notifying` (a heartbeat file the screen refreshes, fresh for 90 seconds). Closing the screen to focus leaves that fallback on; macOS's Focus modes are the way to silence both. Left out on purpose: two screens open notify twice, a mention of a user group is not notified (its members are not known here), the click does not open the conversation, and a socket that silently stops sending messages silences both until it drops.
- **A draft is corrected on request, by a command of the user's (2026-10-22).** Denis writes French on a QWERTY keyboard and leaves the accents out. Ctrl-G in the reply box corrects the draft in place, Enter sends what was read, Ctrl-G again restores the typed text; a hook on every send was turned down, since it would send words not seen and cost every message, "ok merci" and English included, the model's few seconds. The model is `fix_command` in `~/.config/slack/config.toml`, the tool's first config file: no API key or HTTP client here, any runner fits, and a file reaches a screen opened from a terminal's key binding where an environment variable would not. The instructions live in `fix.rs`, so the command is the runner alone. For `claude -p` the flags matter: without `--tools ''` and its own `--system-prompt` it is Claude Code, which answered a draft as a coding agent from this repository (seen on 2026-10-22), hence also the temporary directory as its working directory. `--bare` would be cleaner but takes an API key only, not the subscription. `slack send` has no correction: what it sends is written by Claude Code, whose French needs none.
- **An image is looked at in the screen, any other file in its own application (2026-10-22).** `u` on a message opens its files beside its links: a PNG, JPEG or WebP is drawn over the messages pane through `ratatui-image` (the terminal's kitty, iTerm2 or sixel graphics, half blocks without), from Slack's preview of 1024 pixels at most, one download on demand; `u` again hands the file itself to the system viewer. A GIF goes straight to the viewer, since only its first frame would show here, and so does anything else (a PDF, a video). Images inline in the history were turned down: the pane scrolls by wrapped rows, which clip a picture badly, and every reload would download each conversation's previews. The copies are cached a week, since the viewer needs a file on disk and a file looked at again should not be downloaded again. `ratatui-image` 9 is the last release on ratatui 0.29, with its `image` dependency cut to PNG, JPEG and WebP: about 1 MB of binary.
- **Reactions work as a click in the app (2026-10-21).** `R` on a message puts the emoji on, or takes it back when it is already yours, and offers Slack's own frequently used list (`emoji_use` in `users.prefs.get`, already read with the counts) rather than a local one. Reacting is not reading: no cursor moves. Someone else's reaction comes through the socket.
- **Gmail's keys where Gmail has the action (2026-10-21).** Denis's hands know Gmail: `e` marks read, or closes an opened conversation without marking it (closing is not reading, and an unread one is listed in its section too), `x` selects for `e`, `r` replies, `c` composes, `o` opens any conversation, Enter goes deeper as `l` does. Slack's own `R` reacts, and `O` opens in the Slack app. The mark-everything key went with it: `x` then `e` covers the need without a key that can empty the list. `h` stays back rather than help, since it is vim's left and pairs with `l`.
- **Search is Slack's own (2026-10-21).** `slack search` and the screen's `/` send the query to `search.messages` as typed, so Slack's operators work and nothing is indexed here; the newest matches first, not Slack's relevance, since what is looked for from a terminal is usually recent. A hit opens in its conversation read around it (25 messages on each side, `history::around`), not in the recent history, which an older hit would miss; `/` because it is Gmail's search key. One page: 20 by default and 100 at most on the command line, 50 in the screen, where Slack's operators narrow rather than a second page.
- **Binary name `slack`**, kept because it is fast to type. Slack's platform CLI (`brew install slack-cli`) also installs a `slack` binary; it is not on this machine.
- **Public repo, MIT**, like gh-prs.

Kept from slackcli: the browser-session credentials and their capture (`auth/`). Deliberately not ported: app tokens, `parse-curl`, drafts, scheduling, canvases, the custom emoji list, file uploads and listings, saved items, user groups, the update checker, log files, Windows. If one becomes needed, it is a feature, not a port.

## Not planned yet

`--workspace`, a `watch` mode, loading older history from the TUI.

Deferred on purpose by the review of 2026-10-20 (`7a4bfc8..065ccde`), to pick up when the code around them moves: keeping a conversation that fails to load in the list with its error instead of dropping it, an `ItemKind` enum in place of `Item`'s parallel fields, newtypes for the workspace origin and the message `ts`, a warning when the name lookups fail wholesale.

Deferred on purpose by the review of 2026-10-21 (`v0.9.0..main`):

- The `R` list follows Slack's `emoji_use`, so an emoji just used from here moves up only once Slack counts it (whether `reactions.add` does is unverified). A local bump would be undone by the next reload unless the CLI wrote the pref back with `users.prefs.set`, a write for a cosmetic gain.
- Slack lists only the first users of a popular reaction, so yours may be missing: `R` then adds, `already_reacted` is taken as done and the count shows one too many until a reload. Rare, and the fix needs `react::set` to tell a change from a no-op.
- After a mark or a reply, the next poll still differs from the list's counts and reloads to confirm what the screen already shows. Correct, one history call per conversation; updating the pulse locally would need the new cursors.
- The picker's Tab is silent on a channel and when it unmarks someone, and BackTab no longer moves up since Tab stopped moving down.
- A poll thread that panics leaves the screen without auto-refresh and without a word; a panic there is a bug, not a condition.
- A pref that is present but not JSON is read as absent without a warning (from before this batch).
- `slack react --remove` says "Took back" when there was nothing to take back.

Deferred on purpose by the review of 2026-10-21 (`v0.11.0..main`): a conversation `e` marked but kept, because a message came in after `x` selected it, shows its old unread count and `── new ──` line until the next refresh, up to about 15 seconds. Showing it read up to the mark would need a third way to word the detail line, beside the listed and the opened ones.

Deferred on purpose by the review of 2026-10-21 (`v0.10.0..main`): with people marked, Enter on a typed name whose highlighted match is a channel or a group does nothing and says nothing (the picker has no status line of its own). Rare, since a group is people; a typo that matches nothing goes to the list instead.

Deferred on purpose by the review of 2026-10-21 (`v0.12.0..main`):

- The whole threads are read after the histories and the thread view, not beside them, and a long thread is read again on every reload even when nothing in it moved. Overlapping them needs the thread view first, and a cache keyed by `latest_reply` would be a store of its own; worth it if a reload feels slow.
- Each unread conversation reads one page of 200 and usually keeps 50 or today's. Asking from midnight and falling back to the last 50 would be two calls where one is now; Denis chose the one bigger call.

Deferred on purpose by the review of 2026-10-21 (`v0.14.0..main`):

- A change that leaves a message's text alone (a link's preview, a bot updating its attachments) is dropped from the socket and shows at the next reload. Applying it in place would mean parsing the socket's message as the history does, for what is mostly a preview.
- Your reply from another client bumps no reply count under a root shown in a conversation's history: from here the screen has already read it again, and the socket cannot tell the two apart.
- `slack search` indents the permalink by copying `render_messages`' gutter (`commands.rs`); it drifts if that gutter changes, which a glance at the output shows.
- Untested, since each needs a pure function pulled out of `App` or `history::around` first: the owed reload and the re-nudge of a dropped poll, `Visited::of`, `read_in_place`, and the merge of the two pages around a search hit.

Deferred on purpose by the review of 2026-10-22 (`v0.15.0..main`):

- A DM that comes in while the reply box, the picker or the search is open shows nowhere until it closes: the terminal has the focus, so nothing is notified, and no reload runs outside the list. Notifying then would post while you type in the terminal; a mark on the box's border may be the smaller answer, to weigh once it is felt.
- The notifications' mute set is built from the conversations `client.counts` lists, so a closed muted DM is notified as direct, and a startup without the preferences notifies every muted conversation's `@here` until the next good poll. A muted DM is rare, and startup has no earlier list to fall back on.
- `notifying` is claimed from `TERM_PROGRAM` alone: Zellij or screen inside WezTerm, or a terminal macOS does not let notify, silences the screen and the fallback both. The first DM shows it; a `notify` key in `config.toml` is the way out if it happens.
- Messages heard nowhere while the socket reconnects are shown by the reload but not notified, and after a laptop wakes the heartbeat is renewed for up to a minute on a dead socket. A note counting what `@ME` gained across a reconnect would cover both.
- A heartbeat that cannot be written is tried again on every pass and never reported; the fallback notifies meanwhile, twice rather than never.
- The channel lookup a notification needs on a miss runs on the screen's thread while it is unfocused: once per new conversation, then cached.
- `fix::run` runs on the screen's thread, as every action does (blocking I/O, above), so a cold `claude -p` freezes the screen for its 10 to 20 seconds; running it on a thread is ready to do, since `State::fixed` already drops a stale answer. Its deadline does not cover the read of the answer once `sh` has exited, which a helper left holding stdout would hang, and a read error there passes a truncated correction, seen before Enter. Neither seen with `claude -p`.
- The markup is parsed again on every redraw, four times a second, for every message in the pane. Worth a cache by message `ts` if the screen shows up in Activity Monitor.
- Closing one of two screens removes the shared heartbeat, so the fallback notifies beside the other for up to 30 seconds; two screens already notify twice.
- Untested, since each needs a pure function pulled out of `App` first: the heartbeat's four transitions in `keep_beat`, and the note `App::notify` builds (a group DM names its sender, an IM does not).

Deferred on purpose by the review of 2026-10-22 (`v0.16.1..main`):

- The reply box's cursor steps by Unicode scalar, not by grapheme: in a decomposed `é` (text copied from a macOS file name) or a 👍🏽, Left can stop inside the character and Delete takes half of it. Typed and corrected French is composed, so it is not felt; the fix is the `unicode-segmentation` crate, and `Move::Left` and `Cut::Char` should then share one step. Up and Down keep the column in characters, not in display width, for the same reason.
- Ctrl-G and its undo put the cursor at the end, so the place in a long draft is lost. The correction is asked before sending, with the cursor at the end in practice; the undo could keep the old cursor beside `Draft::typed` if it is ever felt.
- `Draft::text` and `Draft::cursor` are both public, so nothing but care keeps the cursor on a boundary of the text; `ui.rs`'s tests set the text by hand. Private fields behind a constructor if a third writer appears.
- The reply box wraps its text three times a frame (its height, the caret's row, the drawing). Drafts are a few lines; one count shared by the three if the screen shows up in Activity Monitor, with the markup cache above.

Deferred on purpose by the review of 2026-10-22 (`v0.17.0..main`):

- A file is downloaded on the screen's thread under one 120-second deadline: one that takes longer (a long screen recording) cannot be opened from here, and the screen is frozen meanwhile. The fix is the one `fix::run` is ready for, a thread, with a deadline per read rather than for the whole file; previews are small and videos rare, and `O` opens the message in Slack meanwhile.
- The terminal's graphics are asked on every start, up to 2 seconds on a terminal that does not answer, for a feature used now and then. A shorter timeout would be worse: the crate's reader is not cancelled and would keep taking keys. Asking on the first image instead would race crossterm's reader. WezTerm answers at once.
- A key typed while the startup queries run is lost, and so are bytes the graphics query leaves in std's buffered stdin.
- The week a copy is kept is counted at the next start of the screen, not by `--count`, which the launchd job runs every 5 minutes and which must stay two calls and nothing else.

## Reference implementation

[slackcli](https://github.com/shaharia-lab/slackcli) (TypeScript, Bun) is installed on this machine as `slackcli` and logged in. Clone it into a scratch directory to compare how an endpoint is handled; `src/lib/browser-auth.ts`, `browser-launcher.ts`, `cdp-client.ts`, `unread.ts` and `unread-messages.ts` are the files these modules were ported from. Its `docs/user-guide/authentication.md` documents the two credential kinds.

## Testing

Pure modules are unit-tested in-module, the drawing through ratatui's `TestBackend`. Most response shapes are inline `json!` in the modules' tests; `tests/fixtures/` holds the larger ones, with ids and names made up: `client_counts.json` was written from slackcli's field list and checked field for field against a live response on 2026-10-19 (the live one also carries `updated`, `history_invalid`, `channel_badges`, `activity_v2`, `alerts`, `saved`, none of which is read).
