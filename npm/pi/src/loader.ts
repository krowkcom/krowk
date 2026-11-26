// @gruntas/pi, the package: a loader for the pi extension the gruntas on the
// PATH carries. It asks gruntas where that extension is (`gruntas pi extension
// --json` writes it and the gruntas skill into gruntas's own directory), imports
// it and runs it with this pi, so pi with the package and `gruntas run pi` run
// the same code, at the installed gruntas's version. pi resolves the
// extension's own imports of pi's packages to the running pi, as it does this
// file's.
//
// Without gruntas on the PATH it shows the install hint as a session starts and
// does nothing else. On Windows, where gruntas's hooks and daemon do not run,
// and under `gruntas run pi`, which loads the extension itself, it does nothing.
// `gruntas run pi` becomes pi (exec), so it marks the run with its own pid
// (GRUNTAS_PI_LAUNCHED=<pid>): a pi started from inside that session (an
// agent's bash call) inherits the mark but runs under another pid, and loads
// gruntas as usual.
//
// This file is all the package ships: it imports nothing of the extension's.

import { execFile } from 'node:child_process'
import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent'

/** What the session-start hint says when gruntas is not on the PATH (the extension's INSTALL_HINT, held equal by loader.test.ts). */
export const INSTALL_HINT = 'gruntas is not on the PATH, so this session is not registered with it. Install it: curl -fsSL https://gruntas.com/install | bash'

/** Where gruntas wrote what it carries. */
export type Carried = { extension: string; skill: string }

export type LoaderHost = {
  platform: string
  env: Record<string, string | undefined>
  /** This pi's process id: `gruntas run pi`'s mark names it. */
  pid: number
  /** The carried files, written; undefined when gruntas is not on the PATH. Throws when gruntas cannot give them. */
  carried(): Promise<Carried | undefined>
  import(path: string): Promise<{ default: (pi: ExtensionAPI) => unknown }>
  /** Whether pi finds the installer's gruntas skill (`~/.agents/skills/gruntas`) on its own. */
  installerSkill?(): boolean
}

type On = (event: string, handler: (event: any, ctx: any) => unknown) => void

/** The loader, on `host`. */
export async function load(pi: ExtensionAPI, host: LoaderHost): Promise<void> {
  if (host.platform === 'win32' || host.env.GRUNTAS_PI_LAUNCHED === String(host.pid)) return
  const on = pi.on.bind(pi) as On
  let carried: Carried | undefined
  try {
    carried = await host.carried()
  } catch (e) {
    const why = e instanceof Error ? e.message : String(e)
    on('session_start', (_e, ctx) => ctx.ui.notify(`gruntas could not give pi its extension: ${why}`, 'warning'))
    return
  }
  if (!carried) {
    on('session_start', (_e, ctx) => ctx.ui.notify(INSTALL_HINT, 'warning'))
    return
  }
  // One gruntas skill: the installer's copy is the same file, and pi warns of
  // a collision when it finds two.
  if (!host.installerSkill?.()) {
    const skill = carried.skill
    on('resources_discover', () => ({ skillPaths: [skill] }))
  }
  const extension = await host.import(carried.extension)
  await extension.default(pi)
}

/** Why `gruntas pi extension --json` gave nothing, from the JSON error gruntas writes to stderr. */
function failure(err: (Error & { killed?: boolean }) | null, stderr: string): string {
  let e: any
  try {
    e = JSON.parse(stderr)?.error
  } catch {}
  if (e?.error === 'unknown_command') return 'this gruntas is too old to carry it. Upgrade it: curl -fsSL https://gruntas.com/install | bash'
  if (typeof e?.fix === 'string' && e.fix) return e.fix
  if (err?.killed) return '`gruntas pi extension` did not answer in time'
  return stderr.trim().split('\n')[0] || '`gruntas pi extension --json` said nothing usable'
}

/** Runs `gruntas pi extension --json`: undefined when there is no gruntas to run. */
export function carried(): Promise<Carried | undefined> {
  return new Promise((resolve, reject) => {
    execFile('gruntas', ['pi', 'extension', '--json'], { timeout: 30_000 }, (err, stdout, stderr) => {
      if ((err as NodeJS.ErrnoException | null)?.code === 'ENOENT') return resolve(undefined)
      let data: any
      try {
        data = JSON.parse(stdout)?.data
      } catch {}
      if (!err && typeof data?.extension === 'string' && typeof data?.skill === 'string') return resolve({ extension: data.extension, skill: data.skill })
      reject(new Error(failure(err, String(stderr))))
    })
  })
}

export function systemHost(): LoaderHost {
  return {
    platform: process.platform,
    env: process.env,
    pid: process.pid,
    carried,
    import: path => import(path),
    installerSkill: () => existsSync(join(homedir(), '.agents', 'skills', 'gruntas', 'SKILL.md')),
  }
}

export default async function gruntas(pi: ExtensionAPI): Promise<void> {
  await load(pi, systemHost())
}
