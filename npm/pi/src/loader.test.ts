// The @gruntas/pi loader (./loader.ts). The package does not carry the extension: it runs the
// one the gruntas on the PATH carries (`gruntas pi extension --json` says where). Its cases,
// against a stand-in pi and host:
//
//   load(pi, host): Promise<void>
//   host = { platform, env, carried(): Promise<{ extension, skill } | undefined>, import(path), installerSkill?() }
//
// where carried() is undefined when gruntas is not on the PATH. The real host.import is a dynamic
// import() in the loader's own module, so pi's loader resolves it: only pi's loader gives the
// bundle the running pi's @earendil-works/* packages. The skill is added unless pi finds the
// installer's identical copy anyway (D8); the stand-in has none.

import assert from 'node:assert/strict'
import { test } from 'node:test'

const CARRIED = { extension: '/data/gruntas/pi/gruntas.js', skill: '/data/gruntas/pi/skills/gruntas' }

function standIn(opts: { env?: Record<string, string>; carried?: typeof CARRIED; platform?: string; installerSkill?: boolean } = {}) {
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
    ...(opts.installerSkill === undefined ? {} : { installerSkill: () => opts.installerSkill }),
  }
  const notes: string[] = []
  const emit = async (event: string, e: any = {}) => {
    const out = []
    for (const h of handlers[event] ?? []) out.push(await h(e, { ui: { notify: (m: string) => notes.push(m) } }))
    return out
  }
  return { pi, host, handlers, imported, ran, notes, emit, asked: () => asked }
}

test('with gruntas on the PATH, the loader runs the extension gruntas carries and adds its skill', async () => {
  const { load } = await import('./loader.ts' as string)
  const s = standIn({ carried: CARRIED })
  await load(s.pi, s.host)
  assert.deepEqual(s.imported, [CARRIED.extension])
  assert.deepEqual(s.ran, [s.pi], 'its default export, called with this pi, awaited')
  const [found] = await s.emit('resources_discover', { reason: 'startup' })
  assert.deepEqual((found as any)?.skillPaths, [CARRIED.skill], 'one gruntas skill, the carried one')
})

test('without gruntas on the PATH, it shows the install hint as a session starts and nothing else', async () => {
  const { load } = await import('./loader.ts' as string)
  const { INSTALL_HINT } = await import('./gruntas.ts')
  const s = standIn({ carried: undefined })
  await load(s.pi, s.host)
  assert.deepEqual(s.imported, [])
  await s.emit('session_start', { reason: 'startup' })
  assert.deepEqual(s.notes, [INSTALL_HINT])
  assert.deepEqual(Object.keys(s.handlers), ['session_start'])
})

test('under gruntas run pi (GRUNTAS_PI_LAUNCHED), and on Windows, it stands aside entirely', async () => {
  const { load } = await import('./loader.ts' as string)
  for (const s of [standIn({ env: { GRUNTAS_PI_LAUNCHED: '1' }, carried: CARRIED }), standIn({ platform: 'win32', carried: CARRIED })]) {
    await load(s.pi, s.host)
    assert.equal(s.asked(), 0, 'gruntas not even asked')
    assert.deepEqual([s.imported, Object.keys(s.handlers)], [[], []])
  }
})

test('where pi already finds the installer\'s gruntas skill, the loader adds no second one', async () => {
  const { load } = await import('./loader.ts' as string)
  const s = standIn({ carried: CARRIED, installerSkill: true })
  await load(s.pi, s.host)
  assert.deepEqual(s.ran, [s.pi])
  assert.deepEqual(Object.keys(s.handlers), [], 'no resources_discover: the installer\'s copy is the one skill')
})

test('a gruntas that cannot give the extension says so as a session starts, and nothing runs', async () => {
  const { load } = await import('./loader.ts' as string)
  const s = standIn()
  s.host.carried = async () => { throw new Error('unknown command: pi') }
  await load(s.pi, s.host)
  assert.deepEqual(s.imported, [])
  await s.emit('session_start', { reason: 'startup' })
  assert.equal(s.notes.length, 1)
  assert.match(s.notes[0], /unknown command: pi/)
})
