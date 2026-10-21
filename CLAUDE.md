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
  3. Bump `version` in `Cargo.toml`, `cargo build` so `Cargo.lock` follows, commit `chore: release X.Y.Z`, create the annotated tag (`git tag -a vX.Y.Z -m vX.Y.Z`), and hand Denis the one line: `! git push origin main vX.Y.Z` (`git push origin main` is denied by the global permission settings).
  4. Once he has pushed, watch the release with `gh run watch` and the tap with `gh run watch --repo denrou/homebrew-slack`, then report what shipped.

  The tag push runs `.github/workflows/release.yml`: it checks the tag against the crate version, lints and tests, builds the macOS arm64 and Linux x86_64 binaries, and publishes the GitHub Release with generated notes, the archives (`slack-vX.Y.Z-<target>.tar.gz`) and `checksums.txt`. Its last job asks the Homebrew tap (`denrou/homebrew-slack`, same shape as the gh-prs and ggit taps) to rewrite its formula from `checksums.txt`; that job needs the `GH_TOKEN` secret on the repository and is skipped without it, in which case run `gh workflow run update-formula.yml --repo denrou/homebrew-slack -f version=X.Y.Z` by hand and watch it with `gh run watch --repo denrou/homebrew-slack`. `.github/workflows/ci.yml` runs fmt, clippy and the tests on every push to `main`.

- **Live checks.** `slack login` needs a human in the Chrome window; once the session is saved, every read-only command (`whoami`, `--count`, `unreads`) can be run from here to check the output. **Never run `send`, `read` or anything in the screen (`slack` alone) that writes from here**, not even to see an error path: `read` acts on whatever `~/.cache/slack/last-view.json` holds, and on 2026-10-19 a run meant to show the "nothing to mark" message marked eight channels as read instead. Inspect the cache file; leave the writes to the user.

## Ideas

Denis asks for enhancements, fixes and features as they come to him in use, and wants a reaction, not an order taken. Give each one a moment of thought before building:

- When it is right as stated, build it, no question asked; one line on why it holds is enough.
- When it needs more thought, or another approach serves the same want better (a smaller change, another key, a different place in the flow, a cost it would carry later), say so first: the want as understood, the concern, the alternative with its trade-off, and a lean. Then build what he picks.

A concern about one of several asks never holds up the others. A reversal is fine: the picker opened on its list on request, then in its filter a day later, because use decides.

## Security rules

- Never print, log or serialise a token (`xoxc`, `xoxd`, the Cookie header), in output, errors or `--json`.
- Only `https://*.slack.com` origins, never `app.slack.com`, may be paired with the session cookie: `capture::is_slack_workspace_url` is the gate, and it is load-bearing. The socket `rtm.connect` names goes through its `wss://` twin, `is_slack_socket_url`.
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
- **Reactions work as a click in the app (2026-10-21).** `R` on a message puts the emoji on, or takes it back when it is already yours, and offers Slack's own frequently used list (`emoji_use` in `users.prefs.get`, already read with the counts) rather than a local one. Reacting is not reading: no cursor moves. Someone else's reaction comes through the socket.
- **Gmail's keys where Gmail has the action (2026-10-21).** Denis's hands know Gmail: `e` marks read, or closes an opened conversation without marking it (closing is not reading, and an unread one is listed in its section too), `x` selects for `e`, `r` replies, `c` composes, `o` opens any conversation, Enter goes deeper as `l` does. Slack's own `R` reacts, and `O` opens in the Slack app. The mark-everything key went with it: `x` then `e` covers the need without a key that can empty the list. `h` stays back rather than help, since it is vim's left and pairs with `l`.
- **Binary name `slack`**, kept because it is fast to type. Slack's platform CLI (`brew install slack-cli`) also installs a `slack` binary; it is not on this machine.
- **Public repo, MIT**, like gh-prs.

Kept from slackcli: the browser-session credentials and their capture (`auth/`). Deliberately not ported: app tokens, `parse-curl`, drafts, scheduling, canvases, the custom emoji list, files, saved items, search, user groups, the update checker, log files, Windows. If one becomes needed, it is a feature, not a port.

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

## Reference implementation

[slackcli](https://github.com/shaharia-lab/slackcli) (TypeScript, Bun) is installed on this machine as `slackcli` and logged in. Clone it into a scratch directory to compare how an endpoint is handled; `src/lib/browser-auth.ts`, `browser-launcher.ts`, `cdp-client.ts`, `unread.ts` and `unread-messages.ts` are the files these modules were ported from. Its `docs/user-guide/authentication.md` documents the two credential kinds.

## Testing

Pure modules are unit-tested in-module, the drawing through ratatui's `TestBackend`. Most response shapes are inline `json!` in the modules' tests; `tests/fixtures/` holds the larger ones, with ids and names made up: `client_counts.json` was written from slackcli's field list and checked field for field against a live response on 2026-10-19 (the live one also carries `updated`, `history_invalid`, `channel_badges`, `activity_v2`, `alerts`, `saved`, none of which is read).
