// Rollover in pi (D9, D10; R-PI-8): what the provider does with each usage reading Claude Code
// reports (`rate_limit_event`). Every reading goes to gruntas's usage record, under the config
// home of the account it is for, through `gruntas hook pi usage`; a limit hit asks
// `gruntas providers next` where the session goes, and follows the person's rollover setting:
// `offer` asks (or, with no UI, says where it could go and stays), `auto` moves and says so,
// `off` leaves it to the person. Until the ladder's key rung can be kept here, `providers next`
// answers subscription accounts only.

import { homedir } from 'node:os'
import { join } from 'node:path'
import type { Account } from './launch.ts'

/** `gruntas providers next --json`'s answer. */
export interface Next {
  next: string | null
  mode: 'offer' | 'auto' | 'off'
  /** When the account it was asked from is out until, if gruntas knows. */
  resetsAt: number | null
}

/** One window of `gruntas hook pi usage`'s payload, in Claude Code's plugin names. */
export interface UsageWindow {
  kind: string
  usedPercent: number
  resetsAtMs?: number
}

/** Claude Code's window names (`five_hour`) as its plugin's usage report names them (`fiveHour`). */
function kind(name: string): string {
  return name.replace(/_([a-z])/g, (_m, c: string) => c.toUpperCase())
}

function percent(w: any): number | undefined {
  const u = w?.utilization
  if (typeof u !== 'number' || !Number.isFinite(u)) return undefined
  return u <= 1 ? u * 100 : u
}

function resetMs(w: any): number | undefined {
  return typeof w?.resetsAt === 'number' ? w.resetsAt * 1000 : undefined
}

/**
 * A `rate_limit_event`'s `rate_limit_info` as usage windows: each of `unifiedWindows`, else the
 * window it names. A window with no figure is left out, save a rejected one, which is spent.
 */
export function usageWindows(info: any): UsageWindow[] {
  if (!info || typeof info !== 'object') return []
  const named = typeof info.rateLimitType === 'string' ? info.rateLimitType : ''
  const rejected = info.status === 'rejected'
  const out = new Map<string, UsageWindow>()
  const add = (name: string, w: any) => {
    // The window a rejection names is spent, whatever figure it carries: gruntas reads a limit
    // from the figure alone.
    const used = rejected && name === named ? Math.max(percent(w) ?? 100, 100) : percent(w)
    if (!name || used === undefined) return
    const at = resetMs(w)
    out.set(name, { kind: kind(name), usedPercent: used, ...(at === undefined ? {} : { resetsAtMs: at }) })
  }
  for (const [name, w] of Object.entries(info.unifiedWindows ?? {})) add(name, w)
  if (named && !out.has(named)) add(named, info)
  return [...out.values()]
}

/** Claude Code's config home for `account`: where gruntas keeps its usage. */
export function configHome(account: Account, env: Record<string, string | undefined> = process.env): string {
  return account.home ?? env.CLAUDE_CONFIG_DIR ?? join(homedir(), '.claude')
}

/** `gruntas hook pi usage`'s payload for a reading on `account`, none when it has no window. */
export function usagePayload(account: Account, info: unknown, env?: Record<string, string | undefined>): string | undefined {
  const windows = usageWindows(info)
  if (windows.length === 0) return undefined
  return JSON.stringify({ configHome: configHome(account, env), windows })
}

/** The limit a reading says was hit, with when it resets; none below the limit. */
export function limitHit(info: any): { resetsAt?: number } | undefined {
  if (info?.status !== 'rejected') return undefined
  const at = resetMs(info) ?? resetMs(info.unifiedWindows?.[info.rateLimitType])
  return at === undefined ? {} : { resetsAt: at }
}

/** When `ms` is, for a person: the time today, else the day and time. */
export function when(ms: number, now = Date.now()): string {
  const d = new Date(ms)
  const n = new Date(now)
  const hm = `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
  if (d.toDateString() === n.toDateString()) return hm
  return `${d.toLocaleDateString('en-US', { weekday: 'short', month: 'short', day: 'numeric' })} ${hm}`
}

/** What rollover shows and asks through: pi's `ctx.ui`, as far as it uses it. */
export interface Ui {
  hasUI: boolean
  confirm(title: string, message: string, opts: { timeout: number }): Promise<boolean>
  notify(message: string, level: 'info' | 'warning' | 'error'): void
}

/** How long the offer waits for an answer: a host that never shows it cannot hang the session. */
export const OFFER_TIMEOUT_MS = 60_000

/** The session's account, as rollover moves it. */
export interface Moves {
  readonly name: string | undefined
  moveTo(name: string): Promise<unknown>
}

/** How long a spent account whose reset was not said is passed over: a 5-hour window. */
const UNKNOWN_RESET_MS = 5 * 3_600_000

export class Rollover {
  /** Accounts this session found spent, with when each is passed over until. */
  private readonly tried = new Map<string, number>()
  private busy?: Promise<void>
  private readonly accounts: Moves
  private readonly next: (from: string, tried: string[]) => Promise<Next | undefined>
  private readonly ui: () => Ui | undefined
  private readonly now: () => number

  constructor(accounts: Moves, next: (from: string, tried: string[]) => Promise<Next | undefined>, ui: () => Ui | undefined, now: () => number = Date.now) {
    this.accounts = accounts
    this.next = next
    this.ui = ui
    this.now = now
  }

  /** A limit hit on `account`: one handling at a time, as Claude Code may say it more than once. */
  hit(account: string, resetsAt?: number): Promise<void> {
    this.busy ??= this.handle(account, resetsAt).finally(() => (this.busy = undefined))
    return this.busy
  }

  private async handle(account: string, resetsAt?: number): Promise<void> {
    // The session already moved on: an old request's reading.
    if (this.accounts.name !== undefined && this.accounts.name !== account) return
    const now = this.now()
    for (const [name, at] of this.tried) if (at <= now) this.tried.delete(name)
    // Not said when it resets: passed over for a 5-hour window's length.
    this.tried.set(account, resetsAt ?? now + UNKNOWN_RESET_MS)
    let answer: Next | undefined
    try {
      answer = await this.next(account, [...this.tried.keys()].filter(t => t !== account))
    } catch {
      answer = undefined
    }
    const at = resetsAt ?? answer?.resetsAt ?? undefined
    if (resetsAt === undefined && at !== undefined) this.tried.set(account, at)
    // gruntas could not say, or the person keeps rollover off: pi's own error stands.
    if (!answer || answer.mode === 'off') return
    const out = `${account} is out of usage${at === undefined ? '' : ` until ${when(at, now)}`}`
    const ui = this.ui()
    const notify = (m: string, level: 'info' | 'warning' | 'error' = 'info') => ui?.notify(m, level)
    const to = answer.next
    if (!to) return notify(`${out}, and no other subscription account has room.`, 'warning')
    if (answer.mode === 'offer') {
      if (!ui?.hasUI) return notify(`${out}. ${to} has room; rollover is offer, so this session stays on ${account}.`, 'warning')
      const yes = await ui.confirm(`${out}. Continue on ${to}?`, 'The conversation goes to it afresh, so its prompt cache starts cold.', { timeout: OFFER_TIMEOUT_MS }).catch(() => false)
      if (!yes) return
    }
    try {
      await this.accounts.moveTo(to)
    } catch (e) {
      return notify(`Could not continue on ${to}: ${e instanceof Error ? e.message : String(e)}`, 'error')
    }
    if (answer.mode === 'auto') notify(`Continued on ${to}: ${out}.`)
  }
}
