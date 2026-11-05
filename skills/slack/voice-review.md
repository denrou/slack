# Voice review

The drafts written from here are written in the user's voice, by the writing-voice rule. The review sets them beside what the user sent and tunes the rule from the difference. It runs when the user asks ("review my voice", "tune the writing voice"), about once a month.

## 1. Find the rule

`~/.claude/rules/writing-voice.md`. A symlink is followed: the change lands where the file lives, and when that is a git repository, say so, since the commit is the user's. No rule yet: step 5 writes the first one.

## 2. Gather

- **The pairs.** `slack drafts --days 30 --json` (90 at most, the history Slack's free plan keeps). Each entry is a draft as saved, its `outcome` and, when there is one, what was `sent`:
  - `unchanged`: sent as written, the rule held.
  - `edited`: the message carries the draft's id, or `matched_by: next` when it is only the user's next message there, which may answer something else; read it as such.
  - `not_sent`: nothing from the user there within three days. Either the draft was not wanted, or the message went elsewhere; the transcript tells which.
  - `pending`: nothing yet, and its three days are not over; leave it for the next review.
  - `unknown`: the place could not be read far enough; leave it out.
- **The corrections.** A pair with a `session` came from that Claude Code session: its transcript, `~/.claude/projects/*/<session>.jsonl`, holds what the user asked to change before the draft was saved ("shorter", "in French", "drop the emoji"). These are the strongest evidence, the user's own words about their voice. Transcripts are kept for `cleanupPeriodDays` (30 days unless set), so a missing one is no error.
- **The user's own writing.** `slack search 'from:me after:<date>' -n 100`, narrowed with `in:@person` or `in:#channel` for more, to check the rule against messages no draft touched, the language and register used with each person above all.

## 3. Sort each difference

- **Voice**: how a message opens and ends, its length, the language, `tu` or `vous`, the emoji, the hedging, lists or prose, the signposts.
- **Content**: a fact, a decision, a link or a date the user added that the draft could not know. Not a rule: leave it out.

## 4. Decide

- A change needs the same pattern in at least three pairs or corrections. Fewer is an observation, reported and not written, to look at again next time.
- A rule the user keeps editing out is reworded or removed: the rule gets shorter as often as longer.
- Slack evidence changes the rule's Slack section and its general voice, never the sections of other formats (commits, tickets, documents).

## 5. Propose, then write on a yes

Report, in the conversation:

- how many drafts were sent as written, edited, not sent (a number to compare from one review to the next);
- the diff of the rule, each change with its evidence: the dates and places, a few words of the user's own wording;
- the observations left for next time.

Write the change once the user says yes. Quote only the user's own words in the rule, never a colleague's: the rule may be shared or published.

With no rule yet, write a first one from the user's messages (step 2's search over 90 days, people and channels varied): a `# Writing voice` title, a scope (the text written on the user's behalf, not the conversation with the agent), a `## Voice` list and a `## Slack` section, each point with the reason the messages show. Show it, and save it at `~/.claude/rules/writing-voice.md` on a yes.
