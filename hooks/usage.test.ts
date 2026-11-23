import { expect, test } from 'claude-code/testing'
import { configHome, payload } from './usage'

const LIMITS = [
  { kind: 'five_hour', percentUsed: 37.5, resetsAt: '2026-11-23T14:00:00.000Z' },
  { kind: 'seven_day', percentUsed: 12, resetsAt: '2026-11-28T09:00:00Z' },
  { kind: 'spend_limit', percentUsed: 120 },
]

test('the writer sends the 5-hour and 7-day windows with their resets, and nothing without them', () => {
  expect(JSON.parse(payload('/h/.claude', LIMITS) ?? 'null')).toEqual({
    configHome: '/h/.claude',
    windows: [
      { kind: 'fiveHour', usedPercent: 37.5, resetsAtMs: Date.UTC(2026, 10, 23, 14) },
      { kind: 'sevenDay', usedPercent: 12, resetsAtMs: Date.UTC(2026, 10, 28, 9) },
    ],
  })
  expect(JSON.parse(payload('/h/.claude', [{ kind: 'five_hour', percentUsed: 3, resetsAt: 'soon' }]) ?? 'null')).toEqual({ configHome: '/h/.claude', windows: [{ kind: 'fiveHour', usedPercent: 3 }] })
  expect(payload('/h/.claude', [])).toBeUndefined()
  expect(payload('/h/.claude', [{ kind: 'spend_limit', percentUsed: 5 }])).toBeUndefined()
})

test('the config home is CLAUDE_CONFIG_DIR, else ~/.claude', () => {
  expect(configHome('/work/.claude-b', '/home/ada')).toBe('/work/.claude-b')
  expect(configHome(undefined, '/home/ada/')).toBe('/home/ada/.claude')
  expect(configHome('', '/home/ada')).toBe('/home/ada/.claude')
  expect(configHome(undefined, undefined)).toBeUndefined()
})

test('a moved window runs `gruntas hook claude usage` with the windows on stdin', async ($, on) => {
  const ran: { argv: readonly string[]; stdin?: string }[] = []
  on('process.run', (_, e) => {
    ran.push({ argv: e.argv, stdin: e.init?.stdin })
    return { value: { exitCode: 0, stdout: '', stderr: '' } }
  })
  on('env.get', (_, e) => ({ value: e.name === 'CLAUDE_CONFIG_DIR' ? '/work/.claude-b' : '/home/ada' }))
  on('session.measure', (_, e) => ({ changed: e.changed }))
  const context = { size: 200_000 } as never
  await $.session.measure({ context, rateLimits: LIMITS, changed: ['context'] })
  expect(ran).toEqual([])
  await $.session.measure({ context, rateLimits: LIMITS, changed: ['rateLimits'] })
  expect(ran.map(r => r.argv)).toEqual([['gruntas', 'hook', 'claude', 'usage']])
  expect(JSON.parse(ran[0]?.stdin ?? 'null').configHome).toBe('/work/.claude-b')
})

test('no gruntas on the PATH leaves the session as it was', async ($, on) => {
  on('process.run', () => {
    throw new Error('spawn gruntas ENOENT')
  })
  on('env.get', () => ({ value: '/home/ada' }))
  on('session.measure', (_, e) => ({ changed: e.changed }))
  const context = { size: 200_000 } as never
  await expect($.session.measure({ context, rateLimits: LIMITS, changed: ['rateLimits'] })).resolves.toEqual({ changed: ['rateLimits'] })
})
