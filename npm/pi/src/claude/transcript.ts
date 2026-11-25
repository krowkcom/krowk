// Copied from pi-claude-subscription 0.2.4 (MIT, Jarno Keskikangas; THIRD-PARTY-NOTICES):
// pi's transcript as Claude Code takes it.

import type {
	AssistantMessage,
	ImageContent,
	Message,
	SystemMessage,
	TextContent,
	Tool,
	ToolResultMessage,
	TranscriptContext,
	UserMessage,
} from "@earendil-works/pi-ai";
import { sha, stableStringify } from "./util.ts";

// ---- pi's transcript, read as pi-ai reads it -----------------------------------------------
// pi-ai's own helpers, restated so this file loads without pi's packages (the tests run it on
// bare node). They follow pi-ai 1.6's utils/transcript.ts and utils/text.ts.

type TranscriptEntry = { role: string };

function contentText(content: string | TextContent[]): string {
	return typeof content === "string" ? content : content.filter((b) => b.type === "text").map((b) => b.text).join("\n");
}

function getInitialSystemMessage(messages: readonly TranscriptEntry[]): SystemMessage | undefined {
	const first = messages[0];
	return first?.role === "system" ? (first as SystemMessage) : undefined;
}

function getSystemMessageText(m: SystemMessage): string {
	const parts = [contentText(m.content)];
	for (const text of Object.values(m.sections ?? {})) if (text !== null) parts.push(text);
	return parts.filter((p) => p.length > 0).join("\n\n");
}

function renderSystemMessageUpdate(m: SystemMessage): string {
	const parts: string[] = [];
	const text = contentText(m.content);
	if (text.length > 0) parts.push(text);
	for (const [name, value] of Object.entries(m.sections ?? {})) {
		parts.push(value === null ? `Removed system prompt section "${name}".` : `Updated system prompt section "${name}":\n\n${value}`);
	}
	return parts.join("\n\n");
}

function getCurrentTools(messages: readonly TranscriptEntry[]): Tool[] {
	const tools = new Map<string, Tool>();
	for (const m of messages) {
		if (m.role !== "system") continue;
		for (const t of (m as SystemMessage).toolsRemoved ?? []) tools.delete(t.name);
		for (const t of (m as SystemMessage).toolsAdded ?? []) tools.set(t.name, t);
	}
	return [...tools.values()];
}

function toToolDeclaration(tool: Tool): Tool {
	return {
		name: tool.name,
		description: tool.description,
		parameters: JSON.parse(JSON.stringify(tool.parameters)),
		...(tool.constrainedSampling === undefined ? {} : { constrainedSampling: tool.constrainedSampling }),
	};
}

/** The pi API of the `gruntas` provider's models, which its own turns carry. */
export const API = "gruntas-claude-code";

export const MCP_SERVER = "pi";
export const MCP_PREFIX = `mcp__${MCP_SERVER}__`;

/** A transcript message the provider consumes: everything but the leading system message. */
export type TurnMessage = UserMessage | AssistantMessage | ToolResultMessage | SystemMessage;

export interface Transcript {
	/** Leading system prompt text; fixed for the lifetime of a Claude Code session. */
	systemPrompt: string;
	/** Tools in effect for this request (after every transcript delta). */
	tools: Tool[];
	toolsHash: string;
	messages: TurnMessage[];
	/** chain[i] identifies the conversation state after messages[0..i). chain[0] is the prompt alone. */
	chain: string[];
}

/**
 * Prepare pi's transcript for Claude Code. Failed or aborted assistant turns are dropped, as pi's
 * built-in Anthropic provider does, together with tool results that answer their calls.
 */
export function readTranscript(context: TranscriptContext): Transcript {
	const leading = getInitialSystemMessage(context.messages);
	const systemPrompt = leading ? getSystemMessageText(leading) : "";
	const tools = getCurrentTools(context.messages).map(toToolDeclaration);
	const messages: TurnMessage[] = [];
	const droppedCalls = new Set<string>();
	for (const m of context.messages) {
		if (m === leading) continue;
		if (m.role === "assistant" && (m.stopReason === "error" || m.stopReason === "aborted")) {
			for (const c of m.content) if (c.type === "toolCall") droppedCalls.add(c.id);
			continue;
		}
		if (m.role === "toolResult" && droppedCalls.has(m.toolCallId)) continue;
		messages.push(m as TurnMessage);
	}
	const chain = [sha(`prompt\0${systemPrompt}`)];
	for (const m of messages) chain.push(sha(`${chain[chain.length - 1]}\0${fingerprint(m)}`));
	return { systemPrompt, tools, toolsHash: sha(stableStringify(tools)), messages, chain };
}

/** Content identity of a message: what the model sees, without timestamps, usage or ids pi adds. */
export function fingerprint(m: TurnMessage): string {
	switch (m.role) {
		case "user":
			return stableStringify(["u", normContent(m.content)]);
		case "toolResult":
			return stableStringify(["r", m.toolCallId, normContent(m.content), !!m.isError]);
		case "system":
			return stableStringify(["s", renderSystemMessageUpdate(m)]);
		case "assistant":
			return stableStringify([
				"a",
				m.content
					.filter((c) => !(c.type === "text" && c.text.length === 0))
					.map((c) =>
						c.type === "text"
							? ["t", c.text]
							: c.type === "thinking"
								? ["k", c.thinking, c.thinkingSignature ?? "", !!c.redacted]
								: ["c", c.id, c.name, c.arguments],
					),
			]);
	}
}

function normContent(content: string | (TextContent | ImageContent)[]): unknown {
	if (typeof content === "string") return [["t", content]];
	return content.map((c) => (c.type === "text" ? ["t", c.text] : ["i", c.mimeType, sha(c.data)]));
}

// ---- pi → Anthropic content -------------------------------------------------------------

const ID_RE = /^[a-zA-Z0-9_-]{1,64}$/;
/** Anthropic tool_use ids must match ^[a-zA-Z0-9_-]+$; ids from other providers may not. */
export function toolUseId(id: string): string {
	return ID_RE.test(id) ? id : `toolu_x${sha(id).slice(0, 24)}`;
}

export function toImageBlock(c: ImageContent) {
	return { type: "image" as const, source: { type: "base64" as const, media_type: c.mimeType, data: c.data } };
}

function userBlocks(content: string | (TextContent | ImageContent)[]): any[] {
	if (typeof content === "string") return content.length > 0 ? [{ type: "text", text: content }] : [];
	return content
		.filter((c) => c.type !== "text" || c.text.length > 0)
		.map((c) => (c.type === "text" ? { type: "text", text: c.text } : toImageBlock(c)));
}

export function toolResultBlock(m: ToolResultMessage): any {
	const content = m.content.map((c) => (c.type === "text" ? { type: "text", text: c.text } : toImageBlock(c)));
	return {
		type: "tool_result",
		tool_use_id: toolUseId(m.toolCallId),
		content: content.length > 0 ? content : [{ type: "text", text: "(no output)" }],
		...(m.isError ? { is_error: true } : {}),
	};
}

/** A later system message (prompt or section change), delivered as a system reminder in the user turn. */
export function systemUpdateText(m: SystemMessage): string | undefined {
	const text = renderSystemMessageUpdate(m).trim();
	return text ? `<system-reminder>\n${text}\n</system-reminder>` : undefined;
}

/**
 * Render consecutive non-assistant messages as one Anthropic user message. Tool results come
 * first, as the API requires, then reminders and user text in transcript order.
 */
export function userTurnBlocks(tail: TurnMessage[]): any[] {
	const results: any[] = [];
	const rest: any[] = [];
	for (const m of tail) {
		if (m.role === "toolResult") results.push(toolResultBlock(m));
		else if (m.role === "user") rest.push(...userBlocks(m.content));
		else if (m.role === "system") {
			const text = systemUpdateText(m);
			if (text) rest.push({ type: "text", text });
		}
	}
	return [...results, ...rest];
}

/** Assistant content for a synthesized Claude Code transcript. */
export function assistantBlocks(m: AssistantMessage, keepThinking: boolean): any[] {
	const blocks: any[] = [];
	for (const c of m.content) {
		if (c.type === "text") {
			if (c.text.length > 0) blocks.push({ type: "text", text: c.text });
		} else if (c.type === "thinking") {
			if (!keepThinking || !c.thinkingSignature) continue;
			if (c.redacted) blocks.push({ type: "redacted_thinking", data: c.thinkingSignature });
			else blocks.push({ type: "thinking", thinking: c.thinking, signature: c.thinkingSignature });
		} else if (c.type === "toolCall") {
			blocks.push({ type: "tool_use", id: toolUseId(c.id), name: MCP_PREFIX + c.name, input: c.arguments ?? {} });
		}
	}
	return blocks;
}

/** Map a tool_use name emitted by Claude back to pi's tool name. */
export function piToolName(name: string): string {
	return name.startsWith(MCP_PREFIX) ? name.slice(MCP_PREFIX.length) : name;
}

export type { Message };
