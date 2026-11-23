// Gruntas's mod: tells the gruntas daemon this Claude Code account's 5-hour
// and 7-day usage windows, so rollover can compare accounts by them
// (R-KIT-7). At session start, whenever a window moves, and every 5 minutes
// (the read is free), it runs `gruntas hook claude usage` with the windows and
// the account's config home on stdin. It never changes the session: no
// gruntas on the PATH, or a daemon that does not answer, and it says nothing.
import type { EngineInterface, Register, SessionRateLimit } from 'claude-code'

/** One window as `gruntas hook claude usage` takes it. */
export type Window = { kind: 'fiveHour' | 'sevenDay'; usedPercent: number; resetsAtMs?: number }

const KINDS: Record<string, Window['kind'] | undefined> = { five_hour: 'fiveHour', seven_day: 'sevenDay' }

/** How often the windows are sent when none has moved. */
export const EVERY_MS = 5 * 60_000

/**
 * The hook's stdin for `limits`, the account in `configHome`: the 5-hour and
 * 7-day windows alone, or nothing when there is neither (off a subscription,
 * or before the first response reported them).
 */
export function payload(configHome: string, limits: readonly SessionRateLimit[]): string | undefined {
  const windows: Window[] = []
  for (const l of limits) {
    const kind = KINDS[l.kind]
    if (kind === undefined || !Number.isFinite(l.percentUsed)) continue
    const at = l.resetsAt === undefined ? NaN : Date.parse(l.resetsAt)
    windows.push(Number.isFinite(at) && at > 0 ? { kind, usedPercent: l.percentUsed, resetsAtMs: at } : { kind, usedPercent: l.percentUsed })
  }
  return windows.length === 0 ? undefined : JSON.stringify({ configHome, windows })
}

/** The account's config home, as Claude Code picks it: `CLAUDE_CONFIG_DIR`, else `~/.claude`. */
export function configHome(dir: string | undefined, home: string | undefined): string | undefined {
  if (dir !== undefined && dir.trim() !== '') return dir
  return home === undefined || home.trim() === '' ? undefined : `${home.replace(/\/+$/, '')}/.claude`
}

/** Runs the hook with `limits`; any failure is the hook's to log, never the session's. */
async function send($: EngineInterface, limits: readonly SessionRateLimit[]): Promise<void> {
  const home = configHome(await $.env.get('CLAUDE_CONFIG_DIR'), await $.env.get('HOME'))
  const stdin = home === undefined ? undefined : payload(home, limits)
  if (stdin === undefined) return
  try {
    await $.process.run(['gruntas', 'hook', 'claude', 'usage'], { stdin, timeoutMs: 3_000 })
  } catch {
    // No gruntas on the PATH, or one that did not finish in time.
  }
}

/** Reads the windows, free, and sends them. */
async function report($: EngineInterface): Promise<void> {
  try {
    await send($, (await $.session.usage()).rateLimits)
  } catch {
    // Nothing to read: the next measurement or tick tries again.
  }
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    void report($)
    $.clock.every(EVERY_MS, () => void report($))
    return started
  })

  on('session.measure', async ($, e, next) => {
    if (e.changed.includes('rateLimits')) await send($, e.rateLimits)
    return next(e)
  })
}
