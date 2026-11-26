#!/usr/bin/env node
// The carried extension: npm/pi's source bundled into one JavaScript file,
// npm/pi/carried/gruntas.js, which the full gruntas build embeds and
// `gruntas pi extension` writes out for pi to load.
//
// It inlines what the extension uses of the Claude Agent SDK and the MCP SDK
// (THIRD-PARTY-NOTICES), and leaves out the SDK's bundled Claude Code: its
// Claude Code is started by gruntas (`gruntas run claude --no-plugin`). pi's
// own packages (@earendil-works/*) stay external, so they are the running
// pi's.
//
// The file is checked in, so a plain `cargo build` needs no Node. Its first
// line records a hash of what it was built from and of its own body, and
// `--check` (part of `make pi-test`) fails when either no longer matches.
//
//   make pi-bundle                  rebuild (npm ci in npm/pi first)
//   node npm/pi/bundle.mjs --check  is the checked-in file current?

import { createHash } from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const OUT = path.join(here, 'carried', 'gruntas.js')
/** The @gruntas/pi package's loader, which runs this bundle and is no part of it. */
const LOADER = path.join(here, 'src', 'loader.ts')

/**
 * The version the bundle reports to Claude Code (launch.ts's VERSION). gruntas
 * puts its own in place of this token as it writes the file, so the file in
 * the repository stays the same from one release to the next.
 */
const VERSION_TOKEN = '0.0.0-gruntas-version'

const HEADER = /^\/\/ @gruntas\/pi carried extension: sources sha256:([0-9a-f]{64}) body sha256:([0-9a-f]{64})\. Built by `make pi-bundle`; do not edit\.\n/

const sha256 = data => createHash('sha256').update(data).digest('hex')

/** Everything the bundle is built from: the extension's source (not its tests, nor the package's loader), its package.json, the lockfile and this script. */
function sourcesHash() {
  const files = []
  const walk = dir => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name)
      if (e.isDirectory()) walk(p)
      else if (!e.name.endsWith('.test.ts') && p !== LOADER) files.push(p)
    }
  }
  walk(path.join(here, 'src'))
  files.push(path.join(here, 'package.json'), path.join(here, 'package-lock.json'), fileURLToPath(import.meta.url))
  const h = createHash('sha256')
  for (const f of files.map(f => path.relative(here, f).split(path.sep).join('/')).sort()) {
    h.update(f + '\0').update(fs.readFileSync(path.join(here, f))).update('\0')
  }
  return h.digest('hex')
}

function check() {
  let text
  try {
    text = fs.readFileSync(OUT, 'utf8')
  } catch {
    return 'npm/pi/carried/gruntas.js is missing'
  }
  const m = HEADER.exec(text)
  if (!m) return 'npm/pi/carried/gruntas.js has no header saying what it was built from'
  if (m[2] !== sha256(text.slice(m[0].length))) return 'npm/pi/carried/gruntas.js was edited by hand'
  if (m[1] !== sourcesHash()) return 'npm/pi/carried/gruntas.js is stale against npm/pi\'s source'
  return undefined
}

/**
 * The header vouches for package-lock.json, so the bundle must be built from what it pins:
 * node_modules as `npm ci` left it, no package at another version.
 */
function installedAsLocked() {
  const read = f => JSON.parse(fs.readFileSync(path.join(here, f), 'utf8')).packages ?? {}
  let installed
  try {
    installed = read('node_modules/.package-lock.json')
  } catch {
    return 'node_modules is missing'
  }
  const locked = read('package-lock.json')
  for (const [name, p] of Object.entries(installed)) {
    if (locked[name]?.version !== p.version || locked[name]?.integrity !== p.integrity) return `${name} is not the version package-lock.json pins`
  }
  return undefined
}

async function build() {
  const off = installedAsLocked()
  if (off) throw new Error(`${off}: run \`make pi-bundle\`, which runs \`npm ci\` first`)
  let esbuild
  try {
    esbuild = await import('esbuild')
  } catch {
    throw new Error('esbuild is not installed: run `npm ci` in npm/pi first (make pi-bundle does)')
  }
  const result = await esbuild.build({
    absWorkingDir: here,
    entryPoints: ['src/gruntas.ts'],
    bundle: true,
    platform: 'node',
    format: 'esm',
    target: 'node22',
    external: ['@earendil-works/*'],
    define: { GRUNTAS_PI_VERSION: JSON.stringify(VERSION_TOKEN) },
    legalComments: 'eof',
    charset: 'utf8',
    write: false,
    logLevel: 'warning',
  })
  const body = result.outputFiles[0].text
  if (!body.includes(VERSION_TOKEN)) throw new Error('the bundle lost the version token gruntas stamps')
  const header = `// @gruntas/pi carried extension: sources sha256:${sourcesHash()} body sha256:${sha256(body)}. Built by \`make pi-bundle\`; do not edit.\n`
  fs.mkdirSync(path.dirname(OUT), { recursive: true })
  fs.writeFileSync(OUT, header + body)
  process.stdout.write(`npm/pi/bundle.mjs: wrote npm/pi/carried/gruntas.js (${Buffer.byteLength(header + body)} bytes)\n`)
}

if (process.argv.includes('--check')) {
  const why = check()
  if (why) {
    process.stderr.write(`npm/pi/bundle.mjs: ${why}. Rebuild it with \`make pi-bundle\` and commit it.\n`)
    process.exit(1)
  }
} else {
  await build().catch(e => {
    process.stderr.write(`npm/pi/bundle.mjs: ${e.message}\n`)
    process.exit(1)
  })
}
