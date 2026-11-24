// Gruntas's mod. It tells the gruntas daemon this Claude Code account's
// 5-hour and 7-day usage windows, so rollover can compare accounts by them
// (R-KIT-7). At session start, whenever a window moves, and every 5 minutes
// (the read is free), it runs `gruntas hook claude usage` with the windows and
// the account's config home on stdin. It never changes the session: no
// gruntas on the PATH, or a daemon that does not answer, and it says nothing.
//
// It also delivers the god agent's steers (god agent D5): the daemon writes
// each one its steer setting let through to this session's inbox, which the
// mod reads every 2 seconds with $.fs.read (no process spawned; one run of
// `gruntas hook claude inbox` at session start says where it is). A message,
// already framed with its origin by the daemon, is submitted as a prompt; an
// interrupt aborts the running turn, tracked from turn.start, or is answered
// "nothing running". Each is acked in the acks file beside the inbox, which
// the daemon waits on to answer the command.
//
// The daemon's own notices come the same way (Rollover for launched
// sessions D5): when this account runs low in a session `gruntas run`
// started, which account comes next and that `/exit` continues there,
// shown as a toast and kept in the status line.
//
// It answers `/gruntas:account` itself (D8), so the command costs no turn and
// leaves nothing to the model: the plugin's `commands/account.md` gives the
// name, and this module's `command.run` hook runs `gruntas hook claude
// account` with the session and the arguments, and shows what it says. With a
// name that it queued, the line stays in the status line until `/exit`.
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
  try {
    const home = configHome(await $.env.get('CLAUDE_CONFIG_DIR'), await $.env.get('HOME'))
    const stdin = home === undefined ? undefined : payload(home, limits)
    if (stdin === undefined) return
    // The hook itself gives up inside 2 s.
    await $.process.run(['gruntas', 'hook', 'claude', 'usage'], { stdin, timeoutMs: 2_000 })
  } catch {
    // No gruntas on the PATH, or one that did not finish in time.
  }
}

/**
 * Reads the windows, free, and sends them. A new session has none until its
 * first response: its first `session.measure` sends them then.
 */
async function report($: EngineInterface): Promise<void> {
  try {
    await send($, (await $.session.usage()).rateLimits)
  } catch {
    // Nothing to read: the next measurement or tick tries again.
  }
}

/** How often the steer inbox is read (D5). */
export const POLL_MS = 2_000

/** A steer older than this is not delivered: the daemon has answered its command by then (`inbox::STALE`). */
export const STALE_MS = 8_000

/** The acks kept in the acks file. */
const ACKS_KEPT = 64

/** One line of the inbox, as the daemon writes it (`sync::god::inbox::Steer`). */
export type Steer = { type: 'message' | 'notice'; id: string; text: string; at: string } | { type: 'interrupt'; id: string; at: string }

/** What came of a steer, as the daemon reads it (`inbox::Ack`). */
export type Ack = { id: string; outcome: 'delivered' | 'nothingRunning' | 'refused'; message?: string }

/** The steers in `inbox` to act on at `now`: well formed, fresh, and not in `done`. */
function due(inbox: string, done: ReadonlySet<string>, now: number): Steer[] {
  const out: Steer[] = []
  for (const line of inbox.split('\n')) {
    let s: Partial<Steer> & { type?: unknown }
    try {
      s = JSON.parse(line)
    } catch {
      continue
    }
    if (typeof s?.id !== 'string' || typeof s.at !== 'string' || done.has(s.id)) continue
    const age = now - Date.parse(s.at)
    if (!(age >= -STALE_MS && age < STALE_MS)) continue
    if ((s.type === 'message' || s.type === 'notice') && typeof (s as { text?: unknown }).text === 'string') out.push(s as Steer)
    else if (s.type === 'interrupt') out.push(s as Steer)
  }
  return out
}

/** The ids `acks` (the acks file) holds. */
function acked(acks: string): Set<string> {
  const ids = new Set<string>()
  for (const line of acks.split('\n')) {
    try {
      const a = JSON.parse(line) as Partial<Ack>
      if (typeof a?.id === 'string') ids.add(a.id)
    } catch {
      // A torn or foreign line.
    }
  }
  return ids
}

/**
 * The main loop's running turn, from its `turn.start` to its `turn.complete`.
 * A reload of the mod mid-turn forgets it until the next turn: an interrupt
 * then says nothing is running.
 */
let turn: string | undefined

/** The steers this module acted on, so each is delivered once. */
const done = new Set<string>()

/** Whether a read of the inbox is under way. */
let polling = false

/** What `s` comes to in this session. */
async function act($: EngineInterface, s: Steer): Promise<Ack> {
  if (s.type === 'interrupt') {
    const running = turn
    if (running === undefined) return { id: s.id, outcome: 'nothingRunning' }
    try {
      await $.turn.abort({ turnId: running })
      return { id: s.id, outcome: 'delivered' }
    } catch {
      return { id: s.id, outcome: 'nothingRunning' }
    }
  }
  if (s.type === 'notice') {
    // Not awaited, as a prompt is not: a UI that cannot show it loses nothing.
    void Promise.resolve($.ui.toast(s.text)).catch(() => undefined)
    void Promise.resolve($.ui.status(s.text)).catch(() => undefined)
    return { id: s.id, outcome: 'delivered' }
  }
  // The daemon framed it; a message that is not is not the daemon's.
  if (!s.text.startsWith('<steer from ')) return { id: s.id, outcome: 'refused', message: 'the message came without its origin' }
  // Submitted, not awaited: a busy session takes it once its turn ends.
  void $.prompt.submit({ text: s.text }).catch(() => undefined)
  return { id: s.id, outcome: 'delivered' }
}

/** One read of the inbox in `dir`: each steer due acted on once, and acked. */
async function poll($: EngineInterface, dir: string): Promise<void> {
  if (polling) return
  polling = true
  try {
    const session = await $.session.id()
    const inbox = await $.fs.read(`${dir}/${session}.jsonl`).catch(() => '')
    if (typeof inbox !== 'string' || inbox === '') return
    const acksPath = `${dir}/${session}.acks.jsonl`
    let steers = due(inbox, done, await $.clock.now())
    if (steers.length === 0) return
    const read = await $.fs.read(acksPath).catch(() => '')
    const acks = typeof read === 'string' ? read : ''
    const before = acked(acks)
    steers = steers.filter(s => !before.has(s.id))
    if (steers.length === 0) return
    let lines = acks.split('\n').filter(l => l.trim() !== '')
    // Each acked as soon as it is acted on, so a reload in between never
    // delivers one again.
    for (const s of steers) {
      done.add(s.id)
      lines = [...lines, JSON.stringify(await act($, s))].slice(-ACKS_KEPT)
      await $.fs.write(acksPath, lines.join('\n') + '\n')
    }
  } catch {
    // No inbox yet, or a read that failed: the next poll tries again.
  } finally {
    polling = false
  }
}

/** Where this machine's steer inboxes are, from `gruntas hook claude inbox`; none without gruntas. */
async function inboxDir($: EngineInterface): Promise<string | undefined> {
  try {
    const r = await $.process.run(['gruntas', 'hook', 'claude', 'inbox'], { timeoutMs: 2_000 })
    const dir = r.stdout.trim()
    return r.exitCode === 0 && dir.startsWith('/') ? dir : undefined
  } catch {
    return undefined
  }
}

/** How long `/gruntas:account` waits on gruntas: it asks every account's vendor whether it is signed in. */
export const ACCOUNT_MS = 20_000

/** `/gruntas:account [args]`: what gruntas says of it. */
async function account($: EngineInterface, args: string): Promise<string> {
  const words = (args ?? '').trim().split(/\s+/).filter(w => w !== '')
  let r
  try {
    r = await $.process.run(['gruntas', 'hook', 'claude', 'account', await $.session.id(), ...words], { timeoutMs: ACCOUNT_MS })
  } catch {
    return 'gruntas is not on the PATH, or did not answer: install it with curl -fsSL https://gruntas.com/install | bash'
  }
  const said = r.stdout.trimEnd() || r.stderr.trim() || 'gruntas said nothing'
  if (r.exitCode === 0 && words.length === 1) void Promise.resolve($.ui.status(words[0] === '--cancel' ? undefined : said)).catch(() => undefined)
  return said
}

export const register: Register = on => {
  on('command.run', { command: 'gruntas:account' }, async ($, e) => ({ text: await account($, e.args), isTextDisplayOnly: true }))

  on('session.start', async ($, e, next) => {
    const started = await next(e)
    void report($)
    $.clock.every(EVERY_MS, () => void report($))
    // Not awaited: the session starts at once.
    void inboxDir($).then(dir => {
      if (dir !== undefined) $.clock.every(POLL_MS, () => void poll($, dir))
    })
    return started
  })

  on('turn.start', ($, e, next) => {
    if (e.agentId === undefined) turn = e.turnId
    return next(e)
  })

  on('turn.complete', ($, e, next) => {
    if (e.agentId === undefined && e.turnId === turn) turn = undefined
    return next(e)
  })

  on('session.measure', async ($, e, next) => {
    // Not awaited: the measurement goes on at once.
    if (e.changed.includes('rateLimits')) void send($, e.rateLimits)
    return next(e)
  })
}
