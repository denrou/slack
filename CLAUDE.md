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

- **Commit straight on `main`**, conventional-commit messages (`feat:`, `fix:`, `docs:`, `chore: release X.Y.Z`). Format, lint and test before committing; run Prettier on any Markdown touched.
- **Pushing is the user's move.** `git push origin main` is denied by the global permission settings, so hand them the exact line to run:

  ```
  ! git push origin main
  ```

  Do not route around the denial by pushing a branch and opening a pull request.

- **Releases are on request and bundle several changes** (since 2026-10-20). Land each change as its own commit on `main`; never bump the version or create a tag on your own. Denis tries the tip of `main` first (`cargo install --path .` puts it in `~/.cargo/bin`) and asks for a release once he is happy with the batch. A release is then: bump `version` in `Cargo.toml`, `cargo build` so `Cargo.lock` follows, commit `chore: release X.Y.Z` on `main`, create the annotated tag (`git tag -a vX.Y.Z -m vX.Y.Z`), and hand the user `! git push origin main vX.Y.Z`. The tag push runs `.github/workflows/release.yml`: it checks the tag against the crate version, lints and tests, builds the macOS arm64 and Linux x86_64 binaries, and publishes the GitHub Release with generated notes, the archives (`slack-vX.Y.Z-<target>.tar.gz`) and `checksums.txt`. Its last job asks the Homebrew tap (`denrou/homebrew-slack`, same shape as the gh-prs and ggit taps) to rewrite its formula from `checksums.txt`; that job needs the `GH_TOKEN` secret on the repository and is skipped without it, in which case run `gh workflow run update-formula.yml --repo denrou/homebrew-slack -f version=X.Y.Z` by hand and watch it with `gh run watch --repo denrou/homebrew-slack`. `.github/workflows/ci.yml` runs fmt, clippy and the tests on every push to `main`.
- **Live checks.** `slack login` needs a human in the Chrome window; once the session is saved, every read-only command (`whoami`, `--count`, `unreads`) can be run from here to check the output. **Never run `send`, `read` or anything in the screen (`slack` alone) that writes from here**, not even to see an error path: `read` acts on whatever `~/.cache/slack/last-view.json` holds, and on 2026-10-19 a run meant to show the "nothing to mark" message marked eight channels as read instead. Inspect the cache file; leave the writes to the user.

## Security rules

- Never print, log or serialise a token (`xoxc`, `xoxd`, the Cookie header), in output, errors or `--json`.
- Only `https://*.slack.com` origins, never `app.slack.com`, may be paired with the session cookie: `capture::is_slack_workspace_url` is the gate, and it is load-bearing.
- A profile path from the environment is user input: a directory without our sentinel is never reused and never deleted.

## Decisions (2026-10-19)

Written down because none of them can be read from the code.

- **Blocking I/O, no async runtime.** A handful of sequential HTTP calls, one WebSocket for the capture, a `std::thread::scope` for the lookups. Switching later would cost `api.rs` and `auth/cdp.rs`, not the pure modules.
- **Credentials in `workspaces.json` (mode 0600), not the Keychain.** The footer polls `slack --count` from a launchd job, and the Keychain prompts whenever the binary changes, which is every rebuild. The browser profile next to it is a plaintext credential store anyway. A keychain backend can come later as an opt-in.
- **Mark-as-read is explicit** (`slack read`, `r` and `R` in the screen), never a side effect of reading, so the footer count keeps agreeing with the Slack app. The one exception is a reply to a conversation from the screen, which reads it up to the reply, as the app does. A mark never moves a cursor back: what the app read since is left alone.
- **Slack's own mute list is honoured; there is no local one.**
- **Every unread conversation is shown, muted ones excepted (2026-10-20).** The first rule hid public channels unless private or set to "All new messages" in Slack, and twice hid a channel Denis wanted to see (`#log-team-dev`, then `#proj-prediction-service`). He mutes what he does not want, so the rest is wanted: the view and the count follow Slack's sidebar, bold for anything unread, muted ones greyed out. The `-a` tier (`slack -a`, `--count --all`, `ui -a`, `read --all`, the `a` key) went with it.
- **The screen reads with context, the text view without (2026-10-20).** `slack` shows the last 50 messages of each unread conversation with the read part dimmed and a `── new ──` line, because a message is understood with what came before it. `slack unreads` keeps printing the new part alone: a script or Claude Code wants what is new and nothing else.
- **`slack` alone is the interactive view (2026-10-20).** Denis lives in the TUI; the text view is `slack unreads`, for scripts and for Claude Code reading messages. Without a terminal on both ends, `slack` prints the text instead of drawing, so a pipe never hangs on a screen.
- **Binary name `slack`**, kept because it is fast to type. Slack's platform CLI (`brew install slack-cli`) also installs a `slack` binary; it is not on this machine.
- **Public repo, MIT**, like gh-prs.

Kept from slackcli: the browser-session credentials and their capture (`auth/`). Deliberately not ported: app tokens, `parse-curl`, drafts, scheduling, canvases, emoji, files, saved items, search, user groups, the update checker, log files, Windows. If one becomes needed, it is a feature, not a port.

## Not planned yet

`--workspace`, a `watch` mode, loading older history from the TUI.

**Auto-refresh of the screen** is wanted (Denis, 2026-10-20) but not designed: think it through before building it. A refresh today is one `client.counts` call plus one history call per unread conversation, so a timer multiplies the API load by the number of conversations; `State::replace_items` sends the focus back to the list, which would throw you out of a thread or a reply box mid-typing; and the loop blocks on `event::read`, so it needs a `poll` with a timeout first. The cheap version is to poll `client.counts` alone and reload only when a cursor or a count moved, keeping focus, cursor, thread and the reply box as they are.

## Reference implementation

[slackcli](https://github.com/shaharia-lab/slackcli) (TypeScript, Bun) is installed on this machine as `slackcli` and logged in. Clone it into a scratch directory to compare how an endpoint is handled; `src/lib/browser-auth.ts`, `browser-launcher.ts`, `cdp-client.ts`, `unread.ts` and `unread-messages.ts` are the files these modules were ported from. Its `docs/user-guide/authentication.md` documents the two credential kinds.

## Testing

Pure modules are unit-tested in-module, the drawing through ratatui's `TestBackend`. Most response shapes are inline `json!` in the modules' tests; `tests/fixtures/` holds the larger ones, with ids and names made up: `client_counts.json` was written from slackcli's field list and checked field for field against a live response on 2026-10-19 (the live one also carries `updated`, `history_invalid`, `channel_badges`, `activity_v2`, `alerts`, `saved`, none of which is read).
