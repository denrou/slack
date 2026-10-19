# CLAUDE.md

Guidance for Claude Code in this repository.

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

- **Commit straight on `main`**, conventional-commit messages (`feat:`, `fix:`, `docs:`, `chore: release X.Y.Z`). Format, lint and test before committing; run Prettier on any Markdown touched.
- **Pushing is the user's move.** `git push origin main` is denied by the global permission settings, so hand them the exact line to run:

  ```
  ! git push origin main
  ```

  Do not route around the denial by pushing a branch and opening a pull request.

- **Offer a release after every landed change** once the release workflow exists (PLAN.md, step 8): bump `version` in `Cargo.toml`, `cargo build` to refresh `Cargo.lock`, commit `chore: release X.Y.Z`, have the user push, then `gh release create vX.Y.Z --target main`.
- **PLAN.md is the agreed plan.** Keep its step list in step with reality; delete the file once the steps are done and this file plus the README carry everything.
- **Live checks.** `slack login` needs a human in the Chrome window; once the session is saved, every read-only command (`whoami`, `--count`, the view) can be run from here to check the output. **Never run `send` or `read` from here**, not even to see an error path: `read` acts on whatever `~/.cache/slack/last-view.json` holds, and on 2026-10-19 a run meant to show the "nothing to mark" message marked eight channels as read instead. Inspect the cache file; leave the writes to the user.

## Architecture

One crate, binary `slack`, blocking I/O throughout (`ureq`, `tungstenite`); no async runtime, deliberately. Modules by what they know:

- `main.rs`: clap definitions and dispatch. No subcommand is the view; `--count` is the status-bar path.
- `commands.rs`: one function per command, the only place that prints.
- `api.rs`: the one transport. A form POST to `<workspace url>/api/<method>` with `token=<xoxc>` in the body and `Cookie: d=<xoxd>`, the way the web client does it. Parses the `ok` envelope, maps `invalid_auth` and friends to `Error::Auth` ("run `slack login`"), retries HTTP 429 twice honouring `Retry-After` (capped at 30 s). Commands never see headers or tokens.
- `store.rs`: `~/.config/slack/workspaces.json`, workspaces keyed by team id with their credentials, written atomically with mode `0600`. `Workspace` has no `Debug` so a token can never land in a log.
- `paths.rs`: XDG lookup (`~/.config/slack`, like gh-prs and slackcli; not `~/Library/Application Support`) and owner-only file writes.
- `auth/chrome.rs`: find Chrome, launch it with `--remote-debugging-port=0` on our own profile, read `DevToolsActivePort`, find the page target, shut down (SIGTERM, SIGKILL, then `pkill -f user-data-dir=<profile>` for the helpers Chrome leaves behind). The profile is owned through a sentinel file; `logout` deletes only an owned profile.
- `auth/cdp.rs`: a hand-rolled CDP session over one WebSocket: numbered commands, buffered events, read timeouts on the socket so a poll never blocks.
- `auth/capture.rs`: the capture policy and its pure extractors (`xoxc` from a request body in the three encodings Slack uses, the `d` cookie, the workspace list in `localConfig_v2`, the origin gate). Ported from slackcli's `browser-auth.ts`; the tests pin the parsing rules.
- `counts.rs`: `client.counts` into `Conversation`s and the `Threads` block, with the attention rule (DMs with unreads, channels with a mention, plus one for threads).
- `view.rs`: the model (`View`: direct messages, mentions, channels, threads) built from the counts, then rendered as text or `--json`. The model is kept apart from the rendering so a TUI can sit on the same data. `shown()` is what `slack read` may mark.
- `history.rs`: the unread part of a conversation (`conversations.history` from `last_read`, newest page, oldest first in the output, joins and leaves dropped).
- `threads.rs`: `subscriptions.thread.getView`, paginated on `current_ts`, unread threads first; a read thread ends the walk, a shape change degrades to the summary line with a warning.
- `names.rs`: the id-to-name cache (`~/.cache/slack/names.json`), filled per id on a miss through `users.info` and `conversations.info`, a few lookups at a time.
- `mrkdwn.rs`: Slack markup to terminal text (mentions, channel links, URLs with a label that only repeats the URL dropped, entities, bold/italic/strike/code when colors are on). Pure, with the parsing rules pinned by tests.
- `lastview.rs`: `~/.cache/slack/last-view.json`, the conversations the last view showed with the newest message each had.
- `target.rs`: what a conversation argument means (`#channel`, `@person`, an id, a permalink); the name walks (`conversations.list`, `users.list`) live in `names.rs` and fill the cache.
- `send.rs` and `read.rs`: the two commands that write to Slack (`chat.postMessage`, `conversations.mark`). `read` only ever marks what `lastview` recorded.
- `style.rs`: ANSI styling, off under `--no-color`, `NO_COLOR` or a non-terminal stdout. `par.rs`: a small ordered parallel map over a few threads for the lookups.

### Error direction

An error must never look like "nothing to do", above all for `--count` in a status bar. Any Slack or transport failure fails the run with a non-zero exit. Undocumented endpoints (`client.counts`, later `subscriptions.thread.getView`, `localConfig_v2`) are read defensively: a missing or changed shape is an `Error::Shape`, or a warning on stderr where degrading is the better call (the thread view), never silence.

### Security rules

- Never print, log or serialise a token (`xoxc`, `xoxd`, the Cookie header), in output, errors or `--json`.
- Only `https://*.slack.com` origins, never `app.slack.com`, may be paired with the session cookie: `capture::is_slack_workspace_url` is the gate, and it is load-bearing.
- A profile path from the environment is user input: a directory without our sentinel is never reused and never deleted.

## Reference implementation

[slackcli](https://github.com/shaharia-lab/slackcli) (TypeScript, Bun) is installed on this machine as `slackcli` and logged in. Clone it into a scratch directory to compare how an endpoint is handled; `src/lib/browser-auth.ts`, `browser-launcher.ts`, `cdp-client.ts`, `unread.ts` and `unread-messages.ts` are the files these modules were ported from. Its `docs/user-guide/authentication.md` documents the two credential kinds.

## Testing

Pure modules are unit-tested in-module. `tests/fixtures/` holds response shapes with ids and names made up: `client_counts.json` was written from slackcli's field list and checked field for field against a live response on 2026-10-19 (the live one also carries `updated`, `history_invalid`, `channel_badges`, `activity_v2`, `alerts`, `saved`, none of which is read).
