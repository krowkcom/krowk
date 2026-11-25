// How a Claude turn's Claude Code is started: by gruntas, on the session's account
// (`gruntas run claude --account <name> --no-plugin`), so the account's login or API key stays
// in gruntas and never passes through pi (D5, D6). Nothing here reads a credential: the
// environment pi hands on is pi's own, less what would make Claude Code think it runs nested.

import { spawn } from 'node:child_process'
import { accessSync, constants, readFileSync } from 'node:fs'
import { delimiter, join } from 'node:path'

/** A gruntas account as the provider runs on it. */
export interface Account {
  /** Its gruntas name (`gruntas providers list`'s `instance`). */
  name: string
  /** Its Claude Code config home, where its transcripts are; none when gruntas does not say. */
  home?: string
}

/** What the Agent SDK asks of a spawned Claude Code (its `SpawnOptions`). */
export interface SpawnRequest {
  command: string
  args: string[]
  cwd?: string
  env: Record<string, string | undefined>
  signal?: AbortSignal
}

/** The launcher's argv for Claude Code's `args` on `account`; the SDK's `command` is not used. */
export function launcherArgv(account: string, args: readonly string[]): string[] {
  return ['gruntas', 'run', 'claude', '--account', account, '--no-plugin', ...args]
}

/** `env` less the variables that tell Claude Code it runs inside another Claude Code. */
export function launcherEnv(env: Record<string, string | undefined>): Record<string, string> {
  const out: Record<string, string> = {}
  for (const [k, v] of Object.entries(env)) {
    if (v === undefined || k === 'CLAUDECODE' || k === 'CLAUDE_CODE_ENTRYPOINT') continue
    out[k] = v
  }
  return out
}

/**
 * The `gruntas` the launch runs, as the SDK's `pathToClaudeCodeExecutable`: given none, the SDK
 * looks for the Claude Code it bundles, which this provider never runs.
 */
export function gruntasPath(path: string | undefined): string {
  for (const dir of (path ?? '').split(delimiter)) {
    if (!dir) continue
    try {
      accessSync(join(dir, 'gruntas'), constants.X_OK)
      return join(dir, 'gruntas')
    } catch {}
  }
  return 'gruntas'
}

/** Starts the SDK's Claude Code through gruntas on `account`; its stderr goes to `onStderr`. */
export function spawnOnAccount(account: string, o: SpawnRequest, onStderr: (d: string) => void) {
  const [file, ...args] = launcherArgv(account, o.args)
  const child = spawn(file!, args, { cwd: o.cwd, env: launcherEnv(o.env), signal: o.signal, stdio: ['pipe', 'pipe', 'pipe'] })
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', onStderr)
  return child
}

const VERSION = JSON.parse(readFileSync(new URL('../../package.json', import.meta.url), 'utf8')).version

/**
 * The environment the SDK gives Claude Code, before `launcherEnv`. Copied from
 * pi-claude-subscription 0.2.4's `childEnv` (MIT; THIRD-PARTY-NOTICES), less its removal of
 * API keys and cloud switches, which are the account's for gruntas to set.
 */
export function childEnv(base: Record<string, string | undefined>, spec: { noCache?: boolean; maxOutputTokens?: number }): Record<string, string | undefined> {
  return {
    ...base,
    // Declare this process honestly as an Agent SDK app, even when pi itself runs inside Claude Code.
    CLAUDECODE: undefined,
    CLAUDE_CODE_ENTRYPOINT: undefined,
    CLAUDE_AGENT_SDK_CLIENT_APP: `@gruntas/pi/${VERSION}`,
    // pi owns context management: no compaction, no output-size cap on tool results.
    DISABLE_AUTO_COMPACT: '1',
    DISABLE_COMPACT: '1',
    MAX_MCP_OUTPUT_TOKENS: '10000000',
    MCP_TOOL_TIMEOUT: String(7 * 24 * 3600 * 1000),
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1',
    CLAUDE_CODE_DISABLE_AUTO_MEMORY: '1',
    CLAUDE_CODE_DISABLE_CLAUDE_MDS: '1',
    ...(spec.noCache ? { DISABLE_PROMPT_CACHING: '1' } : {}),
    ...(spec.maxOutputTokens ? { CLAUDE_CODE_MAX_OUTPUT_TOKENS: String(spec.maxOutputTokens) } : {}),
  }
}
