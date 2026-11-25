// The session's account: which gruntas account pi's Claude turns run on. It is session state,
// not part of the model (D7). A session starts on the account it used last, else on the ready
// account with the most headroom by rollover's ranking (D8); `moveTo` moves it from the next
// request, which the picker (/account) and rollover call.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import type { Account } from './launch.ts'

/** One row of `gruntas providers list --json`, as far as the extension reads it. No credential is in it. */
export interface Listed {
  instance: string
  kind: string
  ready: boolean
  config_dir?: string | null
}

/** How the accounts reach gruntas: one seam, so tests replace it. */
export interface Gruntas {
  /** `gruntas providers list --json`'s instances. */
  list(): Promise<Listed[]>
  /** The account `gruntas run claude --account auto` picks: rollover's ranking's first ready one. */
  auto(): Promise<string | undefined>
}

/** pi's config home, where the extension keeps the account a session used last. */
export function agentDir(env: Record<string, string | undefined>): string {
  return env.PI_CODING_AGENT_DIR || join(homedir(), '.pi', 'agent')
}

/** Whether pi can run on `row`: a Claude Code account. Codex accounts are not usable from pi yet. */
export function usable(row: Listed): boolean {
  return row.kind === 'claude-code'
}

export class Accounts {
  private now?: Account
  private starting?: Promise<Account>
  private readonly moved: ((to: Account) => void)[] = []
  private readonly gruntas: Gruntas
  /** The file the last account is kept in. */
  readonly file: string

  constructor(gruntas: Gruntas, file: string) {
    this.gruntas = gruntas
    this.file = file
  }

  /** The account the next request runs on; the first call settles where the session starts. */
  current(): Promise<Account> {
    if (this.now) return Promise.resolve(this.now)
    this.starting ??= this.start().finally(() => (this.starting = undefined))
    return this.starting
  }

  /** The account it is on, if it has started: what a footer shows. */
  get name(): string | undefined {
    return this.now?.name
  }

  /** Called with the new account on each move. */
  onMove(f: (to: Account) => void): void {
    this.moved.push(f)
  }

  /**
   * Moves the session to `name` from the next request: the new account gets the conversation pi
   * holds, its prompt cache cold. Refused for an account gruntas does not list, or one pi cannot
   * run on (a Codex account).
   */
  async moveTo(name: string): Promise<Account> {
    const row = (await this.gruntas.list()).find(r => r.instance === name)
    if (!row) throw new Error(`gruntas has no account named ${name}`)
    if (!usable(row)) throw new Error(`${name} is not usable from pi yet: pi runs Claude Code accounts only`)
    const to = account(row)
    this.set(to)
    for (const f of this.moved) f(to)
    return to
  }

  private async start(): Promise<Account> {
    const rows = await this.gruntas.list()
    const find = (name: string | undefined) => rows.find(r => r.instance === name && usable(r) && r.ready)
    const row = find(this.last()) ?? find(await this.gruntas.auto())
    if (!row) throw new Error('gruntas: no Claude Code account is ready. `gruntas providers list --check` says why')
    const to = account(row)
    this.set(to)
    return to
  }

  private set(to: Account): void {
    this.now = to
    try {
      mkdirSync(dirname(this.file), { recursive: true })
      writeFileSync(`${this.file}.tmp`, JSON.stringify({ last: to.name }) + '\n')
      renameSync(`${this.file}.tmp`, this.file)
    } catch {
      // Remembering it is for the next session; this one goes on.
    }
  }

  private last(): string | undefined {
    try {
      const last = JSON.parse(readFileSync(this.file, 'utf8')).last
      return typeof last === 'string' ? last : undefined
    } catch {
      return undefined
    }
  }
}

function account(row: Listed): Account {
  return { name: row.instance, ...(row.config_dir ? { home: row.config_dir } : {}) }
}

/** The line `--account auto` writes on stderr: `--account auto: <name>, <why>`. */
export function autoPick(stderr: string): string | undefined {
  return /^--account auto: ([^,\s]+),/m.exec(stderr)?.[1]
}
