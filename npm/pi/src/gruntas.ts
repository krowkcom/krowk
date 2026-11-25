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
import { systemHost } from './hook.ts'
import type { Host } from './hook.ts'

/** What the session-start hint says when gruntas is not on the PATH. */
export const INSTALL_HINT = 'gruntas is not on the PATH, so this session is not registered with it. Install it: curl -fsSL https://gruntas.com/install | bash'

/** The extension, on `host`: what pi's factory does, on the machine as it is. */
export function install(_pi: ExtensionAPI, host: Host): void {
  if (host.platform === 'win32') return
  // The events are wired by the tickets that follow: session and run events,
  // the bash guard, the MCP server and skill.
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
