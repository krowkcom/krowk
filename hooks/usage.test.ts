import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code/testing'
import { configHome, payload, PICKER, POLL_MS } from './usage'
import type { Choices } from './usage'

const LIMITS = [
  { kind: 'five_hour', percentUsed: 37.5, resetsAt: '2026-11-23T14:00:00.000Z' },
  { kind: 'seven_day', percentUsed: 12, resetsAt: '2026-11-28T09:00:00Z' },
  { kind: 'spend_limit', percentUsed: 120 },
]

test('the writer sends the 5-hour and 7-day windows with their resets, and nothing without them', () => {
  expect(JSON.parse(payload('/h/.claude', LIMITS) ?? 'null')).toEqual({
    configHome: '/h/.claude',
    windows: [
      { kind: 'fiveHour', usedPercent: 37.5, resetsAtMs: Date.UTC(2026, 10, 23, 14) },
      { kind: 'sevenDay', usedPercent: 12, resetsAtMs: Date.UTC(2026, 10, 28, 9) },
    ],
  })
  expect(JSON.parse(payload('/h/.claude', [{ kind: 'five_hour', percentUsed: 3, resetsAt: 'soon' }]) ?? 'null')).toEqual({ configHome: '/h/.claude', windows: [{ kind: 'fiveHour', usedPercent: 3 }] })
  expect(payload('/h/.claude', [])).toBeUndefined()
  expect(payload('/h/.claude', [{ kind: 'spend_limit', percentUsed: 5 }])).toBeUndefined()
})

test('the config home is CLAUDE_CONFIG_DIR, else ~/.claude', () => {
  expect(configHome('/work/.claude-b', '/home/ada')).toBe('/work/.claude-b')
  expect(configHome(undefined, '/home/ada/')).toBe('/home/ada/.claude')
  expect(configHome('', '/home/ada')).toBe('/home/ada/.claude')
  expect(configHome(undefined, undefined)).toBeUndefined()
})

test('a moved window runs `gruntas hook claude usage` with the windows on stdin', async ($, on) => {
  const ran: { argv: readonly string[]; stdin?: string }[] = []
  on('process.run', (_, e) => {
    ran.push({ argv: e.argv, stdin: e.init?.stdin })
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('env.get', (_, e) => ({ value: e.name === 'CLAUDE_CONFIG_DIR' ? '/work/.claude-b' : '/home/ada' }))
  on('session.measure', (_, e) => ({ changed: e.changed }))
  const context = { size: 200_000 } as never
  await $.session.measure({ context, rateLimits: LIMITS, changed: ['context'] })
  expect(ran).toEqual([])
  await $.session.measure({ context, rateLimits: LIMITS, changed: ['rateLimits'] })
  // The send is not awaited by the measurement: let it run.
  for (let i = 0; i < 100 && ran.length === 0; i++) await Promise.resolve()
  expect(ran.map(r => r.argv)).toEqual([['gruntas', 'hook', 'claude', 'usage']])
  expect(JSON.parse(ran[0]?.stdin ?? 'null').configHome).toBe('/work/.claude-b')
})

test('no gruntas on the PATH leaves the session as it was', async ($, on) => {
  on('process.run', () => {
    throw new Error('spawn gruntas ENOENT')
  })
  on('env.get', () => ({ value: '/home/ada' }))
  on('session.measure', (_, e) => ({ changed: e.changed }))
  const context = { size: 200_000 } as never
  await expect($.session.measure({ context, rateLimits: LIMITS, changed: ['rateLimits'] })).resolves.toEqual({ changed: ['rateLimits'] })
})

const NOW = Date.UTC(2026, 10, 24, 10)
const DIR = '/run/gruntas/abc/inbox'
const FRAMED = '<steer from 01a0ec7b-0000-7000-8000-0000000000c1 on laptop, by an agent>\nrebase first\n</steer>'

/**
 * A session `s1` whose inbox holds `lines`, its acks file kept in `files`;
 * `poll` lets the mod read it once.
 */
function inbox(on: On, lines: readonly object[]): { files: Map<string, string>; poll: () => Promise<void> } {
  const files = new Map([[`${DIR}/s1.jsonl`, lines.map(l => JSON.stringify(l)).join('\n') + '\n']])
  const clock = mock.clock(on, { now: NOW })
  mock.env(on, {})
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.id', () => ({ value: 's1' }))
  on('process.run', (_, e) => ({ value: { exitCode: 0, stdout: e.argv.includes('inbox') ? `${DIR}\n` : '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }))
  on('fs.read', (_, e) => {
    const text = files.get(e.path)
    if (text === undefined) throw new Error(`ENOENT: ${e.path}`)
    return { value: text }
  })
  on('fs.write', (_, e) => {
    files.set(e.path, e.text)
    return { value: undefined }
  })
  return { files, poll: () => clock.advance(POLL_MS) }
}

const start = ($: Parameters<Parameters<typeof test>[1]>[0]) => $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })

const acks = (files: Map<string, string>) => (files.get(`${DIR}/s1.acks.jsonl`) ?? '').trim().split('\n').filter(Boolean).map(l => JSON.parse(l))

test('a queued message is submitted once, framed, and acked; a stale or unframed one is not submitted', async ($, on) => {
  const at = new Date(NOW - 1_000).toISOString()
  const { files, poll } = inbox(on, [
    { type: 'message', id: 'm1', text: FRAMED, origin: { kind: 'agent' }, at },
    { type: 'message', id: 'm0', text: FRAMED, origin: { kind: 'agent' }, at: new Date(NOW - 60_000).toISOString() },
    { type: 'message', id: 'm2', text: 'rebase first', origin: { kind: 'agent' }, at },
  ])
  const submitted: string[] = []
  on('prompt.submit', (_, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  await start($)
  await poll()
  await poll()
  expect(submitted).toEqual([FRAMED])
  expect(acks(files)).toEqual([
    { id: 'm1', outcome: 'delivered' },
    { id: 'm2', outcome: 'refused', message: 'the message came without its origin' },
  ])
})

test('an interrupt aborts the tracked turn, or reports nothing running', async ($, on) => {
  const at = new Date(NOW).toISOString()
  const { files, poll } = inbox(on, [{ type: 'interrupt', id: 'i1', origin: { kind: 'agent' }, at }])
  const aborted: string[] = []
  on('turn.abort', (_, e) => {
    aborted.push(e.turnId)
    return { value: undefined }
  })
  on('turn.start', (_, e) => ({ turnId: e.turnId }))
  on('turn.complete', (_, e) => ({ text: e.answer }))
  await start($)
  await poll()
  expect(aborted).toEqual([])
  expect(acks(files)).toEqual([{ id: 'i1', outcome: 'nothingRunning' }])

  files.set(`${DIR}/s1.jsonl`, JSON.stringify({ type: 'interrupt', id: 'i2', origin: { kind: 'agent' }, at }) + '\n')
  await $.turn.start({ text: 'go', turnId: 't1' })
  // A subagent's turn is not the one an interrupt stops.
  await $.turn.start({ text: 'sub', turnId: 't2', agentId: 'a1' })
  await poll()
  expect(aborted).toEqual(['t1'])
  expect(acks(files).at(-1)).toEqual({ id: 'i2', outcome: 'delivered' })
})

test('a notice of the next account shows as a toast and in the status line, once', async ($, on) => {
  const text = 'This account has hit its usage limit. Next: claude:b on opus (another subscription) — type /exit to continue this session there.'
  const { files, poll } = inbox(on, [{ type: 'notice', id: 'n1', text, at: new Date(NOW).toISOString() }])
  const toasts: string[] = []
  const statuses: string[] = []
  on('ui.toast', (_, e) => {
    toasts.push(e.text)
    return { value: undefined }
  })
  on('ui.status', (_, e) => {
    statuses.push(e.text)
    return { value: undefined }
  })
  await start($)
  await poll()
  await poll()
  expect(toasts).toEqual([text])
  expect(statuses).toEqual([text])
  expect(acks(files)).toEqual([{ id: 'n1', outcome: 'delivered' }])
})

const LIST = '  claude        ready  a@x.io (max) · 5h 60% left, resets Tue 17:48\n* claude:trip1  ready  t@x.io (max) · usage unknown\n'

test('/gruntas:account lists the accounts from gruntas, the current one marked, with no model turn', async ($, on) => {
  const ran: (readonly string[])[] = []
  const turns: string[] = []
  on('session.id', () => ({ value: 's1' }))
  on('process.run', (_, e) => {
    ran.push(e.argv)
    // Outside a session `gruntas run` started: no picker, the list.
    const stdout = e.argv.at(-1) === '--choices' ? JSON.stringify({ launched: false, current: 'claude:trip1', queued: null, choices: [], list: LIST.trimEnd() }) : LIST
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('prompt.submit', (_, e) => {
    turns.push(e.text)
    return { text: e.text }
  })
  on('turn.start', (_, e) => {
    turns.push(e.text)
    return { turnId: e.turnId }
  })
  const r = await $.command.run({ command: 'gruntas:account', args: '' })
  // One run of gruntas: --choices carries the list where there is no picker.
  expect(ran).toEqual([['gruntas', 'hook', 'claude', 'account', 's1', '--choices']])
  expect(r.text).toBe(LIST.trimEnd())
  expect(r.text).toContain('* claude:trip1')
  expect(r.isTextDisplayOnly).toBe(true)
  expect(turns).toEqual([])
})

test('/gruntas:account <name> queues it and keeps the notice in the status line; a refusal does not', async ($, on) => {
  const statuses: (string | undefined)[] = []
  let answer = { exitCode: 0, stdout: 'Type /exit to continue on claude:trip1.\n' }
  on('session.id', () => ({ value: 's1' }))
  on('process.run', (_, e) => ({ value: { ...answer, stdout: e.argv.at(-1) === '--cancel' ? 'claude:trip1 is no longer queued: /exit ends this session as usual.\n' : answer.stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }))
  on('ui.status', (_, e) => {
    statuses.push(e.text)
    return { value: undefined }
  })
  expect((await $.command.run({ command: 'gruntas:account', args: ' claude:trip1 ' })).text).toBe('Type /exit to continue on claude:trip1.')
  expect((await $.command.run({ command: 'gruntas:account', args: '--cancel' })).text).toBe('claude:trip1 is no longer queued: /exit ends this session as usual.')
  answer = { exitCode: 1, stdout: 'There is no account named nope: choose one of claude:trip1.\n' }
  expect((await $.command.run({ command: 'gruntas:account', args: 'nope' })).text).toBe('There is no account named nope: choose one of claude:trip1.')
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(statuses).toEqual(['Type /exit to continue on claude:trip1.', undefined])
})

test('/gruntas:account without gruntas says how to install it', async ($, on) => {
  on('session.id', () => ({ value: 's1' }))
  on('process.run', () => {
    throw new Error('spawn gruntas ENOENT')
  })
  expect((await $.command.run({ command: 'gruntas:account', args: '' })).text).toContain('curl -fsSL https://gruntas.com/install | bash')
})

const CHOICES: Choices = {
  launched: true,
  current: 'claude',
  queued: null,
  choices: [
    { account: 'claude:primevise', label: 'claude:primevise · max · 7d 100% left', why: null },
    { account: 'claude:trip1', label: 'claude:trip1 · usage unknown', why: null },
    { account: 'claude', label: 'claude · this session', why: 'this session runs on it' },
    { account: 'codex:team', label: 'codex:team · Codex account', why: 'it is a Codex account, and a Claude Code session moves only to another Claude Code account' },
  ],
}

/** A session `gruntas run` started, with the picker's answers and what the mod did. */
function launched(on: Parameters<Parameters<typeof test>[1]>[1], answer?: (argv: readonly string[]) => Promise<string>) {
  const did = { ran: [] as (readonly string[])[], opened: [] as unknown[], closed: [] as string[], commands: [] as string[], toasts: [] as string[], turns: [] as string[] }
  on('session.id', () => ({ value: 's1' }))
  on('process.run', async (_, e) => {
    did.ran.push(e.argv)
    const last = e.argv.at(-1)
    const stdout = last === '--choices' ? JSON.stringify(CHOICES) : answer !== undefined ? await answer(e.argv) : `Type /exit to continue on ${last}.\n`
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('ui.open', (_, e) => (did.opened.push(e), { value: { isPlaced: true } }))
  on('ui.close', (_, e) => (did.closed.push(e.id), { value: undefined }))
  on('ui.toast', (_, e) => (did.toasts.push(e.text), { value: undefined }))
  on('ui.status', () => ({ value: undefined }))
  on('command.run', { command: 'exit' }, (_, e) => (did.commands.push(e.command), { text: '' }))
  on('turn.start', (_, e) => (did.turns.push(e.text), { turnId: e.turnId }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  mock.clock(on, { now: 0 })
  mock.env(on, {})
  return did
}

/** The session started as the engine starts it; only `gruntas hook claude account` runs counted. */
async function begin($: Parameters<Parameters<typeof test>[1]>[0], did: ReturnType<typeof launched>) {
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true } as never)
  for (let i = 0; i < 20; i++) await Promise.resolve()
  did.ran.length = 0
}

test('bare /gruntas:account opens a picker of every account; Enter on one queues it and runs /exit', async ($, on) => {
  const did = launched(on)
  await begin($, did)
  const r = await $.command.run({ command: 'gruntas:account', args: '' })
  expect(r.text).toBeUndefined()
  expect(did.opened).toEqual([expect.objectContaining({ id: PICKER, focus: true, closeOnEscape: true })])
  const ui = await $.ui.mount({ plugin: 'gruntas', surface: 'terminal', component: 'Pane', requestId: PICKER, props: { title: 'Continue this session on', isFocused: true, bodyColumns: 80, placement: 'dock', scroll: { offset: 0, bodyRows: 10 }, view: {} } } as never)
  const select = await ui.find({ key: 'account' })
  const labels = (select?.props.options as { label: string }[]).map(o => o.label)
  expect(labels).toEqual(['claude:primevise · max · 7d 100% left', 'claude:trip1 · usage unknown', 'claude · this session', 'codex:team · Codex account'])
  expect(select?.props.value).toBe('claude')
  await ui.select({ key: 'account', value: 'claude:trip1' })
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(did.ran.filter(a => a.includes('account')).at(-1)).toEqual(['gruntas', 'hook', 'claude', 'account', 's1', 'claude:trip1'])
  expect(did.closed).toEqual([PICKER])
  expect(did.commands).toEqual(['exit'])
  expect(did.turns).toEqual([])
  await ui.unmount()
})

test('Enter on an account the session cannot move to says why and keeps the picker open', async ($, on) => {
  const did = launched(on)
  await begin($, did)
  await $.command.run({ command: 'gruntas:account', args: '' })
  const ui = await $.ui.mount({ plugin: 'gruntas', surface: 'terminal', component: 'Pane', requestId: PICKER, props: { title: 'Continue this session on', isFocused: true, bodyColumns: 80, placement: 'dock', scroll: { offset: 0, bodyRows: 10 }, view: {} } } as never)
  await ui.select({ key: 'account', value: 'codex:team' })
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(did.toasts).toEqual(["Not codex:team: it is a Codex account, and a Claude Code session moves only to another Claude Code account."])
  expect(did.ran.filter(a => a.includes('account')).map(a => a.at(-1))).toEqual(['--choices'])
  expect(did.closed).toEqual([])
  expect(did.commands).toEqual([])
  await ui.unmount()
})

test('a second Enter while gruntas queues a pick is ignored: one queue, one /exit', async ($, on) => {
  let release: () => void = () => undefined
  const held = new Promise<void>(r => (release = r))
  const did = launched(on, async argv => {
    if (argv.at(-1) === 'claude:trip1') await held
    return `Type /exit to continue on ${argv.at(-1)}.\n`
  })
  await begin($, did)
  await $.command.run({ command: 'gruntas:account', args: '' })
  const ui = await $.ui.mount({ plugin: 'gruntas', surface: 'terminal', component: 'Pane', requestId: PICKER, props: { title: 'Continue this session on', isFocused: true, bodyColumns: 80, placement: 'dock', scroll: { offset: 0, bodyRows: 10 }, view: {} } } as never)
  const first = ui.select({ key: 'account', value: 'claude:trip1' })
  for (let i = 0; i < 20; i++) await Promise.resolve()
  await ui.select({ key: 'account', value: 'claude:primevise' })
  release()
  await first
  for (let i = 0; i < 40; i++) await Promise.resolve()
  expect(did.ran.filter(a => a.includes('account')).map(a => a.at(-1))).toEqual(['--choices', 'claude:trip1'])
  expect(did.toasts).toEqual(['Switching to claude:trip1…'])
  expect(did.commands).toEqual(['exit'])
  await ui.unmount()
})

test('a picker closed or reopened while gruntas queues a pick keeps the account and puts back what was queued', async ($, on) => {
  let release: () => void = () => undefined
  const held = new Promise<void>(r => (release = r))
  const did = launched(on, async argv => {
    if (argv.at(-1) === 'claude:trip1') await held
    return `Type /exit to continue on ${argv.at(-1)}.\n`
  })
  // Rollover had queued claude:primevise before the pick.
  CHOICES.queued = 'claude:primevise'
  try {
    await begin($, did)
    await $.command.run({ command: 'gruntas:account', args: '' })
    const ui = await $.ui.mount({ plugin: 'gruntas', surface: 'terminal', component: 'Pane', requestId: PICKER, props: { title: 'Continue this session on', isFocused: true, bodyColumns: 80, placement: 'dock', scroll: { offset: 0, bodyRows: 10 }, view: {} } } as never)
    const first = ui.select({ key: 'account', value: 'claude:trip1' })
    for (let i = 0; i < 20; i++) await Promise.resolve()
    // Asked again while it waits: the pick in flight gives way.
    await $.command.run({ command: 'gruntas:account', args: '' })
    release()
    await first
    for (let i = 0; i < 40; i++) await Promise.resolve()
    expect(did.ran.filter(a => a.includes('account')).map(a => a.at(-1))).toEqual(['--choices', 'claude:trip1', '--choices', 'claude:primevise'])
    expect(did.toasts).toEqual(['Switching to claude:trip1…', 'Kept claude.'])
    expect(did.commands).toEqual([])
    await ui.unmount()
  } finally {
    CHOICES.queued = null
  }
})
