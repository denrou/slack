import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { Draft } from '../types'

const PANE = 'slack-draft'
const TOOL = 'mcp__slack__draft'
const draft = atom({ plugin: 'slack', key: 'draft' } as const, null, {
  shape: 'draft-2',
})

// `#channel`, `@handle`, a conversation id or a permalink. Deliberate: no
// number from the last listing, which may be another one by the time the
// person presses Send, and nothing `slack send` would read as a flag.
const TARGET =
  /^([#@]\S+|[CDG][A-Z0-9]{6,}|https:\/\/[\w-]+\.slack\.com\/archives\/\S+)$/
const THREAD = /^(\d+\.\d+|https:\/\/[\w-]+\.slack\.com\/archives\/\S+)$/

const DESCRIPTION = `Show a Slack message drafted for the user in an editable pane beside the chat, instead of in the reply. The user edits it there and presses Send, which runs \`slack send\` with their text: that press is their explicit yes, so never run \`slack send\` for a draft shown here. A note in the conversation says when it was sent (with the permalink) or discarded. Calling it again replaces the draft, for a revision. The text is Slack markup, written as the user would.`

const PLACED =
  'The draft is in the pane. Tell the user in one line it is there to edit and send; do not repeat it, and do not run `slack send` for it.'
const WAITING =
  'The pane could not be shown (the terminal is too narrow for a pane opened unasked). Show the draft in your reply with its target, and tell the user `/slack-draft` opens the pane to edit and send it.'

function titleOf(target: string): string {
  return `Slack → ${target.length > 40 ? `${target.slice(0, 39)}…` : target}`
}

async function tell($: EngineInterface, text: string): Promise<void> {
  try {
    await $.session.append({
      message: { type: 'user', content: [{ type: 'text', text }] },
    })
  } catch {
    // The toast still says what happened; a session that refuses the note
    // only leaves Claude unaware of it.
  }
}

// Where the person can still change the draft, correct it or send it.
function isOpen(d: Draft | null): d is Draft {
  return d !== null && (d.status === 'editing' || d.status === 'failed')
}

// Edits land in order and before a Send or a Fix reads the text: a keystroke
// still on its way would otherwise miss the claim and be dropped.
let edits: Promise<unknown> = Promise.resolve()

// Claimed inside the versioned write, so a second press before the redraw
// finds the draft busy and a message never goes out twice.
async function claim(
  $: EngineInterface,
  status: 'sending' | 'fixing',
): Promise<Draft | null> {
  await edits
  let claimed = null as Draft | null
  await update($, draft, (d): Draft | null => {
    claimed = isOpen(d) && d.text.trim() !== '' ? d : null
    return claimed
      ? { ...claimed, status, outcome: undefined, fixNote: undefined }
      : d
  })
  return claimed
}

async function send($: EngineInterface): Promise<void> {
  const now = await claim($, 'sending')
  if (now === null) {
    if ((await read($, draft))?.text.trim() === '') {
      $.ui.toast('Nothing to send.')
    }
    return
  }

  const argv = ['slack', 'send', now.target]
  if (now.thread !== undefined) {
    argv.push('--thread', now.thread)
  }
  let isSent = false
  let outcome: string
  try {
    const run = await $.process.run(argv, {
      stdin: now.text,
      timeoutMs: 60_000,
    })
    isSent = run.exitCode === 0
    outcome =
      (isSent ? run.stdout : run.stderr).trim() ||
      `slack send exited with ${run.exitCode}`
  } catch (error) {
    outcome = `slack send did not run: ${error instanceof Error ? error.message : String(error)}`
  }
  await update(
    $,
    draft,
    (d): Draft | null =>
      d && { ...d, status: isSent ? 'sent' : 'failed', outcome },
  )

  if (!isSent) {
    $.ui.toast(`Not sent to ${now.target}`)
    return
  }
  $.ui.toast(`Sent to ${now.target}`)
  await tell(
    $,
    `The user sent the Slack draft to ${now.target} from the draft pane, with this text:\n\n${now.text}\n\n\`slack send\` printed:\n${outcome}`,
  )
}

async function discard($: EngineInterface): Promise<void> {
  let dropped = null as Draft | null
  await update($, draft, (d): Draft | null => {
    const isBusy = d?.status === 'sending' || d?.status === 'fixing'
    dropped = isBusy ? null : d
    return isBusy ? d : null
  })
  const was = dropped
  if (was === null) {
    return
  }
  await $.ui.close({ id: PANE })
  if (was.status !== 'sent') {
    await tell(
      $,
      `The user discarded the Slack draft to ${was.target}; nothing was sent.`,
    )
  }
}

function keep($: EngineInterface, text: string): Promise<unknown> {
  edits = edits
    .then(() =>
      update($, draft, (d): Draft | null =>
        isOpen(d) && d.text !== text
          ? {
              ...d,
              text,
              status: 'editing',
              outcome: undefined,
              typed: undefined,
              fixNote: undefined,
            }
          : d,
      ),
    )
    .catch(() => undefined)
  return edits
}

// The instructions of the screen's Ctrl-G (`src/fix.rs`, which goes with the
// screen in #54), so a command set up for it reads the same prompt.
function fixPrompt(text: string): string {
  return `Correct the spelling, the missing accents and the grammar of the Slack message between the <message> tags, in the language it is written in. Do not rephrase: keep the words, the tone, the meaning and the line breaks. Leave Slack markup, emoji codes, mentions, code and URLs as they are. Reply with the corrected message alone, without the tags and without a comment; if nothing needs correcting, reply with the message unchanged.\n\n<message>\n${text}\n</message>\n`
}

function cleaned(output: string): string {
  let text = output.trim()
  if (text.startsWith('<message>')) text = text.slice('<message>'.length)
  if (text.endsWith('</message>')) text = text.slice(0, -'</message>'.length)
  return text.trim()
}

async function fix($: EngineInterface, command: string): Promise<void> {
  const before = await claim($, 'fixing')
  if (before === null) {
    return
  }

  let fixed = ''
  let fixNote: string | undefined
  try {
    // Deliberate: from `/`, since an agent CLI such as `claude -p` reads the
    // project files of the directory it starts in.
    const run = await $.process.run(['sh', '-c', command], {
      stdin: fixPrompt(before.text),
      cwd: '/',
      timeoutMs: 30_000,
    })
    fixed = cleaned(run.stdout)
    if (run.exitCode !== 0) {
      const last = run.stderr.trim().split('\n').at(-1)
      fixNote = `the fix command failed: ${last || `exit ${run.exitCode}`}`
    } else if (run.isStdoutTruncated) {
      fixNote = 'the fix command printed more than a message holds'
    } else if (fixed === '') {
      fixNote = 'the fix command printed nothing'
    }
  } catch (error) {
    fixNote = `the fix command did not answer: ${error instanceof Error ? error.message : String(error)}`
  }
  const isChanged = fixNote === undefined && fixed !== before.text
  await update($, draft, (d): Draft | null =>
    d && d.status === 'fixing'
      ? {
          ...d,
          status: 'editing',
          fixNote,
          ...(isChanged ? { text: fixed, typed: before.text } : {}),
        }
      : d,
  )
  if (fixNote === undefined && !isChanged) {
    $.ui.toast('Nothing to correct.')
  }
}

function undoFix($: EngineInterface): Promise<unknown> {
  return update($, draft, d =>
    isOpen(d) && d.typed !== undefined
      ? { ...d, text: d.typed, typed: undefined }
      : d,
  )
}

export const register: Register = (on, options) => {
  const fixCommand =
    typeof options.fixCommand === 'string' ? options.fixCommand.trim() : ''

  on('session.start', async ($, e, next) => {
    // A run cut short by a reload or a crash leaves no answer behind: the
    // draft is handed back, and a send that may have gone out says so.
    await update($, draft, (d): Draft | null =>
      d?.status === 'sending'
        ? {
            ...d,
            status: 'failed',
            outcome: 'interrupted, it may have gone out: check Slack first',
          }
        : d?.status === 'fixing'
          ? { ...d, status: 'editing' }
          : d,
    )
    await $.tool.register({
      name: 'draft',
      description: DESCRIPTION,
      isDeferred: false,
      inputSchema: {
        type: 'object',
        properties: {
          target: {
            type: 'string',
            description:
              '`#channel`, `@handle`, a conversation id or a permalink (a reply in that thread)',
          },
          thread: {
            type: 'string',
            description: 'Reply in this thread: its ts or permalink',
          },
          text: { type: 'string', description: 'The message, in Slack markup' },
        },
        required: ['target', 'text'],
      },
    })
    await $.command.register({
      name: 'slack-draft',
      description: 'Open the pane of the current Slack draft',
    })

    return next(e)
  })

  on('tool.call', { tool: TOOL }, async ($, e) => {
    const { target, thread, text } = e as unknown as Record<string, unknown>
    if (typeof target !== 'string' || !TARGET.test(target)) {
      return {
        deny: 'target must be `#channel`, `@handle`, a conversation id or a permalink; a number from a listing is not taken.',
      }
    }
    if (
      thread !== undefined &&
      (typeof thread !== 'string' || !THREAD.test(thread))
    ) {
      return {
        deny: 'thread must be a ts (`1730000000.123456`) or a permalink.',
      }
    }
    if (typeof text !== 'string' || text.trim() === '') {
      return { deny: 'text is empty.' }
    }
    await edits
    const current = await read($, draft)
    if (current?.status === 'sending') {
      return { deny: 'The previous draft is being sent; wait for its note.' }
    }
    if (isOpen(current) && current.text !== current.seen) {
      await update($, draft, d => d && { ...d, seen: current.text })
      return {
        deny: `The user edited the draft in the pane since you last read it, and it is kept as they left it:\n\n${current.text}\n\nRevise from this text if a change is still wanted, then call the tool again.`,
      }
    }

    const next: Draft = { target, thread, text, seen: text, status: 'editing' }
    await update($, draft, () => next)
    const opened = await $.ui.open({
      id: PANE,
      title: titleOf(target),
      focus: true,
    })

    return { result: opened.isPlaced ? PLACED : WAITING }
  })

  on('command.run', { command: 'slack-draft' }, async $ => {
    const now = await read($, draft)
    if (now === null) {
      return { text: 'No Slack draft to show.' }
    }
    await $.ui.open({ id: PANE, title: titleOf(now.target), focus: true })

    return { text: `Slack draft to ${now.target} opened in its pane.` }
  })

  // An edit the person made in the pane reaches Claude with their next prompt,
  // so "shorter" revises their text rather than Claude's earlier one.
  on('prompt.submit', async ($, e, next) => {
    const now = await read($, draft)
    if (e.command !== undefined || !isOpen(now) || now.text === now.seen) {
      return next(e)
    }
    await update($, draft, d => d && { ...d, seen: now.text })

    return next({
      ...e,
      context: [
        ...(e.context ?? []),
        `The Slack draft pane for ${now.target} now holds this text, edited by the user; work from it:\n\n${now.text}`,
      ],
    })
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const now = await read($, draft)
    const { Box, Text, Button } = $.ui.resolve(e)

    if (now === null) {
      return (
        <Text dimColor>No draft. Ask Claude to draft a Slack message.</Text>
      )
    }

    const isEditable = isOpen(now)
    const lines = now.text.split('\n').length
    const rows = Math.max(4, Math.min(lines + 1, e.props.scroll.bodyRows - 5))
    let field = <Text>{now.text}</Text>
    if (isEditable && e.surface !== 'mobile') {
      const { Input } = $.ui.resolve(e)
      field = (
        <Input
          key="text"
          rows={rows}
          value={now.text}
          submitLabel={fixCommand === '' ? 'keep' : 'fix'}
          autoFocus
          onInput={text => void keep($, text)}
          onSubmit={async text => {
            await keep($, text)
            if (fixCommand !== '') await fix($, fixCommand)
          }}
        />
      )
    }

    return (
      <Box flexDirection="column" gap={1}>
        <Text dimColor>
          To {now.target}
          {now.thread !== undefined ? ', in the thread' : ''}
        </Text>
        {field}
        {now.status === 'fixing' && <Text dimColor>Correcting…</Text>}
        {now.fixNote !== undefined && (
          <Text color="warning">Not corrected: {now.fixNote}</Text>
        )}
        {now.status === 'sending' && <Text dimColor>Sending…</Text>}
        {now.status === 'failed' && (
          <Text color="error">Not sent: {now.outcome}</Text>
        )}
        {now.status === 'sent' && (
          <Text color="success">Sent. {now.outcome}</Text>
        )}
        <Box gap={1}>
          {isEditable && (
            <Button
              key="send"
              label="Send"
              variant="primary"
              onPress={() => send($)}
            />
          )}
          {isEditable && fixCommand !== '' && (
            <Button key="fix" label="Fix" onPress={() => fix($, fixCommand)} />
          )}
          {isEditable && now.typed !== undefined && (
            <Button key="undo" label="Undo fix" onPress={() => undoFix($)} />
          )}
          {now.status !== 'sending' && now.status !== 'fixing' && (
            <Button
              key="discard"
              label={now.status === 'sent' ? 'Close' : 'Discard'}
              onPress={() => discard($)}
            />
          )}
        </Box>
      </Box>
    )
  })
}
