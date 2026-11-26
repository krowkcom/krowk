// What the provider takes from a real pi and the Agent SDK, loaded only inside pi: the tests run
// the rest on bare node, without these packages.

import { calculateCost, createAssistantMessageEventStream } from '@earendil-works/pi-ai'
import type { Runtime } from './extension.ts'
import { LiveSession } from './session.ts'

/** What to do when the Agent SDK is not there: said as a session starts and by each Claude turn. */
export const SDK_MISSING = 'the Claude Agent SDK is not installed beside this extension. gruntas fetches it from npm the first time it writes the extension: install npm (it comes with Node.js), run `gruntas pi extension`, then restart pi'

/**
 * The runtime, with the Agent SDK that gruntas installs beside the carried extension (gruntas
 * never ships the SDK). Without it the provider still registers, so its models are listed, and
 * each turn fails saying how to fix it; `missing` is that fix, for the session start.
 */
export async function load(): Promise<{ runtime: Runtime; missing?: string }> {
  const ai: Runtime['ai'] = { createAssistantMessageEventStream, calculateCost: calculateCost as never }
  let sdk: typeof import('@anthropic-ai/claude-agent-sdk')
  try {
    sdk = await import('@anthropic-ai/claude-agent-sdk')
  } catch (e) {
    // Missing is not-found naming the SDK itself; one of its own imports missing is a broken install.
    const code = (e as { code?: string })?.code
    const missing = (code === 'ERR_MODULE_NOT_FOUND' || code === 'MODULE_NOT_FOUND' || /cannot find/i.test(String(e))) && /@anthropic-ai\/claude-agent-sdk['"`]/.test(String(e))
    const why = missing ? SDK_MISSING : `the Claude Agent SDK beside this extension could not load: ${e instanceof Error ? e.message : String(e)}`
    return { runtime: { ai, createSession: () => { throw new Error(`gruntas: ${why}`) } }, missing: why }
  }
  return { runtime: { ai, createSession: (spec, chain, count) => new LiveSession(spec, chain, count, sdk.query) } }
}
