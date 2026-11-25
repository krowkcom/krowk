// PI06: the `gruntas` provider, at its seams. A scripted Claude Code session stands for the
// Agent SDK and a fake gruntas for the accounts, so nothing here needs pi, the SDK or gruntas.
//
//   node --test npm/pi/src/claude/claude.test.ts

import assert from 'node:assert/strict'
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import { Accounts, agentDir } from './accounts.ts'
import type { Gruntas, Listed } from './accounts.ts'
import { installClaude, PROVIDER } from './extension.ts'
import { childEnv, launcherArgv, launcherEnv, spawnOnAccount } from './launch.ts'
import { ClaudeSdkProvider } from './provider.ts'
import { synthesizeSession } from './synth.ts'
import type { ClaudeSession, SessionSpec } from './session.ts'
import { AsyncQueue } from './util.ts'

const here = dirname(fileURLToPath(import.meta.url))

function scratch(): string {
  return mkdtempSync(join(tmpdir(), 'gruntas-pi-claude-'))
}

// --- accounts ----------------------------------------------------------------

const ROWS: Listed[] = [
  { instance: 'claude', kind: 'claude-code', ready: true, config_dir: '/h/claude' },
  { instance: 'claude:work', kind: 'claude-code', ready: true, config_dir: '/h/work' },
  { instance: 'claude:out', kind: 'claude-code', ready: false, config_dir: '/h/out' },
  { instance: 'codex', kind: 'codex-app-server', ready: true, config_dir: '/h/codex' },
]

function fakeGruntas(auto: string | undefined, rows = ROWS) {
  const calls: string[] = []
  const g: Gruntas = {
    list: async () => (calls.push('list'), rows),
    auto: async () => (calls.push('auto'), auto),
    next: async () => undefined,
  }
  return { g, calls }
}

function accountsIn(dir: string, auto: string | undefined, last?: string) {
  const file = join(dir, 'gruntas', 'account.json')
  if (last !== undefined) {
    mkdirSync(dirname(file), { recursive: true })
    writeFileSync(file, JSON.stringify({ last }))
  }
  const { g, calls } = fakeGruntas(auto)
  return { accounts: new Accounts(g, file), calls, file }
}

test('a new session starts on the account it used last, and keeps it as last', async () => {
  const dir = scratch()
  try {
    const { accounts, calls, file } = accountsIn(dir, 'claude', 'claude:work')
    assert.deepEqual(await accounts.current(), { name: 'claude:work', home: '/h/work' })
    assert.deepEqual(calls, ['list'], 'rollover\'s ranking is not asked')
    assert.equal(JSON.parse(readFileSync(file, 'utf8')).last, 'claude:work')
    // Settled once: the next request asks nothing.
    await accounts.current()
    assert.deepEqual(calls, ['list'])
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('with no last account, or one no longer ready or usable, it starts on rollover\'s first ready account', async () => {
  for (const last of [undefined, 'claude:out', 'codex', 'claude:gone']) {
    const dir = scratch()
    try {
      const { accounts, calls, file } = accountsIn(dir, 'claude:work', last)
      assert.equal((await accounts.current()).name, 'claude:work', String(last))
      assert.deepEqual(calls, ['list', 'auto'])
      assert.equal(JSON.parse(readFileSync(file, 'utf8')).last, 'claude:work')
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  }
})

test('with no ready account, the request fails saying so', async () => {
  const dir = scratch()
  try {
    const { accounts } = accountsIn(dir, undefined)
    await assert.rejects(accounts.current(), /no Claude Code account is ready/)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('the last account is kept in pi\'s config home', () => {
  assert.equal(agentDir({ PI_CODING_AGENT_DIR: '/p' }), '/p')
  assert.match(agentDir({}), /\.pi\/agent$/)
})

test('a move is refused for an unknown account or a Codex one', async () => {
  const dir = scratch()
  try {
    const { accounts } = accountsIn(dir, 'claude')
    await assert.rejects(accounts.moveTo('nope'), /no account named nope/)
    await assert.rejects(accounts.moveTo('codex'), /not usable from pi yet/)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

// --- the provider, on a scripted Claude Code --------------------------------

/** A Claude Code session that answers each user turn with the next scripted response. */
class FakeSession implements ClaudeSession {
  readonly spec: SessionSpec
  readonly sessionId = `s-${Math.random().toString(36).slice(2)}`
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
  readonly sent: any[][] = []
  readonly delivered: any[] = []
  closed = false
  private readonly script: any[][]

  constructor(spec: SessionSpec, chain: string, count: number, script: any[][]) {
    this.spec = spec
    this.systemPrompt = spec.systemPrompt
    this.model = spec.model
    this.chain = chain
    this.count = count
    this.script = script
  }

  private respond() {
    for (const m of this.script.shift() ?? []) this.events.push(m)
  }
  send(content: any[]) {
    this.sent.push(content)
    this.respond()
  }
  deliver(result: any) {
    this.delivered.push(result)
    this.respond()
  }
  async entryUuid() {
    return undefined
  }
  async setModel(model: string) {
    this.model = model
  }
  async interrupt() {}
  close() {
    this.closed = true
    this.dead = true
  }
}

const se = (event: any) => ({ type: 'stream_event', parent_tool_use_id: null, event })
const TOOL_TURN = [
  se({ type: 'message_start', message: { id: 'msg_1', model: 'claude-haiku-4-5', usage: { input_tokens: 10 } } }),
  se({ type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: 'toolu_1', name: 'mcp__pi__bash' } }),
  se({ type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: '{"command":"ls"}' } }),
  se({ type: 'content_block_stop', index: 0 }),
  se({ type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 5 } }),
  se({ type: 'message_stop' }),
]
const TEXT_TURN = (text: string) => [
  se({ type: 'message_start', message: { id: 'msg_2', model: 'claude-haiku-4-5', usage: { input_tokens: 20 } } }),
  se({ type: 'content_block_start', index: 0, content_block: { type: 'text' } }),
  se({ type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } }),
  se({ type: 'content_block_stop', index: 0 }),
  se({ type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 3 } }),
  se({ type: 'message_stop' }),
  { type: 'result', subtype: 'success', is_error: false },
]

/** pi-ai's event stream, as far as the provider uses it: the events, and the end. */
function fakeStream() {
  const events: any[] = []
  let end!: () => void
  const ended = new Promise<void>(r => (end = r))
  return { events, ended, push: (e: any) => events.push(e), end: () => end() }
}

const MODEL = { id: 'claude-haiku-4-5', api: 'gruntas-claude-code', provider: PROVIDER, maxTokens: 1000 } as any
const PROMPT = 'You are pi. Be brief.'
const TOOL = { name: 'bash', description: 'Run a command', parameters: { type: 'object', properties: { command: { type: 'string' } } } }
const system = { role: 'system', content: PROMPT, toolsAdded: [TOOL], timestamp: 0 }
const user = (text: string) => ({ role: 'user', content: text, timestamp: 1 })

function harness(current: () => { name: string; home?: string }, scripts: any[][][]) {
  const sessions: FakeSession[] = []
  const provider = new ClaudeSdkProvider({
    account: async () => current(),
    cwd: () => '/work/repo',
    createSession: (spec, chain, count) => {
      const s = new FakeSession(spec, chain, count, scripts.shift() ?? [])
      sessions.push(s)
      return s
    },
    ai: { createAssistantMessageEventStream: fakeStream as never, calculateCost: () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 }) },
  })
  const turn = async (messages: any[]) => {
    const stream = provider.streamSimple(MODEL, { messages } as never) as unknown as ReturnType<typeof fakeStream>
    await stream.ended
    const last = stream.events[stream.events.length - 1]
    assert.equal(last.type, 'done', JSON.stringify(last))
    return last.message
  }
  return { provider, sessions, turn }
}

test('a turn with a tool call hands the call to pi, and pi\'s result goes back to the same Claude Code', async () => {
  const h = harness(() => ({ name: 'claude:work', home: '/h/work' }), [[TOOL_TURN, TEXT_TURN('done')]])
  const first = await h.turn([system, user('list files')])
  assert.equal(first.stopReason, 'toolUse')
  assert.deepEqual(first.content, [{ type: 'toolCall', id: 'toolu_1', name: 'bash', arguments: { command: 'ls' } }])
  const [s] = h.sessions
  assert.equal(s!.spec.account.name, 'claude:work')
  // pi's prompt is context in the first user turn, never Claude Code's system prompt.
  assert.ok(JSON.stringify(s!.sent[0]).includes(PROMPT))
  assert.ok(JSON.stringify(s!.sent[0]).includes('list files'))
  // pi ran the tool; its result goes to the parked call.
  const result = { role: 'toolResult', toolCallId: 'toolu_1', toolName: 'bash', content: [{ type: 'text', text: 'a.txt' }], isError: false, timestamp: 2 }
  const second = await h.turn([system, user('list files'), first, result])
  assert.equal(second.stopReason, 'stop')
  assert.deepEqual(second.content, [{ type: 'text', text: 'done' }])
  assert.equal(h.sessions.length, 1, 'one Claude Code process for the conversation')
  assert.deepEqual(s!.delivered.map(r => r.toolCallId), ['toolu_1'])
})

test('a move mid-conversation runs the next request on the new account with the conversation pi holds', async () => {
  const home = scratch()
  try {
    const dir = scratch()
    const { g } = fakeGruntas('claude', [
      { instance: 'claude', kind: 'claude-code', ready: true, config_dir: join(home, 'a') },
      { instance: 'claude:b', kind: 'claude-code', ready: true, config_dir: join(home, 'b') },
    ])
    const accounts = new Accounts(g, join(dir, 'account.json'))
    let provider!: ClaudeSdkProvider
    const sessions: FakeSession[] = []
    const scripts = [[TOOL_TURN], [TEXT_TURN('on b')]]
    const registered: any[] = []
    const fakePi = {
      registerProvider: (name: string, config: any) => registered.push({ name, config }),
      on: () => {},
      registerCommand: () => {},
    }
    ;({ provider } = installClaude(fakePi as never, {
      createSession: (spec, chain, count) => {
        const s = new FakeSession(spec, chain, count, scripts.shift() ?? [])
        sessions.push(s)
        return s
      },
      ai: { createAssistantMessageEventStream: fakeStream as never, calculateCost: () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 }) },
    }, accounts))
    assert.equal(registered[0].name, 'gruntas')
    assert.ok(registered[0].config.models.length > 0)
    const turn = async (messages: any[]) => {
      const stream = registered[0].config.streamSimple(MODEL, { messages }) as ReturnType<typeof fakeStream>
      await stream.ended
      return stream.events[stream.events.length - 1]
    }
    const first = (await turn([system, user('list files')])).message
    assert.equal(sessions[0]!.spec.account.name, 'claude')

    assert.deepEqual(await accounts.moveTo('claude:b'), { name: 'claude:b', home: join(home, 'b') })
    assert.equal(accounts.name, 'claude:b')
    assert.ok(sessions[0]!.closed, 'the old account\'s Claude Code is closed')
    assert.equal(JSON.parse(readFileSync(join(dir, 'account.json'), 'utf8')).last, 'claude:b')

    const result = { role: 'toolResult', toolCallId: 'toolu_1', toolName: 'bash', content: [{ type: 'text', text: 'a.txt' }], isError: false, timestamp: 2 }
    const done = await turn([system, user('list files'), first, result])
    assert.equal(done.type, 'done', JSON.stringify(done))
    assert.deepEqual(done.message.content, [{ type: 'text', text: 'on b' }])
    const b = sessions[1]!
    assert.equal(b.spec.account.name, 'claude:b')
    // Resumed from the conversation, written under b's home: pi's context, the user turn and the tool call.
    assert.ok(b.spec.resume?.sessionId)
    const files = readdirSync(join(home, 'b', 'projects'), { recursive: true }).map(String).filter(f => f.endsWith('.jsonl'))
    assert.equal(files.length, 1)
    const written = readFileSync(join(home, 'b', 'projects', files[0]!), 'utf8')
    assert.ok(written.includes(b.spec.resume!.sessionId))
    for (const said of [PROMPT, 'list files', 'mcp__pi__bash']) assert.ok(written.includes(said), said)
    assert.equal(existsSync(join(home, 'a')), false, 'nothing is written under the old account')
    // The tool result goes in as the new session's first turn.
    assert.ok(JSON.stringify(b.sent[0]).includes('a.txt'))
    provider.shutdown()
    rmSync(dir, { recursive: true, force: true })
  } finally {
    rmSync(home, { recursive: true, force: true })
  }
})

// --- no credential reaches pi ------------------------------------------------

test('Claude Code is started through gruntas run on the account, never as pi\'s own claude', () => {
  assert.deepEqual(launcherArgv('claude:work', ['--output-format', 'stream-json']), ['gruntas', 'run', 'claude', '--account', 'claude:work', '--no-plugin', '--output-format', 'stream-json'])
  const env = launcherEnv(childEnv({ PATH: '/bin', CLAUDECODE: '1', CLAUDE_CODE_ENTRYPOINT: 'cli' }, {}))
  assert.equal(env.CLAUDECODE, undefined)
  assert.equal(env.CLAUDE_CODE_ENTRYPOINT, undefined)
  // The SDK's own marker, which it sets after, stays: Claude Code says the Agent SDK runs it.
  assert.equal(launcherEnv({ CLAUDECODE: '1', CLAUDE_CODE_ENTRYPOINT: 'sdk-ts' }).CLAUDE_CODE_ENTRYPOINT, 'sdk-ts')
  // The extension adds no credential: every variable it sets is one of these.
  assert.deepEqual(Object.keys(env).filter(k => k !== 'PATH').sort(), [
    'CLAUDE_AGENT_SDK_CLIENT_APP',
    'CLAUDE_CODE_DISABLE_AUTO_MEMORY',
    'CLAUDE_CODE_DISABLE_CLAUDE_MDS',
    'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC',
    'DISABLE_AUTO_COMPACT',
    'DISABLE_COMPACT',
    'MAX_MCP_OUTPUT_TOKENS',
    'MCP_TOOL_TIMEOUT',
  ])
})

test('the launch runs gruntas with the SDK\'s arguments, cwd and environment, less the nesting markers', async () => {
  const dir = scratch()
  try {
    const bin = join(dir, 'gruntas')
    writeFileSync(bin, '#!/bin/sh\nprintf "%s\\n" "$*"; pwd; echo "nested=${CLAUDECODE:-no} pi=$PI_MARK"; echo oops >&2\n')
    chmodSync(bin, 0o755)
    let err = ''
    const child = spawnOnAccount('claude:work', { command: '/sdk/claude', args: ['--print'], cwd: dir, env: { PATH: `${dir}:/usr/bin:/bin`, CLAUDECODE: '1', PI_MARK: 'x' } }, d => (err += d))
    let out = ''
    child.stdout.setEncoding('utf8').on('data', (d: string) => (out += d))
    child.stdin.end()
    await new Promise(r => child.on('close', r))
    assert.deepEqual(out.trim().split('\n'), ['run claude --account claude:work --no-plugin --print', realpathSync(dir), 'nested=no pi=x'])
    assert.equal(err, 'oops\n')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

/** Every file at or under `at`. */
function files(at: string): string[] {
  return statSync(at).isDirectory() ? readdirSync(at).flatMap(n => files(join(at, n))) : [at]
}

test('the extension\'s code reads no login, key or pi credential store', () => {
  const src = files(join(here, '..')).filter(f => f.endsWith('.ts') && !f.endsWith('.test.ts'))
  assert.ok(src.length > 5)
  for (const f of src) {
    const text = readFileSync(f, 'utf8')
    for (const bad of [/\.credentials\.json/, /auth\.json/, /ANTHROPIC_API_KEY/, /ANTHROPIC_AUTH_TOKEN/, /CLAUDE_CODE_OAUTH_TOKEN/, /oauth\s*:/i, /keychain/i, /authHeader/, /user-agent/i]) {
      assert.doesNotMatch(text, bad, `${f}: ${bad}`)
    }
  }
})

// --- review round 1 ----------------------------------------------------------

test('a conversation written for another account keeps the trailing tool loop\'s thinking of this provider\'s turns', () => {
  const home = scratch()
  try {
    const turn = { role: 'assistant', api: 'gruntas-claude-code', provider: PROVIDER, model: MODEL.id, stopReason: 'toolUse', timestamp: 1, usage: {}, content: [{ type: 'thinking', thinking: 'let me look', thinkingSignature: 'sig-1' }, { type: 'toolCall', id: 'toolu_1', name: 'bash', arguments: { command: 'ls' } }] }
    synthesizeSession(home, '/work/repo', MODEL.id, [user('list files') as never, turn as never])
    const [file] = readdirSync(home, { recursive: true }).map(String).filter(f => f.endsWith('.jsonl'))
    const written = readFileSync(join(home, file!), 'utf8')
    assert.ok(written.includes('"signature":"sig-1"'), written)
    assert.equal(statSync(join(home, file!)).mode & 0o777, 0o600)
  } finally {
    rmSync(home, { recursive: true, force: true })
  }
})

test('a move while a request runs on the old account closes its Claude Code once the request ends', async () => {
  const h = harness(() => ({ name: 'claude:a', home: '/h/a' }), [[]])
  const stream = h.provider.streamSimple(MODEL, { messages: [system, user('list files')] } as never) as unknown as ReturnType<typeof fakeStream>
  for (let i = 0; i < 20 && h.sessions.length === 0; i++) await new Promise(r => setTimeout(r, 1))
  const [s] = h.sessions
  h.provider.retire('claude:b')
  assert.equal(s!.closed, false, 'not while its request runs')
  for (const m of TOOL_TURN) s!.events.push(m)
  await stream.ended
  assert.equal(s!.closed, true)
})

test('a move made while the session is still settling its start wins', async () => {
  const dir = scratch()
  try {
    let release!: () => void
    const held = new Promise<void>(r => (release = r))
    const g: Gruntas = { list: async () => ROWS, auto: async () => (await held, 'claude'), next: async () => undefined }
    const accounts = new Accounts(g, join(dir, 'account.json'))
    const starting = accounts.current()
    await accounts.moveTo('claude:work')
    release()
    assert.equal((await starting).name, 'claude:work')
    assert.equal(JSON.parse(readFileSync(join(dir, 'account.json'), 'utf8')).last, 'claude:work')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
