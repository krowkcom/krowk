// The pi extension at its seam with gruntas: what it sends `gruntas hook pi`,
// and what it does with the answer. A fake pi records handlers and a fake
// host records each hook run, so nothing here needs pi or gruntas.
//
//   node --test npm/pi/src/gruntas.test.ts
//
// The tests skipped with a ticket's name are its acceptance tests, landed
// before it: that ticket un-skips them.

import assert from 'node:assert/strict'
import { chmodSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import gruntas, { INSTALL_HINT, install } from './gruntas.ts'
import { argv, claudeToolName, onPath, run, sessionEnd, sessionStart, stop, toolEvent } from './hook.ts'
import type { Host, Session } from './hook.ts'

const here = dirname(fileURLToPath(import.meta.url))
const pkg = join(here, '..')
const FIXTURES = join(here, '../../../crates/gruntas-harness/tests/fixtures/hooks/pi')

function fixture(event: string): unknown {
  return JSON.parse(readFileSync(join(FIXTURES, `${event}.json`), 'utf8'))
}

// The session the fixtures were written for.
const SESSION: Session = {
  id: '0198b2f4-3c6e-7a10-8d2e-5f4a9c1b7e63',
  file: '/home/user/.pi/agent/sessions/--home-user-repo--/2026-11-25T14-00-00-000Z_0198b2f4-3c6e-7a10-8d2e-5f4a9c1b7e63.jsonl',
  cwd: '/home/user/repo',
}
const CALL = { toolCallId: 'toolu_01PiBashCall7Hq2', toolName: 'bash', input: { command: 'cargo test' } }

type Handler = (event: any, ctx: any) => unknown

/** pi, as far as the extension sees it: handlers by event, and what it registered. */
function fakePi() {
  const handlers = new Map<string, Handler[]>()
  const mcp: { name: string; config: any }[] = []
  const api = {
    on(event: string, h: Handler) {
      handlers.set(event, [...(handlers.get(event) ?? []), h])
    },
    registerMcpServer(name: string, config: unknown) {
      mcp.push({ name, config })
    },
  }
  const notes: { message: string; level?: string }[] = []
  const ctx = {
    cwd: SESSION.cwd,
    hasUI: true,
    mode: 'tui',
    ui: { notify: (message: string, level?: string) => notes.push({ message, level }) },
    sessionManager: { getSessionId: () => SESSION.id, getSessionFile: () => SESSION.file, getCwd: () => SESSION.cwd },
  }
  /** Every handler of `event`, in order; their answers. */
  const fire = async (event: string, payload: unknown) => {
    const out: unknown[] = []
    for (const h of handlers.get(event) ?? []) out.push(await h(payload, ctx))
    return out
  }
  return { api: api as never, handlers, mcp, notes, ctx, fire }
}

/** The machine, faked: each run is recorded and answered with `answer(event)`. */
function fakeHost(answer: (event: string) => string | undefined = () => '', opts: { onPath?: boolean; platform?: string } = {}) {
  const runs: { argv: readonly string[]; stdin: unknown; limitMs: number }[] = []
  const host: Host = {
    platform: opts.platform ?? 'linux',
    onPath: () => opts.onPath ?? true,
    run: async (command, stdin, limitMs) => {
      runs.push({ argv: command, stdin: JSON.parse(stdin), limitMs })
      return answer(command[3] ?? '')
    },
  }
  return { host, runs }
}

/** Lets fire-and-forget runs happen. */
async function settle() {
  for (let i = 0; i < 20; i++) await Promise.resolve()
}

// --- PI01: the package and its seam ------------------------------------------

test('each payload is the Claude-shaped one gruntas hook pi reads (the Rust fixtures)', () => {
  assert.deepEqual(JSON.parse(sessionStart(SESSION, 'startup')), fixture('SessionStart'))
  assert.deepEqual(JSON.parse(sessionEnd(SESSION, 'quit')), fixture('SessionEnd'))
  for (const event of ['PreToolUse', 'PostToolUse', 'PostToolUseFailure'] as const) {
    assert.deepEqual(JSON.parse(toolEvent(event, SESSION, CALL)), fixture(event), event)
  }
  assert.deepEqual(JSON.parse(stop(SESSION, false)), fixture('Stop'))
  assert.equal(JSON.parse(stop(SESSION, true)).stop_hook_active, true)
  // An ephemeral session has no file.
  assert.equal(JSON.parse(sessionStart({ id: 's', cwd: '/r' }, 'startup')).transcript_path, null)
})

test('pi\'s bash is Claude Code\'s Bash, so the paste guard reads it; other tools keep their names', () => {
  assert.equal(claudeToolName('bash'), 'Bash')
  assert.equal(claudeToolName('read'), 'read')
  assert.equal(claudeToolName('mcp__gruntas__gruntas_report'), 'mcp__gruntas__gruntas_report')
  assert.deepEqual(argv('SessionStart'), ['gruntas', 'hook', 'pi', 'SessionStart'])
})

test('the manifest loads the extension, pins pi, and ships what it loads', () => {
  const manifest = JSON.parse(readFileSync(join(pkg, 'package.json'), 'utf8'))
  assert.equal(manifest.name, '@gruntas/pi')
  assert.ok(manifest.keywords.includes('pi-package'))
  assert.deepEqual(manifest.pi.extensions, ['./src/gruntas.ts'])
  // The pi it is tested against. pi's own packages are peers at "*", as pi
  // asks, and never dependencies, which would shadow pi's copies.
  assert.equal(manifest.engines.pi, '>=1.6.0 <2')
  assert.deepEqual(manifest.peerDependencies, { '@earendil-works/pi-ai': '*', '@earendil-works/pi-coding-agent': '*' })
  // The Agent SDK the gruntas provider runs Claude Code through (PI06) is one.
  assert.ok(Object.keys(manifest.dependencies ?? {}).every(d => !d.startsWith('@earendil-works/')), JSON.stringify(manifest.dependencies))
  for (const e of manifest.pi.extensions) assert.ok(manifest.files.includes(e.replace(/^\.\//, '')), e)
  for (const f of manifest.files) assert.ok(existsSync(join(pkg, f)), f)
  // The MIT notice of the code the gruntas provider is copied from ships with it.
  assert.ok(manifest.files.includes('THIRD-PARTY-NOTICES'))
  assert.match(readFileSync(join(pkg, 'THIRD-PARTY-NOTICES'), 'utf8'), /pi-claude-subscription[\s\S]*Copyright \(c\) 2026 Jarno Keskikangas[\s\S]*Permission is hereby granted/)
})

test('the factory loads on pi\'s API and, on Windows, does nothing', async () => {
  const pi = fakePi()
  gruntas(pi.api)
  const win = fakePi()
  const { host, runs } = fakeHost(undefined, { platform: 'win32' })
  install(win.api, host)
  assert.equal(win.handlers.size, 0)
  assert.equal(win.mcp.length, 0)
  await win.fire('session_start', { reason: 'startup' })
  assert.deepEqual(runs, [])
  assert.deepEqual(win.notes, [])
})

test('run hands the payload to the command and answers its stdout; a missing, failing or slow one is nothing', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'gruntas-pi-'))
  try {
    const script = (name: string, body: string) => {
      writeFileSync(join(dir, name), `#!/bin/sh\n${body}\n`)
      chmodSync(join(dir, name), 0o755)
      return join(dir, name)
    }
    assert.equal(await run([script('echo', 'printf "%s|" "$*"; cat'), 'hook', 'pi', 'Stop'], '{"a":1}', 2000), 'hook pi Stop|{"a":1}')
    assert.equal(await run([script('fail', 'echo no; exit 3')], '{}', 2000), undefined)
    const at = Date.now()
    assert.equal(await run([script('slow', 'sleep 5; echo late')], '{}', 200), undefined)
    assert.ok(Date.now() - at < 1500, `${Date.now() - at} ms`)
    assert.equal(await run([join(dir, 'missing')], '{}', 2000), undefined)
    assert.equal(onPath('echo', `/nowhere:${dir}`), true)
    assert.equal(onPath('missing', dir), false)
    assert.equal(onPath('echo', undefined), false)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

// --- PI02: registration, the end of a run, no gruntas -------------------------

test('PI02: session_start runs SessionStart and session_shutdown SessionEnd, neither awaited', { timeout: 5000 }, async () => {
  const pi = fakePi()
  let release = () => {}
  const held = new Promise<void>(r => (release = r))
  const { host, runs } = fakeHost()
  install(pi.api, { ...host, run: async (...a) => (await host.run(...a), await held, '') })
  await pi.fire('session_start', { reason: 'startup' })
  await pi.fire('session_shutdown', { reason: 'quit' })
  release()
  assert.deepEqual(runs.map(r => r.argv), [argv('SessionStart'), argv('SessionEnd')])
  assert.deepEqual(runs[0]?.stdin, fixture('SessionStart'))
  assert.deepEqual(runs[1]?.stdin, fixture('SessionEnd'))
})

test('PI02: a reload keeps the session registered: no SessionEnd, whose late deregistration would undo the new start', async () => {
  const pi = fakePi()
  const { host, runs } = fakeHost()
  install(pi.api, host)
  await pi.fire('session_shutdown', { reason: 'reload' })
  await pi.fire('session_start', { reason: 'reload' })
  await settle()
  assert.deepEqual(runs.map(r => r.argv[3]), ['SessionStart'])
})

test('PI02: a tool call\'s end runs PostToolUse with its input, a failed one PostToolUseFailure', async () => {
  const pi = fakePi()
  const { host, runs } = fakeHost()
  install(pi.api, host)
  for (const [isError, event] of [[false, 'PostToolUse'], [true, 'PostToolUseFailure']] as const) {
    runs.length = 0
    await pi.fire('tool_execution_start', { toolCallId: CALL.toolCallId, toolName: 'bash', args: CALL.input })
    await pi.fire('tool_call', { toolCallId: CALL.toolCallId, toolName: 'bash', input: CALL.input })
    await pi.fire('tool_execution_end', { toolCallId: CALL.toolCallId, toolName: 'bash', result: {}, isError })
    await settle()
    const end = runs.find(r => r.argv[3] === event)
    assert.deepEqual(end?.stdin, fixture(event), event)
  }
})

test('PI02: the end of a run awaits Stop and turns its block into one continuation with the reminder', async () => {
  const pi = fakePi()
  const REMINDER = 'gruntas: you did work since your last status report.'
  const { host, runs } = fakeHost(e => (e === 'Stop' ? JSON.stringify({ decision: 'block', reason: REMINDER }) : ''))
  install(pi.api, host)
  await pi.fire('session_start', { reason: 'startup' })
  // Another extension's proposal, which the answer must keep.
  const theirs = { type: 'custom', customType: 'other', data: {} }
  const [first] = (await pi.fire('agent_before_settle', { entries: [theirs] })) as any[]
  const stops = () => runs.filter(r => r.argv[3] === 'Stop')
  assert.deepEqual(stops()[0]?.stdin, fixture('Stop'))
  assert.ok(stops()[0]!.limitMs <= 2000)
  assert.equal(first?.continue, true)
  assert.deepEqual(first?.entries[0], theirs)
  const added = first?.entries.slice(1)
  assert.equal(added.length, 1, JSON.stringify(first))
  assert.equal(added[0].type, 'custom_message')
  assert.ok(JSON.stringify(added[0].content).includes(REMINDER), JSON.stringify(added[0]))
  // The continuation's own end says so, and is never asked to continue again.
  const [second] = (await pi.fire('agent_before_settle', { entries: [] })) as any[]
  assert.equal((stops()[1]?.stdin as any)?.stop_hook_active, true)
  assert.notEqual(second?.continue, true)
  // The next run starts over: it may be reminded again.
  const [third] = (await pi.fire('agent_before_settle', { entries: [] })) as any[]
  assert.equal((stops()[2]?.stdin as any)?.stop_hook_active, false)
  assert.equal(third?.continue, true)
})

test('PI02: a continuation pi never ran (an abort) does not make the next run\'s end a continuation\'s', async () => {
  const pi = fakePi()
  const { host, runs } = fakeHost(e => (e === 'Stop' ? JSON.stringify({ decision: 'block', reason: 'report' }) : ''))
  install(pi.api, host)
  const [first] = (await pi.fire('agent_before_settle', { entries: [] })) as any[]
  assert.equal(first?.continue, true)
  await pi.fire('agent_settled', { aborted: true })
  const [next] = (await pi.fire('agent_before_settle', { entries: [] })) as any[]
  assert.equal((runs.filter(r => r.argv[3] === 'Stop')[1]?.stdin as any)?.stop_hook_active, false)
  assert.equal(next?.continue, true)
})

test('PI02: a Stop that answers nothing, or never answers, leaves the run to end', { timeout: 5000 }, async () => {
  for (const hangs of [false, true]) {
    const pi = fakePi()
    const { host } = fakeHost(() => undefined)
    // A host that never answers stands for a hook past its limit, which the
    // real host kills (see run, above) and answers nothing for.
    install(pi.api, hangs ? { ...host, run: (_a, _s, limitMs) => new Promise(r => setTimeout(() => r(undefined), Math.min(limitMs, 100))) } : host)
    await pi.fire('session_start', { reason: 'startup' })
    const [answer] = (await pi.fire('agent_before_settle', { entries: [] })) as any[]
    assert.notEqual(answer?.continue, true)
  }
})

test('PI02: without gruntas on the PATH, a session\'s start shows the install hint and runs nothing', async () => {
  const pi = fakePi()
  const { host, runs } = fakeHost(undefined, { onPath: false })
  install(pi.api, host)
  await pi.fire('session_start', { reason: 'startup' })
  await pi.fire('tool_call', { toolCallId: 't', toolName: 'bash', input: { command: 'echo hi' } })
  await pi.fire('agent_before_settle', { entries: [] })
  await pi.fire('session_shutdown', { reason: 'quit' })
  await settle()
  assert.deepEqual(runs, [])
  assert.deepEqual(pi.notes.map(n => n.message), [INSTALL_HINT])
})

// --- PI03: the bash guard and heavy builds -----------------------------------

const DENY = JSON.stringify({ hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: 'gruntas: that uploads to a paste site.' } })

test('PI03: a bash call awaits PreToolUse and is blocked with the guard\'s message', async () => {
  const pi = fakePi()
  const { host, runs } = fakeHost(e => (e === 'PreToolUse' ? DENY : ''))
  install(pi.api, host)
  const [answer] = (await pi.fire('tool_call', { toolCallId: CALL.toolCallId, toolName: 'bash', input: CALL.input })) as any[]
  const pre = runs.find(r => r.argv[3] === 'PreToolUse')
  assert.deepEqual(pre?.stdin, fixture('PreToolUse'))
  assert.ok(pre!.limitMs <= 2000)
  assert.deepEqual(answer, { block: true, reason: 'gruntas: that uploads to a paste site.' })
})

test('PI03: a bash call gruntas lets through, or a hook that answers nothing, runs untouched', async () => {
  for (const said of ['', undefined]) {
    const pi = fakePi()
    const { host } = fakeHost(() => said)
    install(pi.api, host)
    const input = { command: 'cargo build' }
    const answers = await pi.fire('tool_call', { toolCallId: 't', toolName: 'bash', input })
    assert.ok(answers.every(a => a === undefined || (a as any).block !== true), JSON.stringify(answers))
    assert.deepEqual(input, { command: 'cargo build' })
  }
})

test('PI03: only bash calls go to PreToolUse, as in the Claude Code plugin', async () => {
  const pi = fakePi()
  const { host, runs } = fakeHost()
  install(pi.api, host)
  await pi.fire('tool_call', { toolCallId: 't', toolName: 'read', input: { path: 'Cargo.toml' } })
  await settle()
  assert.deepEqual(runs.filter(r => r.argv[3] === 'PreToolUse'), [])
})

// --- PI04: the MCP tools and the skill ---------------------------------------

test('PI04: the gruntas MCP server is registered as gruntas-mcp, started by pi itself with no shell', () => {
  const pi = fakePi()
  install(pi.api, fakeHost().host)
  assert.equal(pi.mcp.length, 1)
  const [server] = pi.mcp
  // Tools are then mcp__gruntas__<tool>, which the hook reads as the report.
  assert.equal(server?.name, 'gruntas')
  assert.equal(server?.config.command, 'gruntas-mcp')
  assert.ok(!['sh', 'bash', 'env', 'npx'].includes(server?.config.command))
  // Declared to the model: pi's default (codemode) would hide them behind codemode.
  assert.equal(server?.config.exposure, 'direct')
})

test('PI04: without gruntas on the PATH, no MCP server is registered', () => {
  const pi = fakePi()
  install(pi.api, fakeHost(undefined, { onPath: false }).host)
  assert.deepEqual(pi.mcp, [])
})

test('PI04: with gruntas but no gruntas-mcp on the PATH (npm installs them apart), no MCP server is registered', () => {
  const pi = fakePi()
  const { host } = fakeHost()
  install(pi.api, { ...host, onPath: (name?: string) => name !== 'gruntas-mcp' })
  assert.deepEqual(pi.mcp, [])
})

/** Every SKILL.md at or under `at`. */
function skills(at: string): string[] {
  if (!existsSync(at)) return []
  if (!statSync(at).isDirectory()) return at.endsWith('SKILL.md') ? [at] : []
  return readdirSync(at).flatMap(n => skills(join(at, n)))
}

test('PI04: the package ships the gruntas skill', () => {
  const manifest = JSON.parse(readFileSync(join(pkg, 'package.json'), 'utf8'))
  assert.ok(manifest.pi.skills?.length > 0, 'no skills in the manifest')
  const found = manifest.pi.skills.flatMap((s: string) => skills(join(pkg, s)))
  assert.ok(found.some((f: string) => /^---\nname: gruntas\n/.test(readFileSync(f, 'utf8'))), JSON.stringify(found))
})
