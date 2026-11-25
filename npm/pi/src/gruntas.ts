// @gruntas/pi: pi joins gruntas. Each pi event the extension handles runs
// `gruntas hook pi <event>` with a Claude-shaped payload on stdin (./hook.ts),
// the entry point gruntas's Claude Code and Codex plugins run. The extension
// waits on the hook only where gruntas answers (the paste guard on a bash
// call, the reminder at the end of a run) and fires the rest.
//
// Without gruntas on the PATH it does nothing but show an install hint as a
// session starts; on Windows, where gruntas's hooks and daemon do not run, it
// does nothing at all.
//
// It also gives pi the `gruntas` provider (./claude/): Claude turns run through
// Claude Code on the session's gruntas account.

import type { ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { installClaude, systemAccounts } from './claude/extension.ts'
import type { Claude } from './claude/extension.ts'
import { argv, LIMIT_MS, sessionEnd, sessionStart, stop, systemHost, toolEvent } from './hook.ts'
import type { HookEvent, Host, Session } from './hook.ts'

/** What the session-start hint says when gruntas is not on the PATH. */
export const INSTALL_HINT = 'gruntas is not on the PATH, so this session is not registered with it. Install it: curl -fsSL https://gruntas.com/install | bash'

type On = (event: string, handler: (event: any, ctx: any) => unknown) => void

/** The session `ctx` is in, as the payloads name it. */
function session(ctx: any): Session {
  const sm = ctx.sessionManager
  return { id: sm.getSessionId(), file: sm.getSessionFile() ?? undefined, cwd: sm.getCwd?.() ?? ctx.cwd }
}

/** The reason of a Stop answer that asks the agent to go on (Claude Code's `{"decision":"block"}`). */
function blockReason(out: string | undefined): string | undefined {
  try {
    const a = JSON.parse(out ?? '')
    return a?.decision === 'block' && typeof a.reason === 'string' && a.reason ? a.reason : undefined
  } catch {
    return undefined
  }
}

/** The extension, on `host`: what pi's factory does, on the machine as it is. */
export function install(pi: ExtensionAPI, host: Host): void {
  if (host.platform === 'win32') return
  const on = pi.on.bind(pi) as On

  // Looked up as pi loads the extension, which it does again on a reload or
  // a session switch.
  if (!host.onPath()) {
    on('session_start', (_e, ctx) => ctx.ui.notify(INSTALL_HINT, 'warning'))
    return
  }

  /** Runs the hook without waiting on it: the host kills it past the limit, so none outlives it. */
  const fire = (event: HookEvent, payload: string) => void host.run(argv(event), payload, LIMIT_MS)

  on('session_start', (e, ctx) => fire('SessionStart', sessionStart(session(ctx), e.reason)))
  // A reload tears the runtime down and starts the same session again: its
  // SessionEnd, not waited on, could land after the new start and
  // deregister the session that is still running.
  on('session_shutdown', (e, ctx) => {
    if (e.reason !== 'reload') fire('SessionEnd', sessionEnd(session(ctx), e.reason))
  })

  // pi's tool end carries no input: keep each call's from its start, then
  // the object tool_call hands on, which tool_call handlers mutate in place.
  const inputs = new Map<string, unknown>()
  on('tool_execution_start', e => void inputs.set(e.toolCallId, e.args))
  on('tool_call', e => void inputs.set(e.toolCallId, e.input))
  on('tool_execution_end', (e, ctx) => {
    const input = inputs.get(e.toolCallId)
    inputs.delete(e.toolCallId)
    const event = e.isError ? 'PostToolUseFailure' : 'PostToolUse'
    fire(event, toolEvent(event, session(ctx), { toolCallId: e.toolCallId, toolName: e.toolName, input }))
  })

  // The end of a run, pi's last boundary before it settles, asks gruntas
  // whether the run's work went unreported; a block becomes one continuation
  // carrying the reminder. That continuation's own end is sent as one
  // (stop_hook_active), and is never continued, so the reminder never comes
  // twice in a row.
  let continuing = false
  on('agent_before_settle', async (e, ctx) => {
    const active = continuing
    continuing = false
    const reason = blockReason(await host.run(argv('Stop'), stop(session(ctx), active), LIMIT_MS))
    if (!reason || active) return
    continuing = true
    const reminder = { type: 'custom_message', customType: 'gruntas-report-reminder', content: reason, display: true }
    return { entries: [...(e.entries ?? []), reminder], continue: true }
  })
  // pi settles once it will not run again by itself: a continuation it ran
  // ended in the boundary above first, so one it never ran (aborted, or
  // overruled by a later extension) must not mark the next run as one.
  on('agent_settled', () => void (continuing = false))
}

/**
 * The `gruntas` provider, where gruntas is there to start Claude Code. A runtime
 * that cannot load (a broken install) leaves it out and says so as a session
 * starts; the rest of the extension goes on.
 */
export async function claude(pi: ExtensionAPI, host: Host, env: Record<string, string | undefined>): Promise<Claude | undefined> {
  if (host.platform === 'win32' || !host.onPath()) return undefined
  try {
    const { runtime } = await import('./claude/runtime.ts')
    return installClaude(pi, runtime, systemAccounts(env))
  } catch (e) {
    pi.on('session_start', (_event, ctx) => ctx.ui.notify(`gruntas: Claude turns through your gruntas accounts are off: ${e instanceof Error ? e.message : String(e)}`, 'warning'))
    return undefined
  }
}

export default async function gruntas(pi: ExtensionAPI): Promise<void> {
  const host = systemHost()
  install(pi, host)
  await claude(pi, host, process.env)
}
