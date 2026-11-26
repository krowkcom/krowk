// Copied from pi-claude-subscription 0.2.4 (MIT, Jarno Keskikangas; THIRD-PARTY-NOTICES), and
// changed to run Claude Code on a gruntas account with Claude Code's own system prompt.

import { randomUUID } from "node:crypto";
import { type Query, query, type SDKMessage, type SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import type { Tool, ToolResultMessage } from "@earendil-works/pi-ai";
import { type Account, childEnv, gruntasPath, spawnOnAccount } from "./launch.ts";
import { MCP_PREFIX, MCP_SERVER, toolUseId } from "./transcript.ts";
import { AsyncQueue, errorText } from "./util.ts";



export interface SessionSpec {
	cwd: string;
	model: string;
	systemPrompt: string;
	tools: Tool[];
	toolsHash: string;
	thinkingKey: string;
	thinking: { type: "adaptive" } | { type: "disabled" } | { type: "enabled"; budgetTokens: number };
	effort?: "low" | "medium" | "high" | "xhigh" | "max";
	maxOutputTokens?: number;
	/** pi asked for no prompt caching (cacheRetention "none"). */
	noCache?: boolean;
	/** Resume this Claude Code session, cut after `at` when given. Omit for a fresh session. */
	resume?: { sessionId: string; at?: string };
	/** The gruntas account whose Claude Code runs this session. */
	account: Account;
	debug?: (msg: string) => void;
}

type CallToolResult = { content: any[]; isError?: boolean };

/** What the provider needs from a Claude Code session; LiveSession is the real one. */
export interface ClaudeSession {
	readonly spec: SessionSpec;
	readonly sessionId: string;
	readonly systemPrompt: string;
	model: string;
	chain: string;
	count: number;
	awaiting: Set<string>;
	servedByClaude: Set<string>;
	dead: boolean;
	/** A provider call is reading this session's events; no other call may attach. */
	busy: boolean;
	lastUsed: number;
	readonly events: AsyncQueue<SDKMessage>;
	readonly exited: Promise<void>;
	readonly stderr: string;
	send(content: any[], priority?: "now" | "next" | "later"): void;
	deliver(result: ToolResultMessage): void;
	/** Transcript-entry uuid streamed for an API assistant message; waits briefly for its final SDK frame. */
	entryUuid(messageId: string): Promise<string | undefined>;
	setModel(model: string): Promise<void>;
	interrupt(): Promise<void>;
	close(): void;
}

/**
 * One Claude Code process in streaming-input mode, holding one conversation. pi owns the agent
 * loop: Claude Code's tool calls reach pi's tools through an in-process MCP server whose
 * handlers park until pi delivers the tool results on its next provider call.
 */
export class LiveSession implements ClaudeSession {
	readonly spec: SessionSpec;
	readonly sessionId: string;
	readonly systemPrompt: string;
	model: string;
	/** Chain hash and length of the pi transcript this process has consumed. */
	chain: string;
	count: number;
	/** Tool calls of the last step that pi has not answered yet. */
	awaiting = new Set<string>();
	/** Tool calls Claude Code answered itself (unknown tool names); pi's results for them are ignored. */
	servedByClaude = new Set<string>();
	dead = false;
	busy = false;
	lastUsed = Date.now();
	readonly events = new AsyncQueue<SDKMessage>();
	/** Resolves once the Claude Code process has exited. */
	readonly exited: Promise<void>;
	private markExited!: () => void;

	private readonly q: Query;
	private readonly input = new AsyncQueue<SDKUserMessage>();
	private readonly parked = new Map<string, { resolve: (r: CallToolResult) => void; reject: (e: Error) => void }>();
	private readonly ready = new Map<string, CallToolResult>();
	/** Last transcript-entry uuid streamed for each top-level API assistant message. */
	private readonly assistantEntries = new Map<string, string>();
	private readonly entryWaiters = new Map<string, Set<(uuid: string | undefined) => void>>();
	private stderrTail = "";

	constructor(spec: SessionSpec, chain: string, count: number) {
		this.spec = spec;
		this.model = spec.model;
		this.systemPrompt = spec.systemPrompt;
		this.chain = chain;
		this.count = count;
		this.sessionId = spec.resume?.sessionId ?? randomUUID();
		this.exited = new Promise((r) => {
			this.markExited = r;
		});

		const persist = spec.tools.length > 0;

		this.q = query({
			prompt: this.input,
			options: {
				cwd: spec.cwd,
				model: spec.model,
				// Claude Code's own prompt (D5): pi's travels as context, in the conversation's first
				// user turn (provider.ts), never as the system prompt.
				systemPrompt: { type: "preset", preset: "claude_code" },
				tools: [],
				mcpServers: spec.tools.length > 0 ? { [MCP_SERVER]: { type: "sdk", name: MCP_SERVER, instance: this.mcpServer(spec.tools) } } : {},
				allowedTools: spec.tools.map((t) => MCP_PREFIX + t.name),
				permissionMode: "bypassPermissions",
				allowDangerouslySkipPermissions: true,
				settingSources: [],
				strictMcpConfig: true,
				includePartialMessages: true,
				verbatimPrompts: true,
				persistSession: persist,
				title: "pi",
				thinking: spec.thinking,
				...(spec.effort ? { effort: spec.effort } : {}),
				...(spec.resume ? { resume: spec.resume.sessionId, ...(spec.resume.at ? { resumeSessionAt: spec.resume.at } : {}) } : { sessionId: this.sessionId }),
				// The account's own Claude Code, started by gruntas (`gruntas run claude --account <name>
				// --no-plugin`), which keeps the account's login or key: none passes through here.
				pathToClaudeCodeExecutable: gruntasPath(process.env.PATH),
				spawnClaudeCodeProcess: (o) =>
					spawnOnAccount(spec.account.name, o, (d) => {
						this.stderrTail = (this.stderrTail + d).slice(-2000);
					}),
				// pi's tools are an in-process server, which this does not touch; "argv" keeps any account's
				// Claude Code version working, where "initialize" needs the SDK's own or newer.
				mcpServersDelivery: "argv",
				env: childEnv(process.env, spec),
			},
		});
		void this.pump();
	}

	private mcpServer(tools: Tool[]): McpServer {
		const server = new McpServer({ name: MCP_SERVER, version: "1.0.0" }, { capabilities: { tools: {} } });
		const list = tools.map((t) => ({
			name: t.name,
			description: t.description,
			inputSchema: { type: "object", ...(JSON.parse(JSON.stringify(t.parameters ?? {})) as object) },
		}));
		server.server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: list as any }));
		server.server.setRequestHandler(CallToolRequestSchema, async (req) => {
			const id = (req.params as any)?._meta?.["claudecode/toolUseId"] as string | undefined;
			if (!id) return { content: [{ type: "text", text: "pi bridge: tool call without a tool_use id" }], isError: true };
			const done = this.ready.get(id);
			if (done) {
				this.ready.delete(id);
				return done as any;
			}
			return new Promise<CallToolResult>((resolve, reject) => this.parked.set(id, { resolve, reject })) as any;
		});
		return server;
	}

	private async pump(): Promise<void> {
		try {
			for await (const m of this.q) {
				const sdkMessage = m as any;
				if (sdkMessage.type === "assistant" && !sdkMessage.parent_tool_use_id && sdkMessage.message?.id && sdkMessage.uuid) this.rememberEntry(sdkMessage.message.id, sdkMessage.uuid);
				if (m.type === "user" && !m.parent_tool_use_id && Array.isArray(m.message.content)) {
					// A tool_result Claude Code produced without asking pi (e.g. an unknown tool name).
					for (const b of m.message.content as any[]) {
						if (b.type === "tool_result" && !this.parkedOrDelivered(b.tool_use_id)) this.servedByClaude.add(b.tool_use_id);
					}
				}
				this.events.push(m);
			}
		} catch (e) {
			this.events.push({ type: "pi_error", error: errorText(e, this.stderrTail) } as any);
		} finally {
			this.dead = true;
			for (const messageId of [...this.entryWaiters.keys()]) this.settleEntryWaiters(messageId, undefined);
			this.events.close();
			for (const p of this.parked.values()) p.reject(new Error("Claude Code session ended"));
			this.parked.clear();
			this.markExited();
		}
	}

	private rememberEntry(messageId: string, uuid: string): void {
		this.assistantEntries.set(messageId, uuid);
		this.settleEntryWaiters(messageId, uuid);
	}

	private settleEntryWaiters(messageId: string, uuid: string | undefined): void {
		const waiters = this.entryWaiters.get(messageId);
		if (!waiters) return;
		this.entryWaiters.delete(messageId);
		for (const resolve of waiters) resolve(uuid);
	}

	async entryUuid(messageId: string): Promise<string | undefined> {
		const known = this.assistantEntries.get(messageId);
		if (known || this.dead) return known;
		return new Promise((resolve) => {
			const done = (uuid: string | undefined) => {
				clearTimeout(timer);
				resolve(uuid ?? this.assistantEntries.get(messageId));
			};
			const timer = setTimeout(() => this.settleEntryWaiters(messageId, undefined), 500);
			timer.unref?.();
			const waiters = this.entryWaiters.get(messageId) ?? new Set<(uuid: string | undefined) => void>();
			waiters.add(done);
			this.entryWaiters.set(messageId, waiters);
		});
	}

	private delivered = new Set<string>();
	private parkedOrDelivered(id: string): boolean {
		return this.parked.has(id) || this.delivered.has(id) || this.ready.has(id);
	}

	/** Start a user turn (or continue after tool results when resuming). */
	send(content: any[], priority?: "now" | "next" | "later"): void {
		this.lastUsed = Date.now();
		this.input.push({
			type: "user",
			message: { role: "user", content },
			parent_tool_use_id: null,
			...(priority ? { priority } : {}),
		} as SDKUserMessage);
	}

	/** Answer a parked (or soon to be issued) MCP tool call with pi's result. */
	deliver(result: ToolResultMessage): void {
		const id = toolUseId(result.toolCallId);
		this.awaiting.delete(id);
		if (this.servedByClaude.has(id)) return;
		this.delivered.add(id);
		const r: CallToolResult = {
			content: result.content.map((c) => (c.type === "text" ? { type: "text", text: c.text } : { type: "image", data: c.data, mimeType: c.mimeType })),
			...(result.isError ? { isError: true } : {}),
		};
		if (r.content.length === 0) r.content.push({ type: "text", text: "(no output)" });
		const p = this.parked.get(id);
		if (p) {
			this.parked.delete(id);
			p.resolve(r);
		} else this.ready.set(id, r);
	}

	async setModel(model: string): Promise<void> {
		if (model === this.model) return;
		await this.q.setModel(model);
		this.model = model;
	}

	async interrupt(): Promise<void> {
		try {
			await this.q.interrupt();
		} catch {}
	}

	close(): void {
		if (this.dead) return;
		this.dead = true;
		this.input.close();
		try {
			this.q.close();
		} catch {}
	}

	get stderr(): string {
		return this.stderrTail;
	}
}
