// PI07: /account and the footer, on a fake pi and a fake gruntas.
//
//   node --test npm/pi/src/claude/picker.test.ts

import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { Accounts, type Gruntas, type Listed } from './accounts.ts'
import { footer, installPicker, line, PICK_TIMEOUT_MS, STATUS_KEY } from './picker.ts'

const ROWS: Listed[] = [
  {
    instance: 'claude',
    kind: 'claude-code',
    label: 'Claude subscription',
    state: 'ready',
    ready: true,
    config_dir: '/h/claude',
    usage: [
      { window: 'five_hour', left: 93, resetsAtMs: 1 },
      { window: 'seven_day', left: 61.4, resetsAtMs: 2 },
    ],
  },
  { instance: 'claude:work', kind: 'claude-code', label: 'Claude subscription', state: 'ready', ready: true, config_dir: '/h/work', usage: [] },
  { instance: 'claude:out', kind: 'claude-code', label: 'Claude subscription', state: 'not_signed_in', ready: false, fix: 'gruntas login claude:out', config_dir: '/h/out' },
  { instance: 'codex', kind: 'codex-app-server', label: 'ChatGPT subscription', state: 'ready', ready: true, config_dir: '/h/codex' },
]

/** A pi with one session: its /account command, its events, and what its UI was asked. */
function setup(opts: { hasUI?: boolean; pick?: (title: string, options: string[], o: any) => string | undefined } = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'gruntas-pi-picker-'))
  const g: Gruntas = { list: async () => ROWS, auto: async () => 'claude' }
  const accounts = new Accounts(g, join(dir, 'account.json'))
  const handlers: Record<string, (e: any, ctx: any) => unknown> = {}
  const commands: Record<string, any> = {}
  const pi = {
    on: (event: string, h: any) => void (handlers[event] = h),
    registerCommand: (name: string, c: any) => void (commands[name] = c),
  }
  const status: (string | undefined)[] = []
  const notes: string[] = []
  const asked: any[] = []
  const ctx = {
    hasUI: opts.hasUI ?? true,
    model: { provider: 'gruntas' },
    ui: {
      setStatus: (key: string, text: string | undefined) => (assert.equal(key, STATUS_KEY), status.push(text)),
      notify: (m: string) => notes.push(m),
      select: async (title: string, options: string[], o: any) => (asked.push({ title, options, o }), opts.pick?.(title, options, o)),
    },
  }
  installPicker(pi as never, accounts, 'gruntas')
  const run = (args: string) => commands.account.handler(args, ctx)
  const cleanup = () => rmSync(dir, { recursive: true, force: true })
  return { accounts, handlers, commands, ctx, status, notes, asked, run, cleanup }
}

const tick = () => new Promise(r => setImmediate(r))

test('a row shows name, kind, readiness and usage left per window; Codex rows say pi cannot use them yet', () => {
  assert.equal(line(ROWS[0], 'claude'), '● claude · Claude subscription · ready · left 5h 93%, 7d 61%')
  assert.equal(line(ROWS[1], 'claude'), '  claude:work · Claude subscription · ready')
  assert.equal(line(ROWS[2], undefined), '  claude:out · Claude subscription · not signed in')
  assert.equal(line(ROWS[3], undefined), '  codex · ChatGPT subscription · not usable from pi yet')
})

test('/account lists every account, the current one marked, in a dialog with a timeout', async () => {
  const s = setup({ pick: () => undefined })
  try {
    await s.accounts.current()
    await s.run('')
    assert.equal(s.asked.length, 1)
    assert.deepEqual(s.asked[0].options, ROWS.map(r => line(r, 'claude')))
    assert.equal(s.asked[0].o.timeout, PICK_TIMEOUT_MS)
    assert.equal(s.accounts.name, 'claude')
  } finally {
    s.cleanup()
  }
})

test('choosing an account moves the session to it and the footer follows', async () => {
  const s = setup({ pick: (_t, options) => options[1] })
  try {
    s.handlers.session_start({}, s.ctx)
    await tick()
    assert.deepEqual(s.status, [footer('claude')])
    await s.run('')
    assert.equal(s.accounts.name, 'claude:work')
    assert.equal((await s.accounts.current()).name, 'claude:work')
    assert.deepEqual(s.status, [footer('claude'), footer('claude:work')])
  } finally {
    s.cleanup()
  }
})

test('a Codex row or an account that is not ready cannot be chosen', async () => {
  for (const i of [2, 3]) {
    const s = setup({ pick: (_t, options) => options[i] })
    try {
      await s.accounts.current()
      await s.run('')
      assert.equal(s.accounts.name, 'claude')
      assert.match(s.notes.at(-1)!, i === 3 ? /not usable from pi yet/ : /not ready: gruntas login claude:out/)
    } finally {
      s.cleanup()
    }
  }
})

test('with no UI nothing opens: `/account <name>` moves directly, a bare `/account` prints the list', async () => {
  const s = setup({ hasUI: false })
  const written: string[] = []
  const write = process.stderr.write
  process.stderr.write = ((m: string) => (written.push(m), true)) as never
  try {
    await s.run('claude:work')
    assert.equal(s.accounts.name, 'claude:work')
    await s.run('')
    await s.run('nope')
    assert.equal(s.asked.length, 0)
    assert.match(written.join(''), /claude:work · Claude subscription · ready/)
    assert.match(written.join(''), /no account named nope/)
  } finally {
    process.stderr.write = write
    s.cleanup()
  }
})

test('the footer names the account only while a gruntas model serves the session', async () => {
  const s = setup()
  try {
    s.handlers.session_start({}, { ...s.ctx, model: { provider: 'anthropic' } })
    s.handlers.model_select({ model: { provider: 'gruntas' } }, s.ctx)
    await tick()
    s.handlers.model_select({ model: { provider: 'openai' } }, s.ctx)
    assert.deepEqual(s.status, [undefined, footer('claude'), undefined])
  } finally {
    s.cleanup()
  }
})

test('completions offer the accounts pi can move to', async () => {
  const s = setup()
  try {
    const items = await s.commands.account.getArgumentCompletions('claude:')
    assert.deepEqual(items.map((i: any) => i.value), ['claude:work'])
  } finally {
    s.cleanup()
  }
})
