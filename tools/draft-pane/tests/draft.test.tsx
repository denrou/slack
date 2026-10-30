import { expect, mock, test } from 'claude-code/testing'
import type { On, ProcessRunResult, UiCopyResult } from 'claude-code'

const TOOL = 'mcp__slack__draft'
const PANE = {
  plugin: 'slack',
  component: 'Pane',
  requestId: 'slack-draft',
  props: {
    title: 'Slack → #dev',
    isFocused: true,
    bodyColumns: 60,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 30 },
    view: {},
  },
} as const

type Run = { argv: readonly string[]; stdin?: string }

// Stands for the engine beneath the plugin, and records each command run,
// what went in the prompt box and on the clipboard: `slack send` answers
// `sender`'s, by default where the message landed, and the fix command
// `fixer`'s.
function engine(
  on: On,
  fixer: (stdin: string) => Partial<ProcessRunResult> = () => ({}),
  sender: () => Partial<ProcessRunResult> = () => ({}),
  clipboard: UiCopyResult = { isCopied: true },
): { runs: Run[]; filled: string[]; copied: string[] } {
  const runs: Run[] = []
  const filled: string[] = []
  const copied: string[] = []
  on('prompt.fill', (_, e) => {
    filled.push(e.text)
    return { isFilled: true }
  })
  on('ui.copy', (_, e) => {
    copied.push(e.text)
    return { value: clipboard }
  })
  on('tool.register', (_, e) => ({ value: { tool: e.name } }))
  on('command.register', (_, e) => ({ value: { command: e.name } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('ui.close', () => ({ value: undefined }))
  on('ui.toast', () => ({ value: undefined }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('prompt.submit', (_, e) => ({ text: e.text, context: e.context }))
  on('process.run', (_, e) => {
    runs.push({ argv: e.argv, stdin: e.init?.stdin })
    const ran: ProcessRunResult = {
      exitCode: 0,
      stdout:
        'Sent to #dev: https://acme.slack.com/archives/C0123456/p1730000000123456\n',
      stderr: '',
      isStdoutTruncated: false,
      isStderrTruncated: false,
      ...(e.argv[0] === 'sh' ? fixer(e.init?.stdin ?? '') : sender()),
    }

    return { value: ran }
  })

  return { runs, filled, copied }
}

// A prompt the person typed and sent with Enter.
function typed(text: string) {
  return { text, wait: false, origin: { kind: 'composer' } } as const
}

test('a stale number or a flag is never a target', async ($, on) => {
  engine(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })

  for (const target of ['2', '--help', '']) {
    const called = await $.tool.call({ tool: TOOL, target, text: 'hi' })
    expect(called.deny).toMatch(/target/)
  }
})

test('Send posts the text as the person edited it, Enter never does', async ($, on) => {
  const { runs } = engine(on)
  const session = mock.session(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })

  for (const surface of ['terminal', 'desktop'] as const) {
    runs.length = 0
    await $.tool.call({ tool: TOOL, target: '#dev', text: 'Hello' })

    const ui = await $.ui.mount({ ...PANE, surface })
    await ui.input({ key: 'text', text: 'Hello all', kind: 'change' })
    await ui.input({ key: 'text', text: 'Hello all,\nthe build is green' })
    expect(runs).toHaveLength(0)
    expect(await ui.find({ key: 'fix' })).toBeUndefined()

    await ui.press({ key: 'send' })
    expect(runs).toEqual([
      {
        argv: ['slack', 'send', '#dev'],
        stdin: 'Hello all,\nthe build is green',
      },
    ])
    expect((await ui.find({ type: 'Text', text: /^Sent\./ }))?.text).toMatch(
      /p1730000000123456/,
    )
    expect(await ui.find({ key: 'send' })).toBeUndefined()
    expect(JSON.stringify(session.appended().at(-1)?.message)).toMatch(
      /the build is green/,
    )
    await ui.press({ key: 'discard' })
    await ui.unmount()
  }
})

test('two presses before the redraw send once', async ($, on) => {
  const { runs } = engine(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '#dev', text: 'Once' })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await Promise.all([ui.press({ key: 'send' }), ui.press({ key: 'send' })])
  expect(runs).toHaveLength(1)
  await ui.unmount()
})

test('a thread reply goes out with --thread', async ($, on) => {
  const { runs } = engine(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({
    tool: TOOL,
    target: '#dev',
    thread: '1730000000.123456',
    text: 'Done',
  })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'send' })
  expect(runs[0]?.argv).toEqual([
    'slack',
    'send',
    '#dev',
    '--thread',
    '1730000000.123456',
  ])
  await ui.unmount()
})

test("the person's edit reaches Claude with their next prompt, once", async ($, on) => {
  engine(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '@jane', text: 'Hi Jane' })

  const untouched = await $.prompt.submit(typed('shorter'))
  expect(untouched.context ?? []).toHaveLength(0)

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.input({ key: 'text', text: 'Hi Jane, quick one', kind: 'change' })
  const edited = await $.prompt.submit(typed('shorter'))
  expect(edited.context?.join('\n')).toMatch(/Hi Jane, quick one/)

  const again = await $.prompt.submit(typed('and warmer'))
  expect(again.context ?? []).toHaveLength(0)
  await ui.unmount()
})

test('Discard sends nothing and says so', async ($, on) => {
  const { runs } = engine(on)
  const session = mock.session(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '@jane', text: 'Hi Jane' })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'discard' })
  expect(runs).toHaveLength(0)
  expect(JSON.stringify(session.appended().at(-1)?.message)).toMatch(
    /discarded/,
  )
  expect((await ui.find({ type: 'Text' }))?.text).toMatch(/No draft/)
  await ui.unmount()
})

const FIX = { options: { fixCommand: 'spellcheck --raw' } }

test(
  'Fix corrects the text, Undo fix puts back what was typed',
  FIX,
  async ($, on) => {
    const { runs } = engine(on, stdin => ({
      stdout: stdin.includes('teh build')
        ? '<message>\nthe build is green\n</message>\n'
        : '',
    }))
    await $.session.start({
      cwd: '/',
      surface: 'terminal',
      isInteractive: true,
    })

    for (const surface of ['terminal', 'desktop'] as const) {
      runs.length = 0
      await $.tool.call({
        tool: TOOL,
        target: '#dev',
        text: 'teh build is green',
      })
      const ui = await $.ui.mount({ ...PANE, surface })

      await ui.press({ key: 'fix' })
      expect(runs[0]?.argv).toEqual(['sh', '-c', 'spellcheck --raw'])
      expect(runs[0]?.stdin).toMatch(
        /<message>\nteh build is green\n<\/message>/,
      )
      expect((await ui.find({ key: 'text' }))?.props.value).toBe(
        'the build is green',
      )

      await ui.press({ key: 'undo' })
      expect((await ui.find({ key: 'text' }))?.props.value).toBe(
        'teh build is green',
      )
      expect(await ui.find({ key: 'undo' })).toBeUndefined()

      // Enter in the field corrects too, and never sends.
      await ui.input({ key: 'text', text: 'teh build is green, again' })
      expect(runs.map(run => run.argv[0])).toEqual(['sh', 'sh'])
      expect((await ui.find({ key: 'text' }))?.props.value).toBe(
        'the build is green',
      )
      await ui.press({ key: 'discard' })
      await ui.unmount()
    }
  },
)

test('a fix that fails keeps the text and says why', FIX, async ($, on) => {
  engine(on, () => ({
    exitCode: 1,
    stdout: '',
    stderr: 'API error: overloaded\n',
  }))
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '#dev', text: 'teh build' })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'fix' })
  expect((await ui.find({ key: 'text' }))?.props.value).toBe('teh build')
  expect(
    (await ui.find({ type: 'Text', text: /Not corrected/ }))?.text,
  ).toMatch(/overloaded/)
  expect(await ui.find({ key: 'send' })).toBeDefined()
  await ui.unmount()
})

test('Discard pressed with Send never says nothing was sent', async ($, on) => {
  const { runs } = engine(on)
  const session = mock.session(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '#dev', text: 'Hello' })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await Promise.all([ui.press({ key: 'send' }), ui.press({ key: 'discard' })])
  // Whichever press wins, the note says what happened.
  const notes = session.appended().map(row => JSON.stringify(row.message))
  expect(notes.some(note => note.includes('discarded'))).toBe(runs.length === 0)
  expect(runs.length).toBeLessThanOrEqual(1)
  await ui.unmount()
})

test("a new draft never overwrites the user's unread edit", async ($, on) => {
  engine(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '@jane', text: 'Hi Jane' })

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.input({ key: 'text', text: 'Hi Jane, quick one', kind: 'change' })
  const refused = await $.tool.call({
    tool: TOOL,
    target: '@jane',
    text: 'Hey',
  })
  expect(refused.deny).toMatch(/Hi Jane, quick one/)
  expect((await ui.find({ key: 'text' }))?.props.value).toBe(
    'Hi Jane, quick one',
  )

  const revised = await $.tool.call({
    tool: TOOL,
    target: '@jane',
    text: 'Hey Jane, quick one',
  })
  expect(revised.deny).toBeUndefined()
  await ui.unmount()
})

test(
  'a correction drops the error of the send before it',
  FIX,
  async ($, on) => {
    engine(
      on,
      () => ({ stdout: 'the build' }),
      () => ({ exitCode: 1, stdout: '', stderr: 'channel_not_found' }),
    )
    await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
    await $.tool.call({ tool: TOOL, target: '#dev', text: 'teh build' })

    const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
    await ui.press({ key: 'send' })
    expect((await ui.find({ type: 'Text', text: /Not sent/ }))?.text).toMatch(
      /channel_not_found/,
    )
    await ui.press({ key: 'fix' })
    expect(await ui.find({ type: 'Text', text: /Not sent/ })).toBeUndefined()
    expect((await ui.find({ key: 'text' }))?.props.value).toBe('the build')
    await ui.unmount()
  },
)

test('Chat quotes the draft in the prompt box and keeps it', async ($, on) => {
  const { runs, filled } = engine(on)
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '@jane', text: 'Hi Jane' })

  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.input({ key: 'text', text: 'Hi Jane,\n\nquick one', kind: 'change' })
  await ui.press({ key: 'chat' })
  expect(filled).toEqual([
    'About the Slack draft to @jane:\n> Hi Jane,\n>\n> quick one\n\n',
  ])
  expect(runs).toHaveLength(0)

  // The quote already holds the edit, so the prompt does not add it again.
  const asked = await $.prompt.submit(typed('is it too short?'))
  expect(asked.context ?? []).toHaveLength(0)
  expect((await ui.find({ key: 'text' }))?.props.value).toBe(
    'Hi Jane,\n\nquick one',
  )
  await ui.unmount()
})

test('Copy puts the draft on the clipboard and drops it', async ($, on) => {
  const { runs, copied } = engine(on)
  const session = mock.session(on)
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '#dev', text: 'Hello' })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.input({ key: 'text', text: 'Hello all', kind: 'change' })
  await ui.press({ key: 'copy' })
  expect(copied).toEqual(['Hello all'])
  expect(runs).toHaveLength(0)
  expect(JSON.stringify(session.appended().at(-1)?.message)).toMatch(
    /copied.*nothing was sent/,
  )
  expect((await ui.find({ type: 'Text' }))?.text).toMatch(/No draft/)
  await ui.unmount()
})

test('a copy the clipboard refuses keeps the draft', async ($, on) => {
  const { copied } = engine(on, undefined, undefined, {
    isCopied: false,
    reason: 'no-clipboard',
  })
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.tool.call({ tool: TOOL, target: '#dev', text: 'Hello' })

  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'copy' })
  expect(copied).toEqual(['Hello'])
  expect(await ui.find({ key: 'send' })).toBeDefined()
  await ui.unmount()
})
