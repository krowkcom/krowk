// What the provider takes from a real pi and the Agent SDK, loaded only inside pi: the tests run
// the rest on bare node, without these packages.

import { calculateCost, createAssistantMessageEventStream } from '@earendil-works/pi-ai'
import type { Runtime } from './extension.ts'
import { LiveSession } from './session.ts'

export const runtime: Runtime = {
  ai: { createAssistantMessageEventStream, calculateCost: calculateCost as never },
  createSession: (spec, chain, count) => new LiveSession(spec, chain, count),
}
