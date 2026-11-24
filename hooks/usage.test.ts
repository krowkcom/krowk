import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code/testing'
import { configHome, payload, POLL_MS } from './usage'

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
