// /account (R-PI-7, D7): the picker that moves a pi session to another gruntas account, and the
// footer that names the account it is on. The rows are `gruntas providers list --check --json`'s:
// every account, with its readiness and usage left per window; Codex accounts are shown but
// pi cannot run on them yet. With no UI (print or JSON mode) nothing opens: `/account <name>`
// moves directly, and a bare `/account` prints the list (D10).

import type { ExtensionAPI } from '@earendil-works/pi-coding-agent'
import { usable, type Accounts, type Listed } from './accounts.ts'

/** The footer's key: one status line, `gruntas: <account>`. */
export const STATUS_KEY = 'gruntas-account'
/** How long the picker stays open before it closes by itself, so no host can hang on it. */
export const PICK_TIMEOUT_MS = 60_000

const WINDOWS: Record<string, string> = { five_hour: '5h', seven_day: '7d', seven_day_overage_included: '7d overage', seven_day_opus: '7d opus', seven_day_sonnet: '7d sonnet' }

/** `5h 93% · 7d 61%`: each window's usage left, as the listing gives it. */
function usageLeft(row: Listed): string {
  return (row.usage ?? [])
    .filter(w => typeof w.left === 'number')
    .map(w => `${WINDOWS[w.window] ?? w.window.replaceAll('_', ' ')} ${Math.round(w.left)}%`)
    .join(', ')
}

/** Why pi cannot move to `row` now, or nothing when it can. */
export function refusal(row: Listed): string | undefined {
  if (!usable(row)) return `${row.instance} is not usable from pi yet: pi runs Claude Code accounts only`
  if (!row.ready) return `${row.instance} is not ready${row.reason || row.fix ? `: ${row.reason || row.fix}` : ''}. \`gruntas providers list --check\` says more`
  return undefined
}

/** One picker row: the current one marked, then name, kind, readiness and usage left. */
export function line(row: Listed, current: string | undefined): string {
  const mark = row.instance === current ? '●' : ' '
  const kind = row.label ?? row.kind
  if (!usable(row)) return `${mark} ${row.instance} · ${kind} · not usable from pi yet`
  const ready = row.ready ? 'ready' : (row.state ?? 'not ready').replaceAll('_', ' ')
  const left = usageLeft(row)
  return `${mark} ${row.instance} · ${kind} · ${ready}${left ? ` · left ${left}` : ''}`
}

/** What the footer says while the session runs on `name`. */
export function footer(name: string): string {
  return `gruntas: ${name}`
}

type Ctx = { hasUI: boolean; ui: any; model?: { provider?: string } }

/** Says `message` where the person sees it: pi's notice, or stderr with no UI. */
function say(ctx: Ctx, message: string, level: 'info' | 'warning' | 'error' = 'info'): void {
  if (ctx.hasUI) ctx.ui.notify(message, level)
  else process.stderr.write(`${message}\n`)
}

/** Registers /account on `pi` and keeps the footer on the session's account. */
export function installPicker(pi: ExtensionAPI, accounts: Accounts, provider: string): void {
  // The context of the session's latest event: where the footer is. Each follow counts, so an
  // account settled for an earlier one does not land after a later one.
  let latest: Ctx | undefined
  let serving = false
  let follows = 0
  const show = (ctx: Ctx | undefined, name: string | undefined) => {
    try {
      ctx?.ui.setStatus(STATUS_KEY, name ? footer(name) : undefined)
    } catch {
      // A context pi has retired (a session replaced, reloaded or ended) has no footer to set.
    }
  }

  // The footer names the account only while a gruntas model serves the session; settling the
  // account here starts the session where its first request would (D8).
  const follow = (ctx: Ctx, onGruntas: boolean) => {
    latest = ctx
    serving = onGruntas
    const n = ++follows
    if (!onGruntas) return show(ctx, undefined)
    const settled = (name: string | undefined) => n === follows && serving && show(latest, name)
    accounts.current().then(a => settled(a.name), () => settled(accounts.name))
  }
  pi.on('session_start', (_e, ctx: any) => follow(ctx, ctx.model?.provider === provider))
  pi.on('model_select', (e, ctx: any) => follow(ctx, e.model?.provider === provider))
  pi.on('session_shutdown', () => {
    latest = undefined
    serving = false
    follows++
  })
  accounts.onMove(to => serving && show(latest, to.name))

  // Completions ask gruntas at most once in a few seconds, however fast the person types.
  let recent: { at: number; rows: Promise<Listed[]> } | undefined
  const listRecent = () => {
    if (!recent || Date.now() - recent.at > 5_000) recent = { at: Date.now(), rows: accounts.list() }
    return recent.rows
  }

  const move = async (ctx: Ctx, row: Listed) => {
    const no = refusal(row)
    if (no) return say(ctx, no, 'warning')
    await accounts.moveTo(row.instance)
    say(ctx, `gruntas: the next request runs on ${row.instance}`)
  }

  pi.registerCommand('account', {
    description: 'Pick the gruntas account Claude turns run on (`/account <name>` moves directly)',
    getArgumentCompletions: async prefix => {
      try {
        const rows = (await listRecent()).filter(r => !refusal(r) && r.instance.startsWith(prefix))
        return rows.length ? rows.map(r => ({ value: r.instance, label: r.instance })) : null
      } catch {
        return null
      }
    },
    handler: async (args, ctx) => {
      const c = ctx as unknown as Ctx
      latest = c
      try {
        const rows = await accounts.list()
        const name = args.trim()
        if (name) {
          const row = rows.find(r => r.instance === name)
          return row ? await move(c, row) : say(c, `gruntas has no account named ${name}: \`/account\` lists them`, 'warning')
        }
        const current = accounts.name
        const lines = rows.map(r => line(r, current))
        if (!c.hasUI) return say(c, `gruntas accounts (\`/account <name>\` moves to one):\n${lines.join('\n')}`)
        const picked = await c.ui.select('gruntas account for Claude turns', lines, { timeout: PICK_TIMEOUT_MS })
        const row = picked === undefined ? undefined : rows[lines.indexOf(picked)]
        if (row && row.instance !== current) await move(c, row)
      } catch (e) {
        say(c, `gruntas: ${e instanceof Error ? e.message : String(e)}`, 'error')
      }
    },
  })
}
