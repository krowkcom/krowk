// The seam between pi and gruntas: what the extension sends `gruntas hook pi
// <event>` on stdin, and the one way it runs the hook.
//
// Every payload has Claude Code's shape (crates/gruntas-harness/tests/
// fixtures/hooks/pi/ holds one of each, and the tests hold these builders to
// them), so the hook treats a pi session as it does a Claude Code one: its
// give-up, logging, daemon start, heavy list, paste guard and report
// reminder are the hook's, not this file's.

import { spawn } from 'node:child_process'
import { accessSync, constants } from 'node:fs'
import { delimiter, join } from 'node:path'

/** The events `gruntas hook pi` takes, in Claude Code's names. */
export type HookEvent = 'SessionStart' | 'SessionEnd' | 'PreToolUse' | 'PostToolUse' | 'PostToolUseFailure' | 'Stop' | 'usage'

/** The argv of the hook for `event`. */
export function argv(event: HookEvent): string[] {
  return ['gruntas', 'hook', 'pi', event]
}

/** What every payload says of the session. */
export interface Session {
  /** pi's session id (`ctx.sessionManager.getSessionId()`). */
  id: string
  /** The session file, none for an ephemeral session. */
  file?: string
  cwd: string
}

/** A tool call, as pi's tool events carry it. */
export interface ToolCall {
  toolCallId: string
  toolName: string
  input: unknown
}

function common(s: Session) {
  return { session_id: s.id, transcript_path: s.file ?? null, cwd: s.cwd }
}

/** pi's tool name as Claude Code's: the paste guard reads `Bash` calls only. */
export function claudeToolName(name: string): string {
  return name === 'bash' ? 'Bash' : name
}

/** `session_start`'s payload; `source` is pi's reason (startup, resume, …). */
export function sessionStart(s: Session, source: string): string {
  return JSON.stringify({ ...common(s), hook_event_name: 'SessionStart', source })
}

/** `session_shutdown`'s payload; `reason` is pi's (quit, new, …). */
export function sessionEnd(s: Session, reason: string): string {
  return JSON.stringify({ ...common(s), hook_event_name: 'SessionEnd', reason })
}

/** A tool call's start (`PreToolUse`) or end (`PostToolUse`, or `PostToolUseFailure` for a failed one). */
export function toolEvent(event: 'PreToolUse' | 'PostToolUse' | 'PostToolUseFailure', s: Session, call: ToolCall): string {
  return JSON.stringify({ ...common(s), hook_event_name: event, tool_name: claudeToolName(call.toolName), tool_input: call.input, tool_use_id: call.toolCallId })
}

/** The end of a run; `active`: the run is already the continuation a Stop answer asked for. */
export function stop(s: Session, active: boolean): string {
  return JSON.stringify({ ...common(s), hook_event_name: 'Stop', stop_hook_active: active })
}

/** How the extension reaches the machine: one seam, so tests replace it. */
export interface Host {
  platform: string
  /** Whether `name` (`gruntas` when none) is on the PATH. */
  onPath(name?: string): boolean
  /** Runs `argv` with `stdin`; its stdout, or nothing when it could not run or took past `limitMs`. */
  run(argv: readonly string[], stdin: string, limitMs: number): Promise<string | undefined>
}

/** The most the extension waits on a hook it awaits: the hook gives up inside 1.5 s itself. */
export const LIMIT_MS = 2000

/** Whether `name` is an executable file in one of `path`'s directories. */
export function onPath(name: string, path: string | undefined): boolean {
  return (path ?? '').split(delimiter).some(dir => {
    if (!dir) return false
    try {
      accessSync(join(dir, name), constants.X_OK)
      return true
    } catch {
      return false
    }
  })
}

/**
 * Runs `argv` with no shell, so the hook's parent is pi, as the hook's
 * harness process must be. Never rejects: a missing binary, a failure or a
 * hook past `limitMs` (killed) is nothing.
 */
export function run(command: readonly string[], stdin: string, limitMs: number): Promise<string | undefined> {
  return new Promise(resolve => {
    const [file, ...args] = command
    if (!file) return resolve(undefined)
    let out = ''
    let done = false
    const finish = (v: string | undefined) => {
      if (done) return
      done = true
      clearTimeout(timer)
      resolve(v)
    }
    const child = spawn(file, args, { stdio: ['pipe', 'pipe', 'ignore'] })
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      // A grandchild may still hold the pipe: let go of it.
      child.stdout.destroy()
      child.unref()
      finish(undefined)
    }, limitMs)
    child.on('error', () => finish(undefined))
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (d: string) => (out += d))
    child.on('close', code => finish(code === 0 ? out : undefined))
    child.stdin.on('error', () => {})
    child.stdin.end(stdin)
  })
}

/** The machine as it is. */
export function systemHost(): Host {
  return { platform: process.platform, onPath: (name = 'gruntas') => onPath(name, process.env.PATH), run }
}

