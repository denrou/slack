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

- **Live checks.** `slack login` needs a human in the Chrome window; once the session is saved, every read-only command (`whoami`, `--count`, `unreads`) can be run from here to check the output. **Never run `send`, `react` or `read` from here**, not even to see an error path: `read` acts on whatever `~/.cache/slack/last-view.json` holds, and on 2026-10-19 a run meant to show the "nothing to mark" message marked eight channels as read instead. Inspect the cache file; leave the writes to the user. `slack watch` sends nothing and can be run from here, with `XDG_STATE_HOME` in the scratchpad so Denis's own log stays his.

## Ideas

Denis asks for enhancements, fixes and features as they come to him in use, and wants a reaction, not an order taken. Give each one a moment of thought before building:

- When it is right as stated, build it, no question asked; one line on why it holds is enough.
- When it needs more thought, or another approach serves the same want better (a smaller change, another key, a different place in the flow, a cost it would carry later), say so first: the want as understood, the concern, the alternative with its trade-off, and a lean. Then build what he picks.

A concern about one of several asks never holds up the others. A reversal is fine, because use decides: the screen's picker opened on its list on request, then in its filter a day later, and the screen itself went once the skill turned out to be the use.

## Security rules

- Never print, log or serialise a token (`xoxc`, `xoxd`, the Cookie header), in output, errors or `--json`.
- Only `https://*.slack.com` origins, never `app.slack.com`, may be paired with the session cookie: `capture::is_slack_workspace_url` is the gate, and it is load-bearing.
- A profile path from the environment is user input: a directory without our sentinel is never reused and never deleted.

## Decisions (2026-10-19)

Written down because none of them can be read from the code.

- **Blocking I/O, no async runtime.** A handful of sequential HTTP calls, one WebSocket for the capture, a `std::thread::scope` for the lookups. Switching later would cost `api.rs` and `auth/cdp.rs`, not the pure modules.
- **Credentials in `workspaces.json` (mode 0600), not the Keychain.** The footer polls `slack --count --json` from a launchd job (`~/.local/bin/update-dev-status`, its SLACK and @ME cells read `unread` and `me`), and the Keychain prompts whenever the binary changes, which is every rebuild. The browser profile next to it is a plaintext credential store anyway. A keychain backend can come later as an opt-in.
- **Mark-as-read is explicit** (`slack read`), never a side effect of reading, so the footer count keeps agreeing with the Slack app. A mark never moves a cursor back: what the app read since is left alone.
- **Slack's own mute list is honoured; there is no local one.**
- **Every unread conversation is shown, muted ones excepted (2026-10-20).** The first rule hid public channels unless private or set to "All new messages" in Slack, and twice hid a channel Denis wanted to see (`#log-team-dev`, then `#proj-prediction-service`). He mutes what he does not want, so the rest is wanted: the view and the count follow Slack's sidebar. The `-a` tier (`slack -a`, `--count --all`, `ui -a`, `read --all`, the `a` key) went with it.
- **Search is Slack's own (2026-10-21).** `slack search` sends the query to `search.messages` as typed, so Slack's operators work and nothing is indexed here; the newest matches first, not Slack's relevance, since what is looked for from a terminal is usually recent. One page, 20 by default and 100 at most: Slack's operators narrow rather than a second page.
- **Who is around, for the people in view (2026-10-23).** What Denis wants is whether he can write to someone now, which status and the other person's clock answer better than the dot. So `slack who` takes named people, a channel's members (50 at most) or the recent DMs, never the whole workspace: Slack has no bulk presence call any more (one `users.getPresence` each, hundreds at Centreon) and keeps no history, so "who was online at 10:00" cannot be answered at all. Left out on purpose: group DMs, and someone else's snooze, which `dnd.teamInfo` does not give.
- **No interactive screen (2026-10-28).** `slack` alone opened a ratatui screen since 2026-10-19 (list and messages panes, replies, reactions, the picker, Slack's socket, notifications, images, the Ctrl-G correction): about 11,000 lines with its socket, notifier and file viewer. In use Denis rarely opened it: the correction he valued moved to a Hammerspoon key that works in the Slack app itself, and the use that stuck is Claude Code through the skill, which only needs the commands. So the screen went, and `slack` alone prints `slack unreads`. A terminal client is slk's job if one is ever wanted; bringing the screen back would be `git show v0.18.1:src/tui/`, not a port.
- **Watch, a dry run first (2026-10-28).** Denis works six hours behind most of his colleagues, so a question asked during his night waits for his morning. `slack watch` hands each DM and each mention of him (`<@him>`, never `@here`, `@channel` or a group) to a command of his, which reads the exchange on stdin and prints a reply. The prompts (a judge, a responder, a leak check) live in that command, outside this repository. Three things are deliberate. The command never gets Slack, only text, and the CLI alone decides where a reply would go. The marker `🤖 Réponse automatique :` is added by the CLI, so no prompt can drop it. And nothing is sent yet: the watch runs all day and logs what it would send next to what Denis wrote afterwards, so the prompts are tuned on real exchanges before security and IT look at it. Sending, when it comes, is a decision of its own, with a window (his nights, his days off, which France does not share) and a limit per conversation so two auto-responders cannot answer each other. Slack's socket came back for it (`rtm.rs`, from v0.18.1): it hears a message whether or not it was read since, and a mention in any thread, which polling `client.counts` cannot.
- **Binary name `slack`**, kept because it is fast to type. Slack's platform CLI (`brew install slack-cli`) also installs a `slack` binary; it is not on this machine.
- **Public repo, MIT**, like gh-prs.

Kept from slackcli: the browser-session credentials and their capture (`auth/`). Deliberately not ported: app tokens, `parse-curl`, drafts, scheduling, canvases, the custom emoji list, file uploads and listings, saved items, user groups, the update checker, log files, Windows. If one becomes needed, it is a feature, not a port.

## Not planned yet

`--workspace`. Sending from the watch (see its decision). A status set from the calendar, since Slack does not change it by itself.

Deferred on purpose by the review of 2026-10-20 (`7a4bfc8..065ccde`), to pick up when the code around them moves: keeping a conversation that fails to load in the view with its error instead of dropping it, newtypes for the workspace origin and the message `ts`, a warning when the name lookups fail wholesale.

Deferred on purpose by the reviews of 2026-10-21 (`v0.9.0..main`, `v0.14.0..main`):

- A pref that is present but not JSON is read as absent without a warning (from before that batch).
- `slack react --remove` says "Took back" when there was nothing to take back.
- `slack search` indents the permalink by copying `render_messages`' gutter (`commands.rs`); it drifts if that gutter changes, which a glance at the output shows.

Deferred on purpose by the review of 2026-10-23 (`v0.18.0..main`):

- The channels a message links are found by a scan of their own (`view::mentions`), beside `mrkdwn`'s token pass that draws them, so odd markup could be looked up and drawn differently. The scan is the one `mentioned_users` always was; one parser for both if a raw id ever shows.
- The lookup rounds stay one after another: on a cold cache, `view::build` waits on the conversations, the threads' channels, the people, then the linked channels. A warm cache makes each a no-op, so this is felt on a first run only.
- Untested end to end, since resolution needs a `Client`: a linked channel shown by name. `mentioned_channels`, the `refused` set and the drawing with a known name are each tested.

Deferred on purpose with the watch (2026-10-28), to pick up once its log has been read:

- A socket drop loses what was posted meanwhile; the log says so (`gap`) but nothing is read back from `client.counts`.
- The command runs one event at a time: a slow one delays the next.
- Whether holding the socket shows Denis as active at night is not checked yet; the first night's log (`me.presence`) tells.

## Reference implementation

[slackcli](https://github.com/shaharia-lab/slackcli) (TypeScript, Bun) is installed on this machine as `slackcli` and logged in. Clone it into a scratch directory to compare how an endpoint is handled; `src/lib/browser-auth.ts`, `browser-launcher.ts`, `cdp-client.ts`, `unread.ts` and `unread-messages.ts` are the files these modules were ported from. Its `docs/user-guide/authentication.md` documents the two credential kinds.

## Testing

Pure modules are unit-tested in-module. Most response shapes are inline `json!` in the modules' tests; `tests/fixtures/` holds the larger ones, with ids and names made up: `client_counts.json` was written from slackcli's field list and checked field for field against a live response on 2026-10-19 (the live one also carries `updated`, `history_invalid`, `channel_badges`, `activity_v2`, `alerts`, `saved`, none of which is read).
