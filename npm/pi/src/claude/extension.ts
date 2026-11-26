// pi's `gruntas` provider (D5, D7): models `gruntas/<model>`, each turn run through Claude Code
// via the Claude Agent SDK on the session's gruntas account, with pi's tools bridged in and run
// by pi. Wired as pi-claude-subscription 0.2.4's index.ts wires its provider (MIT;
// THIRD-PARTY-NOTICES).

import { spawn } from 'node:child_process'
import { appendFileSync } from 'node:fs'
import { join } from 'node:path'
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Accounts, agentDir, type Gruntas, type Listed } from './accounts.ts'
import type { Account } from './launch.ts'
import { anthropicChatModels, claudeModels } from './models.ts'
import { installPicker } from './picker.ts'
import { API, ClaudeSdkProvider, type ProviderOptions } from './provider.ts'
import { limitHit, Rollover, usagePayload, type Next, type Ui } from './rollover.ts'

export const PROVIDER = 'gruntas'

/** What the provider needs that only a real pi and the Agent SDK give. */
export type Runtime = Pick<ProviderOptions, 'createSession' | 'ai'>

/** The provider and the session's account, which the picker and rollover use. */
export interface Claude {
  accounts: Accounts
  provider: ClaudeSdkProvider
}

export interface ClaudeOptions {
  /** Sends `gruntas hook pi usage` a payload, without waiting on it. */
  usage?: (payload: string) => void
  /** GRUNTAS_PI_DEBUG=<file>: each request's account and route, as pi-claude-subscription's PI_CLAUDE_SDK_DEBUG. */
  debugPath?: string
}

/**
 * pi's `ctx.ui` as rollover uses it; with no UI (print or JSON mode) a notice goes to stderr. A
 * limit can be handled after pi has retired `ctx` (print mode ends the session as the turn
 * ends), where touching it throws: the notice then goes to stderr, and an offer is declined.
 */
export function rolloverUi(ctx: { hasUI: boolean; ui: any }): Ui {
  return {
    hasUI: ctx.hasUI,
    confirm: async (title, message, opts) => ctx.ui.confirm(title, message, opts),
    notify: (message, level) => {
      try {
        if (ctx.hasUI) return void ctx.ui.notify(message, level)
      } catch {}
      process.stderr.write(`gruntas: ${message}\n`)
    },
  }
}

/** Registers the `gruntas` provider on `pi`. */
export function installClaude(pi: ExtensionAPI, runtime: Runtime, accounts: Accounts, opts: ClaudeOptions = {}): Claude {
  const debugPath = opts.debugPath ?? process.env.GRUNTAS_PI_DEBUG
  const debug = debugPath ? (msg: string) => appendFileSync(debugPath, `${new Date().toISOString()} ${msg}\n`) : undefined
  // The UI of the session's latest event: where rollover asks and tells.
  let ui: Ui | undefined
  const rollover = new Rollover(accounts, (from, tried) => accounts.next(from, tried), () => ui)
  const onRateLimit = (a: Account, info: unknown) => {
    debug?.(`usage account=${a.name} ${JSON.stringify(info)}`)
    const payload = usagePayload(a, info)
    if (payload) opts.usage?.(payload)
    const hit = limitHit(info)
    // Never a rejection pi would die of.
    if (hit) rollover.hit(a.name, hit.resetsAt).catch(() => {})
  }
  const provider = new ClaudeSdkProvider({ ...runtime, account: () => rollover.settled().then(() => accounts.current()), debug, onRateLimit })
  accounts.onMove(to => provider.retire(to.name))
  for (const event of ['session_start', 'agent_start', 'turn_start'] as const) pi.on(event, (_event: unknown, ctx: any) => void (ui = rolloverUi(ctx)))
  installPicker(pi, accounts, PROVIDER)

  const register = (models = claudeModels()) =>
    pi.registerProvider(PROVIDER, {
      name: 'gruntas (Claude Code on your accounts)',
      // Not a credential: Claude Code is started by gruntas, which keeps the account's login or
      // key. pi only needs a non-empty value to treat the provider as configured.
      apiKey: 'gruntas-account',
      api: API as never,
      baseUrl: 'gruntas://claude-code',
      models,
      streamSimple: provider.streamSimple,
    })
  register()

  // Pick up Claude models newer than the bundled snapshot from pi's own catalog.
  let refreshed = false
  pi.on('session_start', (_event, ctx) => {
    if (refreshed) return
    refreshed = true
    try {
      const live = anthropicChatModels(ctx.modelRegistry.getAll() as never)
      const known = new Set(claudeModels().map(m => m.id))
      if (live.some(m => !/-\d{8}$/.test(m.id) && !known.has(m.id))) register(claudeModels(live))
    } catch {
      // The snapshot stays registered.
    }
  })
  pi.on('session_shutdown', () => provider.shutdown())
  process.once('exit', () => provider.shutdown())
  return { accounts, provider }
}

/** Runs `gruntas <args>` with no shell; its stdout and stderr, or nothing when it could not run in `limitMs`. */
function gruntas(args: string[], limitMs: number): Promise<{ code: number | null; out: string; err: string } | undefined> {
  return new Promise(resolve => {
    let out = ''
    let err = ''
    const child = spawn('gruntas', args, { stdio: ['ignore', 'pipe', 'pipe'], timeout: limitMs })
    child.stdout.setEncoding('utf8').on('data', (d: string) => (out += d))
    child.stderr.setEncoding('utf8').on('data', (d: string) => (err += d))
    child.on('error', () => resolve(undefined))
    child.on('close', code => resolve({ code, out, err }))
  })
}

/** `gruntas providers next --json <args>`'s answer; none when gruntas could not say. */
async function providersNext(args: string[]): Promise<Next | undefined> {
  const r = await gruntas(['providers', 'next', '--json', ...args], 15_000)
  if (!r || r.code !== 0) return undefined
  try {
    return JSON.parse(r.out).data
  } catch {
    return undefined
  }
}

/**
 * The rows of `gruntas providers list --check --json`: its data, or, when no account is ready
 * (exit 3, `none_ready`), the same rows on the failure's details. None when it said neither.
 */
export function listed(r: { code: number | null; out: string; err: string }): Listed[] | undefined {
  try {
    const rows = r.code === 0 ? JSON.parse(r.out).data.instances : r.code === 3 ? JSON.parse(r.err).error.details.instances : undefined
    return Array.isArray(rows) ? rows : undefined
  } catch {
    return undefined
  }
}

/** The machine's gruntas, as the accounts read it. */
export const systemGruntas: Gruntas = {
  async list(): Promise<Listed[]> {
    const r = await gruntas(['providers', 'list', '--check', '--json'], 15_000)
    const rows = r && listed(r)
    if (!rows) throw new Error(`gruntas: \`gruntas providers list --check\` failed${r?.err ? `: ${r.err.trim()}` : ''}`)
    return rows
  },
  async auto(): Promise<string | undefined> {
    // None with room, or no answer: the default account, as `--account auto` falls back to it.
    return (await providersNext([]))?.next ?? 'claude'
  },
  next(from: string, tried: string[]): Promise<Next | undefined> {
    return providersNext(['--from', from, ...(tried.length ? ['--tried', tried.join(',')] : [])])
  },
}

/**
 * The accounts of the pi session in `env`'s pi config home, starting on the one `gruntas run pi
 * --account` put in GRUNTAS_PI_ACCOUNT, when it names one pi can run on.
 */
export function systemAccounts(env: Record<string, string | undefined>): Accounts {
  return new Accounts(systemGruntas, join(agentDir(env), 'gruntas', 'account.json'), env.GRUNTAS_PI_ACCOUNT)
}
