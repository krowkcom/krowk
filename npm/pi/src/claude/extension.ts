// pi's `gruntas` provider (D5, D7): models `gruntas/<model>`, each turn run through Claude Code
// via the Claude Agent SDK on the session's gruntas account, with pi's tools bridged in and run
// by pi. Wired as pi-claude-subscription 0.2.4's index.ts wires its provider (MIT;
// THIRD-PARTY-NOTICES).

import { spawn } from 'node:child_process'
import { appendFileSync } from 'node:fs'
import { join } from 'node:path'
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { Accounts, agentDir, autoPick, type Gruntas, type Listed } from './accounts.ts'
import { anthropicChatModels, claudeModels } from './models.ts'
import { API, ClaudeSdkProvider, type ProviderOptions } from './provider.ts'

export const PROVIDER = 'gruntas'

/** What the provider needs that only a real pi and the Agent SDK give. */
export type Runtime = Pick<ProviderOptions, 'createSession' | 'ai'>

/** The provider and the session's account, which the picker and rollover use. */
export interface Claude {
  accounts: Accounts
  provider: ClaudeSdkProvider
}

/** Registers the `gruntas` provider on `pi`. */
export function installClaude(pi: ExtensionAPI, runtime: Runtime, accounts: Accounts, debugPath = process.env.GRUNTAS_PI_DEBUG): Claude {
  // GRUNTAS_PI_DEBUG=<file>: each request's account and route, as pi-claude-subscription's PI_CLAUDE_SDK_DEBUG.
  const debug = debugPath ? (msg: string) => appendFileSync(debugPath, `${new Date().toISOString()} ${msg}\n`) : undefined
  const onRateLimit = debug && ((a: { name: string }, info: unknown) => debug(`usage account=${a.name} ${JSON.stringify(info)}`))
  const provider = new ClaudeSdkProvider({ ...runtime, account: () => accounts.current(), debug, ...(onRateLimit ? { onRateLimit } : {}) })
  accounts.onMove(to => provider.retire(to.name))

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

/** The machine's gruntas, as the accounts read it. */
export const systemGruntas: Gruntas = {
  async list(): Promise<Listed[]> {
    const r = await gruntas(['providers', 'list', '--json'], 15_000)
    if (!r || r.code !== 0) throw new Error(`gruntas: \`gruntas providers list\` failed${r?.err ? `: ${r.err.trim()}` : ''}`)
    return JSON.parse(r.out).data.instances
  },
  async auto(): Promise<string | undefined> {
    // `--version` makes the launch it resolves a cheap one: Claude Code prints its version.
    const r = await gruntas(['run', 'claude', '--account', 'auto', '--no-plugin', '--version'], 15_000)
    return r && autoPick(r.err)
  },
}

/** The accounts of the pi session in `env`'s pi config home. */
export function systemAccounts(env: Record<string, string | undefined>): Accounts {
  return new Accounts(systemGruntas, join(agentDir(env), 'gruntas', 'account.json'))
}
