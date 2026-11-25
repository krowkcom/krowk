// Rollover in pi (R-PI-8, D9, D10): usage readings to gruntas's usage record, and a limit hit
// under offer, auto, off and with no UI, simulated: no real account is spent.

import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import { Accounts, type Gruntas, type Listed } from './accounts.ts'
import { installClaude, rolloverUi } from './extension.ts'
import { configHome, limitHit, OFFER_TIMEOUT_MS, Rollover, usagePayload, usageWindows, when, type Next, type Ui } from './rollover.ts'
import type { ClaudeSession, SessionSpec } from './session.ts'
import { AsyncQueue } from './util.ts'

const here = dirname(fileURLToPath(import.meta.url))
const FIXTURE = join(here, '../../../../crates/gruntas-harness/tests/fixtures/hooks/pi/usage.json')

// Claude Code's own readings, as gruntas's Claude Code fixtures hold them.
const REJECTED = { status: 'rejected', resetsAt: 1790431200, rateLimitType: 'five_hour', utilization: 1.0 }
const UNIFIED = { status: 'rejected', resetsAt: 1791846000, rateLimitType: 'seven_day', unifiedWindows: { five_hour: { utilization: 0.9, resetsAt: 1791628200 }, seven_day: { utilization: 1.0, resetsAt: 1791846000 } } }
const WARNING = { status: 'allowed_warning', resetsAt: 4102444800, rateLimitType: 'five_hour', unifiedWindows: { five_hour: { utilization: 0.96, resetsAt: 4102444800 }, seven_day: { utilization: 0.3, resetsAt: 4102444800 } } }

// --- usage readings ------------------------------------------------------------

test('a reading becomes the usage payload the hook takes, under the account\'s config home', () => {
  const want = JSON.parse(readFileSync(FIXTURE, 'utf8'))
  const payload = JSON.parse(usagePayload({ name: 'claude:work', home: '/h/work' }, WARNING)!)
  assert.deepEqual(Object.keys(payload).sort(), Object.keys(want).sort())
  assert.deepEqual(Object.keys(payload.windows[0]).sort(), Object.keys(want.windows[0]).sort())
  assert.equal(payload.configHome, '/h/work')
  assert.deepEqual(payload.windows, [
    { kind: 'fiveHour', usedPercent: 96, resetsAtMs: 4102444800000 },
    { kind: 'sevenDay', usedPercent: 30, resetsAtMs: 4102444800000 },
  ])
  // A rejected window with no figure of its own is spent.
  assert.deepEqual(usageWindows({ status: 'rejected', resetsAt: 10, rateLimitType: 'five_hour' }), [{ kind: 'fiveHour', usedPercent: 100, resetsAtMs: 10_000 }])
  assert.deepEqual(usageWindows(UNIFIED).map(w => [w.kind, Math.round(w.usedPercent)]), [['fiveHour', 90], ['sevenDay', 100]])
  // Nor one whose figure says less: rejected is spent.
  assert.equal(usageWindows({ status: 'rejected', resetsAt: 10, rateLimitType: 'five_hour', utilization: 0.97 })[0]!.usedPercent, 100)
  // Nothing to record: no payload.
  assert.equal(usagePayload({ name: 'a' }, { status: 'allowed', rateLimitType: 'five_hour' }), undefined)
  assert.equal(usagePayload({ name: 'a' }, null), undefined)
  // The default account with no home given: Claude Code's own.
  assert.equal(configHome({ name: 'claude' }, { CLAUDE_CONFIG_DIR: '/c' }), '/c')
  assert.match(configHome({ name: 'claude' }, {}), /\.claude$/)
})

test('only a rejected reading is a limit hit, with when it resets', () => {
  assert.deepEqual(limitHit(REJECTED), { resetsAt: 1790431200000 })
  assert.equal(limitHit(WARNING), undefined)
  // The named window's reset, when the reading has none at the top.
  assert.deepEqual(limitHit({ status: 'rejected', rateLimitType: 'seven_day', unifiedWindows: UNIFIED.unifiedWindows }), { resetsAt: 1791846000000 })
  assert.equal(limitHit(undefined), undefined)
  const now = new Date(2026, 10, 25, 9, 0).getTime()
  assert.equal(when(new Date(2026, 10, 25, 14, 5).getTime(), now), '14:05')
  assert.match(when(new Date(2026, 10, 27, 8, 0).getTime(), now), /Nov 27 08:00$/)
})

// --- a limit hit ---------------------------------------------------------------

const RESET = new Date(2026, 10, 25, 14, 30).getTime()
const NOW = new Date(2026, 10, 25, 12, 0).getTime()

function fakeUi(hasUI: boolean, answer: boolean | 'never') {
  const asked: { title: string; message: string; timeout: number }[] = []
  const told: [string, string][] = []
  const ui: Ui = {
    hasUI,
    confirm: (title, message, opts) => {
      asked.push({ title, message, timeout: opts.timeout })
      // A host that never answers: the dialog's own timeout (pi resolves false) is what ends it.
      return answer === 'never' ? new Promise(r => setTimeout(() => r(false), 5)) : Promise.resolve(answer)
    },
    notify: (m, level) => told.push([m, level]),
  }
  return { ui, asked, told }
}

function rig(next: Next | undefined, hasUI: boolean, answer: boolean | 'never' = true) {
  const moves: string[] = []
  const accounts = { name: 'claude:a' as string | undefined, moveTo: async (n: string) => (moves.push(n), (accounts.name = n)) }
  const asks: [string, string[]][] = []
  const { ui, asked, told } = fakeUi(hasUI, answer)
  const r = new Rollover(accounts, async (from, tried) => (asks.push([from, tried]), next), () => ui, () => NOW)
  return { r, accounts, moves, asks, asked, told }
}

test('offer asks, with a timeout, and moves on yes', async () => {
  const t = rig({ next: 'claude:b', mode: 'offer', resetsAt: null }, true, true)
  await t.r.hit('claude:a', RESET)
  assert.equal(t.asked.length, 1)
  assert.equal(t.asked[0]!.title, 'claude:a is out of usage until 14:30. Continue on claude:b?')
  assert.equal(t.asked[0]!.timeout, OFFER_TIMEOUT_MS)
  assert.ok(OFFER_TIMEOUT_MS > 0)
  assert.deepEqual(t.moves, ['claude:b'])
  assert.deepEqual(t.asks, [['claude:a', []]])
})

test('offer declined, or never answered, stays', async () => {
  for (const answer of [false, 'never'] as const) {
    const t = rig({ next: 'claude:b', mode: 'offer', resetsAt: null }, true, answer)
    await t.r.hit('claude:a', RESET)
    assert.equal(t.asked.length, 1, String(answer))
    assert.deepEqual(t.moves, [], String(answer))
  }
})

test('auto moves and says so', async () => {
  const t = rig({ next: 'claude:b', mode: 'auto', resetsAt: null }, true)
  await t.r.hit('claude:a', RESET)
  assert.deepEqual(t.asked, [], 'no dialog')
  assert.deepEqual(t.moves, ['claude:b'])
  assert.deepEqual(t.told, [['Continued on claude:b: claude:a is out of usage until 14:30.', 'info']])
})

test('with no UI, offer names the next account and the reset, and stays; auto still moves', async () => {
  const t = rig({ next: 'claude:b', mode: 'offer', resetsAt: RESET }, false)
  await t.r.hit('claude:a')
  assert.deepEqual(t.asked, [], 'no dialog where there is no UI')
  assert.deepEqual(t.moves, [])
  assert.deepEqual(t.told, [['claude:a is out of usage until 14:30. claude:b has room; rollover is offer, so this session stays on claude:a.', 'warning']])
  const a = rig({ next: 'claude:b', mode: 'auto', resetsAt: RESET }, false)
  await a.r.hit('claude:a')
  assert.deepEqual(a.moves, ['claude:b'])
})

test('off, no answer from gruntas, or none left: no move', async () => {
  const off = rig({ next: 'claude:b', mode: 'off', resetsAt: null }, true)
  await off.r.hit('claude:a', RESET)
  assert.deepEqual([off.moves, off.asked, off.told], [[], [], []])
  const silent = rig(undefined, true)
  await silent.r.hit('claude:a', RESET)
  assert.deepEqual([silent.moves, silent.told], [[], []])
  const none = rig({ next: null, mode: 'auto', resetsAt: null }, true)
  await none.r.hit('claude:a', RESET)
  assert.deepEqual(none.moves, [])
  assert.deepEqual(none.told, [['claude:a is out of usage until 14:30, and no other subscription account has room.', 'warning']])
})

test('a spent account is tried once: the next hit skips it, and a repeat or old reading changes nothing', async () => {
  const t = rig({ next: 'claude:b', mode: 'auto', resetsAt: null }, true)
  await Promise.all([t.r.hit('claude:a', RESET), t.r.hit('claude:a', RESET)])
  assert.deepEqual(t.moves, ['claude:b'], 'one move for a reading said twice')
  await t.r.hit('claude:a', RESET)
  assert.deepEqual(t.moves, ['claude:b'], 'a reading of the account it left')
  await t.r.hit('claude:b', RESET)
  assert.deepEqual(t.asks.at(-1), ['claude:b', ['claude:a']])
})

test('a spent account is passed over until it resets, five hours when nobody said', async () => {
  let now = NOW
  const accounts = { name: 'claude:a' as string | undefined, moveTo: async (n: string) => (accounts.name = n) }
  const asks: string[][] = []
  const r = new Rollover(accounts, async (_from, tried) => (asks.push(tried), { next: accounts.name === 'claude:a' ? 'claude:b' : 'claude:a', mode: 'auto', resetsAt: null }), () => fakeUi(true, true).ui, () => now)
  await r.hit('claude:a')
  now += 60_000
  await r.hit('claude:b', NOW + 3_600_000)
  assert.deepEqual(asks.at(-1), ['claude:a'], 'a is still out')
  now = NOW + 5 * 3_600_000 - 1
  accounts.name = 'claude:c'
  await r.hit('claude:c')
  assert.deepEqual(asks.at(-1), ['claude:a'], 'a is out until five hours have passed')
  now = NOW + 5 * 3_600_000 + 1
  accounts.name = 'claude:d'
  await r.hit('claude:d')
  assert.deepEqual(asks.at(-1), ['claude:c'], 'a and b have reset')
})

test('a reset gruntas knows, where the reading said none, is how long the account is passed over', async () => {
  let now = NOW
  const accounts = { name: 'claude:a' as string | undefined, moveTo: async (n: string) => (accounts.name = n) }
  const asks: string[][] = []
  const r = new Rollover(accounts, async (from, tried) => (asks.push(tried), { next: 'claude:x', mode: 'off', resetsAt: from === 'claude:a' ? NOW + 3_600_000 : null }), () => undefined, () => now)
  await r.hit('claude:a')
  now = NOW + 3_600_000 - 1
  accounts.name = 'claude:b'
  await r.hit('claude:b', NOW + 9 * 3_600_000)
  assert.deepEqual(asks.at(-1), ['claude:a'], 'until gruntas\'s reset')
  now = NOW + 3_600_000 + 1
  accounts.name = 'claude:c'
  await r.hit('claude:c', NOW + 9 * 3_600_000)
  assert.deepEqual(asks.at(-1), ['claude:b'], 'a dropped at gruntas\'s reset, not five hours')
})

test('a request sent while a hit is handled waits on it, so it goes where rollover moved the session', async () => {
  let answer!: (n: Next) => void
  const accounts = { name: 'claude:a' as string | undefined, moveTo: async (n: string) => (accounts.name = n) }
  const r = new Rollover(accounts, () => new Promise<Next>(res => (answer = res)), () => fakeUi(true, true).ui, () => NOW)
  await r.settled()
  void r.hit('claude:a', RESET)
  let where: string | undefined
  const request = r.settled().then(() => (where = accounts.name))
  await new Promise(res => setTimeout(res, 5))
  assert.equal(where, undefined, 'still waiting on gruntas')
  answer({ next: 'claude:b', mode: 'auto', resetsAt: null })
  await request
  assert.equal(where, 'claude:b')
  // A handling that fails still lets the request go.
  const failing = new Rollover(accounts, async () => ({ next: 'claude:c', mode: 'auto', resetsAt: null }), () => fakeUi(true, true).ui, () => NOW)
  accounts.moveTo = async () => Promise.reject(new Error('gone'))
  accounts.name = 'claude:b'
  void failing.hit('claude:b', RESET)
  await failing.settled()
})

test('a context pi has retired (print mode ends the session with the turn): the notice goes to stderr, an offer is declined', async () => {
  // Built while the context was live, used once pi has retired it, as pi's own ctx throws.
  let live = true
  const stale = () => {
    throw new Error('This extension ctx is stale')
  }
  const ctx = {
    get hasUI(): boolean {
      return live ? false : stale()
    },
    get ui(): any {
      return stale()
    },
  }
  const ui = rolloverUi(ctx)
  live = false
  const written: string[] = []
  const write = process.stderr.write
  process.stderr.write = ((s: string) => (written.push(s), true)) as never
  try {
    ui.notify('claude:a is out of usage.', 'warning')
  } finally {
    process.stderr.write = write
  }
  assert.deepEqual(written, ['gruntas: claude:a is out of usage.\n'])
  await assert.rejects(ui.confirm('t', 'm', { timeout: 1 }))
  // Rollover takes a refused dialog as no.
  const moves: string[] = []
  const r = new Rollover({ name: 'claude:a', moveTo: async n => moves.push(n) }, async () => ({ next: 'claude:b', mode: 'offer', resetsAt: null }), () => ({ ...ui, hasUI: true }), () => NOW)
  await r.hit('claude:a', RESET)
  assert.deepEqual(moves, [])
})

// --- wired into the provider ---------------------------------------------------

class FakeSession implements ClaudeSession {
  readonly spec: SessionSpec
  readonly sessionId = 's-1'
  readonly systemPrompt: string
  model: string
  chain: string
  count: number
  awaiting = new Set<string>()
  servedByClaude = new Set<string>()
  dead = false
  busy = false
  lastUsed = Date.now()
  readonly events = new AsyncQueue<any>()
  readonly exited = Promise.resolve()
  readonly stderr = ''
  private readonly script: any[]
  constructor(spec: SessionSpec, chain: string, count: number, script: any[]) {
    this.spec = spec
    this.systemPrompt = spec.systemPrompt
    this.model = spec.model
    this.chain = chain
    this.count = count
    this.script = script
  }
  send() {
    for (const m of this.script) this.events.push(m)
  }
  deliver() {}
  async entryUuid() {
    return undefined
  }
  async setModel() {}
  async interrupt() {}
  close() {
    this.dead = true
  }
}

test('a limit in a turn records the usage under the account\'s home and rolls the session over', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'gruntas-pi-rollover-'))
  try {
    const rows: Listed[] = [
      { instance: 'claude:a', kind: 'claude-code', ready: true, config_dir: '/h/a' },
      { instance: 'claude:b', kind: 'claude-code', ready: true, config_dir: '/h/b' },
    ]
    const asked: [string, string[]][] = []
    const g: Gruntas = { list: async () => rows, auto: async () => 'claude:a', next: async (from, tried) => (asked.push([from, tried]), { next: 'claude:b', mode: 'auto', resetsAt: null }) }
    const accounts = new Accounts(g, join(dir, 'account.json'))
    const handlers = new Map<string, (e: unknown, ctx: unknown) => void>()
    const registered: any[] = []
    const fakePi = { registerProvider: (_n: string, config: any) => registered.push(config), on: (e: string, f: any) => handlers.set(e, f), registerCommand: () => {} }
    const sent: string[] = []
    const script = [
      { type: 'rate_limit_event', rate_limit_info: REJECTED },
      { type: 'result', subtype: 'error_during_execution', is_error: true, errors: ['You\'ve hit your limit'] },
    ]
    installClaude(fakePi as never, {
      createSession: (spec, chain, count) => new FakeSession(spec, chain, count, script),
      ai: {
        createAssistantMessageEventStream: (() => {
          let end!: () => void
          const ended = new Promise<void>(r => (end = r))
          return { push: () => {}, end: () => end(), ended }
        }) as never,
        calculateCost: () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 }),
      },
    }, accounts, { usage: p => sent.push(p) })
    const told: string[] = []
    handlers.get('agent_start')!({}, { hasUI: true, ui: { notify: (m: string) => told.push(m), confirm: async () => true } })
    const stream = registered[0].streamSimple({ id: 'claude-haiku-4-5', api: 'gruntas-claude-code', provider: 'gruntas', maxTokens: 1000 }, { messages: [{ role: 'user', content: 'hi', timestamp: 1 }] })
    await stream.ended
    for (let i = 0; i < 50 && accounts.name !== 'claude:b'; i++) await new Promise(r => setTimeout(r, 1))
    assert.deepEqual(sent.map(p => JSON.parse(p)), [{ configHome: '/h/a', windows: [{ kind: 'fiveHour', usedPercent: 100, resetsAtMs: 1790431200000 }] }])
    assert.deepEqual(asked, [['claude:a', []]])
    assert.equal(accounts.name, 'claude:b')
    assert.equal(told.length, 1)
    assert.match(told[0]!, /^Continued on claude:b: claude:a is out of usage until /)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
