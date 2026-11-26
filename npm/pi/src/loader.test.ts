// Pending: the @gruntas/pi loader (ticket 03). The package stops carrying the extension and runs
// the one the gruntas on the PATH carries (`gruntas pi extension --json` says where). Its three
// cases, against a stand-in pi and host. ./loader.ts is written by ticket 03; its seam:
//
//   load(pi, host): Promise<void>
//   host = { platform, env, carried(): Promise<{ extension, skill } | undefined>, import(path) }
//
// where carried() is undefined when gruntas is not on the PATH.

import assert from 'node:assert/strict'
import { test } from 'node:test'

const PENDING = { skip: 'pending: the @gruntas/pi loader (ticket 03)' }
const CARRIED = { extension: '/data/gruntas/pi/gruntas.js', skill: '/data/gruntas/pi/skills/gruntas' }

function standIn(opts: { env?: Record<string, string>; carried?: typeof CARRIED; platform?: string } = {}) {
  const handlers: Record<string, ((e: any, ctx: any) => unknown)[]> = {}
  const pi = { on: (event: string, h: any) => void (handlers[event] ??= []).push(h) }
  const imported: string[] = []
  const ran: unknown[] = []
  let asked = 0
  const host = {
    platform: opts.platform ?? 'linux',
    env: opts.env ?? {},
    carried: async () => (asked++, opts.carried),
    import: async (path: string) => (imported.push(path), { default: async (p: unknown) => void ran.push(p) }),
  }
  const notes: string[] = []
  const emit = async (event: string, e: any = {}) => {
    const out = []
    for (const h of handlers[event] ?? []) out.push(await h(e, { ui: { notify: (m: string) => notes.push(m) } }))
    return out
  }
  return { pi, host, handlers, imported, ran, notes, emit, asked: () => asked }
}

test('with gruntas on the PATH, the loader runs the extension gruntas carries and adds its skill', PENDING, async () => {
  const { load } = await import('./loader.ts' as string)
  const s = standIn({ carried: CARRIED })
  await load(s.pi, s.host)
  assert.deepEqual(s.imported, [CARRIED.extension])
  assert.deepEqual(s.ran, [s.pi], 'its default export, called with this pi, awaited')
  const [found] = await s.emit('resources_discover', { reason: 'startup' })
  assert.deepEqual((found as any)?.skillPaths, [CARRIED.skill], 'one gruntas skill, the carried one')
})

test('without gruntas on the PATH, it shows the install hint as a session starts and nothing else', PENDING, async () => {
  const { load } = await import('./loader.ts' as string)
  const { INSTALL_HINT } = await import('./gruntas.ts')
  const s = standIn({ carried: undefined })
  await load(s.pi, s.host)
  assert.deepEqual(s.imported, [])
  await s.emit('session_start', { reason: 'startup' })
  assert.deepEqual(s.notes, [INSTALL_HINT])
  assert.deepEqual(Object.keys(s.handlers), ['session_start'])
})

test('under gruntas run pi (GRUNTAS_PI_LAUNCHED), and on Windows, it stands aside entirely', PENDING, async () => {
  const { load } = await import('./loader.ts' as string)
  for (const s of [standIn({ env: { GRUNTAS_PI_LAUNCHED: '1' }, carried: CARRIED }), standIn({ platform: 'win32', carried: CARRIED })]) {
    await load(s.pi, s.host)
    assert.equal(s.asked(), 0, 'gruntas not even asked')
    assert.deepEqual([s.imported, Object.keys(s.handlers)], [[], []])
  }
})
