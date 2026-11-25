// Copied from pi-claude-subscription 0.2.4 (MIT, Jarno Keskikangas; THIRD-PARTY-NOTICES), its
// cc-sessions.ts, with the config home a parameter: each gruntas account has its own.

import { randomUUID } from "node:crypto";
import { mkdirSync, realpathSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import { assistantBlocks, type TurnMessage, userTurnBlocks } from "./transcript.ts";

/** Claude Code's per-project session directory under `home`: the working directory's realpath, sanitized. */
export function projectDir(home: string, cwd: string): string {
	let real = cwd;
	try {
		real = realpathSync(cwd);
	} catch {}
	return join(home, "projects", real.replace(/[^a-zA-Z0-9]/g, "-"));
}

/**
 * Write a Claude Code transcript, under the Claude Code config home `home` of the account that
 * resumes it, for history that never ran through that account's Claude Code: another provider's
 * turns, another account's (a move), a compacted session, a session from before this extension
 * or before pi restarted. Returns the session id and the uuid to resume at. `messages` must end
 * with an assistant message.
 */
export function synthesizeSession(home: string, cwd: string, model: string, messages: TurnMessage[]): { sessionId: string; at: string } {
	const sessionId = randomUUID();
	const lines: string[] = [];
	let parent: string | null = null;
	const now = Date.now();
	const common = { isSidechain: false, userType: "external", entrypoint: "sdk-cli", cwd, sessionId };
	// Thinking signatures are only needed (and only checked) inside the trailing tool loop;
	// earlier thinking is dropped by the API anyway.
	let lastUserText = -1;
	messages.forEach((m, i) => {
		if (m.role === "user") lastUserText = i;
	});
	let i = 0;
	let n = 0;
	while (i < messages.length) {
		const uuid = randomUUID();
		const timestamp = new Date(now - (messages.length - i) * 1000).toISOString();
		const m = messages[i];
		if (m.role === "assistant") {
			const keepThinking = i > lastUserText && m.model === model && (m.api === "claude-agent-sdk" || m.api === "anthropic-messages");
			const content = assistantBlocks(m as AssistantMessage, keepThinking);
			lines.push(
				JSON.stringify({
					parentUuid: parent,
					...common,
					type: "assistant",
					message: {
						id: `msg_pi_${(n++).toString(36)}_${sessionId.slice(0, 8)}`,
						type: "message",
						role: "assistant",
						model,
						content: content.length > 0 ? content : [{ type: "text", text: "(empty)" }],
						stop_reason: content.some((c) => c.type === "tool_use") ? "tool_use" : "end_turn",
						stop_sequence: null,
						usage: { input_tokens: 0, output_tokens: 0 },
					},
					uuid,
					timestamp,
				}),
			);
			i++;
		} else {
			let j = i;
			while (j < messages.length && messages[j].role !== "assistant") j++;
			const content = userTurnBlocks(messages.slice(i, j));
			lines.push(
				JSON.stringify({
					parentUuid: parent,
					...common,
					type: "user",
					message: { role: "user", content: content.length > 0 ? content : [{ type: "text", text: "(empty)" }] },
					uuid,
					timestamp,
				}),
			);
			i = j;
		}
		parent = uuid;
	}
	const dir = projectDir(home, cwd);
	mkdirSync(dir, { recursive: true });
	writeFileSync(join(dir, `${sessionId}.jsonl`), `${lines.join("\n")}\n`);
	return { sessionId, at: parent! };
}
