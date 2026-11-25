// Copied from pi-claude-subscription 0.2.4 (MIT, Jarno Keskikangas; THIRD-PARTY-NOTICES), and
// changed to run each request on the session's gruntas account.

import type {
	Api,
	AssistantMessage,
	AssistantMessageEventStream,
	Model,
	SimpleStreamOptions,
	StopReason,
	TextContent,
	ThinkingContent,
	ToolCall,
	ToolResultMessage,
	TranscriptContext,
} from "@earendil-works/pi-ai";
import type { Account } from "./launch.ts";
import { thinkingOptions } from "./models.ts";
import type { ClaudeSession, SessionSpec } from "./session.ts";
import { synthesizeSession } from "./synth.ts";
import { piToolName, readTranscript, type Transcript, type TurnMessage, userTurnBlocks, fingerprint } from "./transcript.ts";
import { errorText, sha } from "./util.ts";

export const API = "gruntas-claude-code";

export interface ProviderOptions {
	/** Live Claude Code processes kept for reuse (one per concurrent conversation). */
	maxSessions?: number;
	/** Close a process idle this long; the conversation is written afresh on its next turn. */
	idleMs?: number;
	debug?: (msg: string) => void;
	cwd?: () => string;
	/** The account the next request runs on. */
	account: () => Promise<Account>;
	/** Creates the Claude Code session (session.ts's LiveSession); tests substitute a scripted fake. */
	createSession: (spec: SessionSpec, chain: string, count: number) => ClaudeSession;
	/** pi-ai's stream and cost, handed in so this file loads without pi's packages. */
	ai: { createAssistantMessageEventStream: () => AssistantMessageEventStream; calculateCost: (model: Model<Api>, usage: AssistantMessage["usage"]) => AssistantMessage["usage"]["cost"] };
	/** Each usage-limit reading Claude Code reports, with the account it is for. */
	onRateLimit?: (account: Account, info: unknown) => void;
}

/**
 * pi's system prompt, as context in the conversation's first user turn: the system prompt is
 * Claude Code's own (D5).
 */
export function contextMessage(systemPrompt: string): TurnMessage {
	const text = `<system-reminder>\nYou are working in pi, a terminal coding agent, which runs your turns through Claude Code: pi's tools are yours, named mcp__pi__<tool>, and Claude Code's own are off. pi's instructions for this session follow, and take precedence over Claude Code's where they differ.\n\n<pi-instructions>\n${systemPrompt}\n</pi-instructions>\n</system-reminder>`;
	return { role: "user", content: [{ type: "text", text }], timestamp: 0 };
}

/** How a request was attached to Claude Code; exposed for tests and the debug log. */
export type Route = "live" | "synth" | "fresh";

export class ClaudeSdkProvider {
	private sessions: ClaudeSession[] = [];
	/** Every session whose process may still run, pooled or not: the leak backstop closes from here. */
	private readonly all = new Set<ClaudeSession>();
	/** Sessions a provider call is reading right now (not the `busy` flag, which a failed attach could leave set). */
	private readonly active = new Set<ClaudeSession>();
	private readonly opts: Required<Pick<ProviderOptions, "maxSessions" | "idleMs" | "cwd">> & ProviderOptions;
	private timer?: NodeJS.Timeout;
	lastRoute?: Route;

	constructor(opts: ProviderOptions) {
		// An option explicitly set to undefined means "not configured": it must not overwrite a
		// default. (The extension once passed idleMs: undefined that way, and every idle-sweep
		// comparison against undefined is false, so the sweep closed in-flight sessions.)
		const given = Object.fromEntries(Object.entries(opts).filter(([, v]) => v !== undefined));
		this.opts = { maxSessions: 3, idleMs: 20 * 60_000, cwd: () => process.cwd(), ...opts, ...(given as Partial<ProviderOptions>) };
	}

	readonly streamSimple = (model: Model<Api>, context: TranscriptContext, options?: SimpleStreamOptions): AssistantMessageEventStream => {
		const stream = this.opts.ai.createAssistantMessageEventStream();
		const output: AssistantMessage = {
			role: "assistant",
			content: [],
			api: model.api,
			provider: model.provider,
			model: model.id,
			usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
			stopReason: "pending",
			timestamp: Date.now(),
		};
		void this.run(model, context, options, stream, output);
		return stream;
	};

	private log(msg: string): void {
		this.opts.debug?.(msg);
	}

	private async run(model: Model<Api>, context: TranscriptContext, options: SimpleStreamOptions | undefined, stream: AssistantMessageEventStream, output: AssistantMessage): Promise<void> {
		let session: ClaudeSession | undefined;
		let started = false;
		const fail = (reason: "error" | "aborted", message: string) => {
			output.stopReason = reason;
			output.errorMessage = message;
			if (!started) stream.push({ type: "start", partial: output });
			stream.push({ type: "error", reason, error: output });
			stream.end();
		};
		try {
			if (options?.maxTokens === 1) {
				// A cache-warming replay. Claude Code keeps its own one-hour cache; answer without
				// touching the live session (a replay of an older context would otherwise end it).
				output.stopReason = "length";
				stream.push({ type: "start", partial: output });
				stream.push({ type: "done", reason: "length", message: output });
				stream.end();
				return;
			}
			const t = readTranscript(context);
			if (t.messages.length === 0 || t.messages[t.messages.length - 1].role === "assistant") {
				return fail("error", "gruntas: the transcript must end with a user message or tool results");
			}
			const thinking = thinkingOptions(options?.reasoning, !!(model as any).thinkingLevelMap);
			const account = await this.opts.account();
			const spec = {
				cwd: this.opts.cwd(),
				account,
				model: model.id,
				systemPrompt: t.systemPrompt,
				tools: t.tools,
				toolsHash: t.toolsHash,
				thinkingKey: JSON.stringify([thinking, options?.cacheRetention === "none"]),
				noCache: options?.cacheRetention === "none",
				...thinking,
				maxOutputTokens: options?.maxTokens ?? model.maxTokens,
				debug: this.opts.debug,
			} satisfies SessionSpec;

			const attached = await this.attach(t, spec);
			session = attached.session;
			this.lastRoute = attached.route;
			this.log(`route=${attached.route} account=${account.name} session=${session.sessionId} prefix=${attached.prefix}/${t.messages.length}`);

			const payload = await options?.onPayload?.({ sessionId: session.sessionId, route: attached.route, model: model.id, tail: t.messages.length - attached.prefix }, model);
			void payload; // The request body is built inside Claude Code; there is nothing to replace.

			// Hand pi's new messages to Claude Code.
			const tail = t.messages.slice(attached.prefix);
			const results = tail.filter((m): m is ToolResultMessage => m.role === "toolResult");
			const rest = tail.filter((m) => m.role !== "toolResult");
			if (attached.route === "live" && session.awaiting.size > 0) {
				// Mid-turn: a message typed while tools ran is queued first so Claude Code adds it at
				// this tool boundary, then the parked tool calls are released.
				const extra = userTurnBlocks(rest);
				if (extra.length > 0) session.send(extra, "next");
				for (const r of results) session.deliver(r);
			} else {
				// A fresh Claude Code session gets pi's instructions with the first turn.
				session.send(userTurnBlocks(attached.prefix === 0 ? [contextMessage(t.systemPrompt), ...tail] : tail));
			}
			session.chain = t.chain[t.messages.length];
			session.count = t.messages.length;

			const step = await this.readStep(session, model, options, stream, output, () => {
				started = true;
			});
			if (step === "aborted") return fail("aborted", "Request was aborted");
			if (!step.ok) {
				// Claude Code is in an unknown state after a failure; the next call resumes from disk.
				this.drop(session);
				return fail("error", step.error);
			}

			// Record the new conversation state, which the next call continues from.
			session.chain = sha(`${session.chain}\0${fingerprint(output)}`);
			session.count += 1;
			session.awaiting = new Set(output.content.filter((c): c is ToolCall => c.type === "toolCall").map((c) => c.id));
			if (output.stopReason !== "toolUse") session.awaiting.clear();
			stream.push({ type: "done", reason: output.stopReason as Extract<StopReason, "stop" | "length" | "toolUse">, message: output });
			stream.end();
			if (session.spec.tools.length === 0) this.drop(session);
		} catch (e) {
			if (session) this.drop(session);
			fail(options?.signal?.aborted ? "aborted" : "error", errorText(e, session?.stderr));
		} finally {
			if (session) {
				session.busy = false;
				this.active.delete(session);
			}
			this.scheduleSweep();
		}
	}

	/**
	 * Find or create the Claude Code process for this transcript: a live process on the
	 * request's account that has consumed a prefix of it, else a new one on that account, resumed
	 * from a transcript written there for the history (the source package's anchor registry,
	 * which resumes Claude Code's own bytes after a divergence, is left out: a gruntas account
	 * keeps its sessions in its own home); a fresh one when there is no history.
	 */
	private async attach(t: Transcript, spec: SessionSpec): Promise<{ session: ClaudeSession; prefix: number; route: Route }> {
		const n = t.messages.length;
		for (const s of this.sessions) {
			if (s.dead || s.busy || s.spec.account.name !== spec.account.name || s.systemPrompt !== t.systemPrompt || s.spec.toolsHash !== t.toolsHash || s.spec.thinkingKey !== spec.thinkingKey) continue;
			if (s.count > n || t.chain[s.count] !== s.chain) continue;
			const tail = t.messages.slice(s.count);
			if (tail.some((m) => m.role === "assistant")) continue;
			const resultIds = new Set(tail.filter((m): m is ToolResultMessage => m.role === "toolResult").map((m) => m.toolCallId));
			if (s.awaiting.size > 0 && ![...s.awaiting].every((id) => resultIds.has(id))) continue;
			if (s.awaiting.size === 0 && [...resultIds].some((id) => !s.servedByClaude.has(id))) continue;
			s.busy = true; // before any await, so a concurrent call cannot select it too
			this.active.add(s);
			s.lastUsed = Date.now();
			if (s.model !== spec.model) {
				try {
					await s.setModel(spec.model);
				} catch (e) {
					// run() never receives this session, so nothing else would release it.
					s.busy = false;
					this.active.delete(s);
					this.drop(s);
					throw e;
				}
			}
			return { session: s, prefix: s.count, route: "live" };
		}

		let resume: { sessionId: string; at?: string } | undefined;
		let prefix = 0;
		let route: Route = "fresh";
		let last = -1;
		t.messages.forEach((m, i) => {
			if (m.role === "assistant") last = i;
		});
		if (last >= 0) {
			if (!spec.account.home) throw new Error(`gruntas: ${spec.account.name} has no Claude Code config home to continue this conversation in`);
			resume = synthesizeSession(spec.account.home, spec.cwd, spec.model, [contextMessage(t.systemPrompt), ...t.messages.slice(0, last + 1)]);
			prefix = last + 1;
			route = "synth";
		}
		const session = this.opts.createSession({ ...spec, resume }, t.chain[prefix], prefix);
		session.busy = true;
		this.active.add(session);
		this.sessions.push(session);
		this.track(session);
		this.evict();
		return { session, prefix, route };
	}

	/**
	 * Consume Claude Code events until the current API response ends, emitting pi events.
	 * Returns the API message id, "aborted", or an error.
	 */
	private async readStep(
		session: ClaudeSession,
		model: Model<Api>,
		options: SimpleStreamOptions | undefined,
		stream: AssistantMessageEventStream,
		output: AssistantMessage,
		onStart: () => void,
	): Promise<{ ok: true; messageId?: string } | "aborted" | { ok: false; error: string }> {
		const signal = options?.signal;
		type Block = (TextContent | ThinkingContent | (ToolCall & { partialJson?: string })) & { index?: number };
		const blocks = output.content as Block[];
		let started = false;
		let messageId: string | undefined;
		let stopReason: string | undefined;
		// A turn can span several API responses: Claude Code continues on its own after an output
		// limit, an empty response or a cut-off stream. They fold into one pi message.
		let inResponse = false;
		let responseStart = 0;
		let turnStop: StopReason | undefined;
		const finished = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, cacheWrite1h: 0 };
		let current = { ...finished };
		const start = () => {
			if (started) return;
			started = true;
			onStart();
			stream.push({ type: "start", partial: output });
		};
		const setUsage = (u: any) => {
			if (!u) return;
			if (u.input_tokens != null) current.input = u.input_tokens;
			if (u.output_tokens != null) current.output = u.output_tokens;
			if (u.cache_read_input_tokens != null) current.cacheRead = u.cache_read_input_tokens;
			if (u.cache_creation_input_tokens != null) current.cacheWrite = u.cache_creation_input_tokens;
			if (u.cache_creation?.ephemeral_1h_input_tokens != null) current.cacheWrite1h = u.cache_creation.ephemeral_1h_input_tokens;
			output.usage.input = finished.input + current.input;
			output.usage.output = finished.output + current.output;
			output.usage.cacheRead = finished.cacheRead + current.cacheRead;
			output.usage.cacheWrite = finished.cacheWrite + current.cacheWrite;
			const w1h = finished.cacheWrite1h + current.cacheWrite1h;
			if (w1h > 0) output.usage.cacheWrite1h = w1h;
			// pi reads totalTokens as the context size (compaction, footer): that of the latest response,
			// not the sum over a folded turn. The other fields sum, so cost stays right.
			output.usage.totalTokens = current.input + current.cacheRead + current.cacheWrite + current.output;
			output.usage.cost = this.opts.ai.calculateCost(model, output.usage);
		};
		const onAbort = () => void session.interrupt();
		signal?.addEventListener("abort", onAbort, { once: true });
		try {
			for (;;) {
				let m: any;
				try {
					m = await session.events.next(signal);
					session.lastUsed = Date.now();
				} catch {
					this.drop(session);
					return "aborted";
				}
				if (m === undefined) return { ok: false, error: errorText(new Error("Claude Code exited unexpectedly"), session.stderr) };
				if (m.type === "pi_error") return { ok: false, error: m.error };
				if (m.type === "stream_event" && !m.parent_tool_use_id) {
					const ev = m.event;
					await options?.onProviderStreamEvent?.(ev, model);
					switch (ev.type) {
						case "message_start":
							// The previous response broke off mid-stream and Claude Code retried it: drop its partial blocks.
							if (inResponse) blocks.length = responseStart;
							responseStart = blocks.length;
							inResponse = true;
							current = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, cacheWrite1h: 0 };
							stopReason = undefined;
							messageId = ev.message.id;
							output.responseId = ev.message.id;
							output.responseModel = ev.message.model;
							setUsage(ev.message.usage);
							start();
							break;
						case "content_block_start": {
							start();
							const cb = ev.content_block;
							if (cb.type === "text") {
								blocks.push({ type: "text", text: "", index: ev.index });
								stream.push({ type: "text_start", contentIndex: blocks.length - 1, partial: output });
							} else if (cb.type === "thinking") {
								blocks.push({ type: "thinking", thinking: "", thinkingSignature: "", index: ev.index });
								stream.push({ type: "thinking_start", contentIndex: blocks.length - 1, partial: output });
							} else if (cb.type === "redacted_thinking") {
								blocks.push({ type: "thinking", thinking: "[Reasoning redacted]", thinkingSignature: cb.data, redacted: true, index: ev.index });
								stream.push({ type: "thinking_start", contentIndex: blocks.length - 1, partial: output });
							} else if (cb.type === "tool_use") {
								blocks.push({ type: "toolCall", id: cb.id, name: piToolName(cb.name), arguments: {}, partialJson: "", index: ev.index });
								stream.push({ type: "toolcall_start", contentIndex: blocks.length - 1, partial: output });
							}
							break;
						}
						case "content_block_delta": {
							const i = blocks.findIndex((b) => b.index === ev.index);
							const b = blocks[i];
							if (!b) break;
							const d = ev.delta;
							if (d.type === "text_delta" && b.type === "text") {
								b.text += d.text;
								stream.push({ type: "text_delta", contentIndex: i, delta: d.text, partial: output });
							} else if (d.type === "thinking_delta" && b.type === "thinking") {
								b.thinking += d.thinking;
								stream.push({ type: "thinking_delta", contentIndex: i, delta: d.thinking, partial: output });
							} else if (d.type === "signature_delta" && b.type === "thinking") {
								b.thinkingSignature = (b.thinkingSignature ?? "") + d.signature;
							} else if (d.type === "input_json_delta" && b.type === "toolCall") {
								b.partialJson = (b.partialJson ?? "") + d.partial_json;
								stream.push({ type: "toolcall_delta", contentIndex: i, delta: d.partial_json, partial: output });
							}
							break;
						}
						case "content_block_stop": {
							const i = blocks.findIndex((b) => b.index === ev.index);
							const b = blocks[i];
							if (!b) break;
							delete b.index;
							if (b.type === "text") stream.push({ type: "text_end", contentIndex: i, content: b.text, partial: output });
							else if (b.type === "thinking") stream.push({ type: "thinking_end", contentIndex: i, content: b.thinking, partial: output });
							else if (b.type === "toolCall") {
								try {
									b.arguments = b.partialJson ? JSON.parse(b.partialJson) : {};
								} catch {
									b.arguments = {};
								}
								delete b.partialJson;
								stream.push({ type: "toolcall_end", contentIndex: i, toolCall: b, partial: output });
							}
							break;
						}
						case "message_delta":
							stopReason = ev.delta?.stop_reason ?? stopReason;
							setUsage(ev.usage);
							break;
						case "message_stop":
							for (const b of blocks) delete b.index;
							inResponse = false;
							Object.assign(finished, {
								input: finished.input + current.input,
								output: finished.output + current.output,
								cacheRead: finished.cacheRead + current.cacheRead,
								cacheWrite: finished.cacheWrite + current.cacheWrite,
								cacheWrite1h: finished.cacheWrite1h + current.cacheWrite1h,
							});
							current = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, cacheWrite1h: 0 };
							turnStop = mapStop(stopReason);
							output.stopReason = turnStop;
							// A tool call hands control to pi. Any other stop may still be followed by Claude
							// Code's own continuation, so the turn ends only at its result.
							if (turnStop === "toolUse") return { ok: true, messageId };
							break;
					}
				} else if (m.type === "rate_limit_event") {
					this.opts.onRateLimit?.(session.spec.account, m.rate_limit_info);
				} else if (m.type === "assistant" && m.error && !started) {
					const text = (m.message?.content ?? []).map((c: any) => c.text ?? "").join("");
					return { ok: false, error: text || `Claude Code error: ${m.error}` };
				} else if (m.type === "result") {
					if (m.is_error || m.subtype !== "success") return { ok: false, error: (m.errors ?? []).join("\n") || m.result || `Claude Code ${m.subtype}` };
					output.stopReason = turnStop ?? "stop";
					start();
					return { ok: true, messageId };
				}
			}
		} finally {
			signal?.removeEventListener("abort", onAbort);
		}
	}

	private drop(s: ClaudeSession): void {
		s.close();
		this.sessions = this.sessions.filter((x) => x !== s);
	}

	private track(s: ClaudeSession): void {
		this.all.add(s);
		void s.exited.then(() => this.all.delete(s));
	}

	private evict(): void {
		const live = this.sessions.filter((s) => !s.dead);
		this.sessions = live;
		// Sessions in a provider call are never evicted, so the pool may briefly exceed maxSessions.
		const idle = this.sessions.filter((s) => !this.active.has(s)).sort((a, b) => a.lastUsed - b.lastUsed);
		while (this.sessions.length > this.opts.maxSessions && idle.length > 0) this.drop(idle.shift()!);
		// Backstop: a process that left the pool without being closed, or a session whose busy flag
		// outlived its call, is closed once more than maxSessions processes run outside provider calls.
		const pooled = (s: ClaudeSession) => (this.sessions.includes(s) ? 1 : 0);
		const stray = [...this.all].filter((s) => !s.dead && !this.active.has(s)).sort((a, b) => pooled(a) - pooled(b) || a.lastUsed - b.lastUsed);
		while (stray.length > this.opts.maxSessions) this.drop(stray.shift()!);
	}

	private scheduleSweep(): void {
		if (this.timer) return;
		this.timer = setInterval(() => {
			this.sweep();
			if (this.all.size === 0 && this.timer) {
				clearInterval(this.timer);
				this.timer = undefined;
			}
		}, 60_000);
		this.timer.unref();
	}

	/** Close processes idle longer than idleMs. */
	sweep(now = Date.now()): void {
		for (const s of [...this.all]) {
			if (now - s.lastUsed <= this.opts.idleMs) continue;
			// A pooled session parked on tool calls waits for pi's results; anything else idle goes,
			// including a call that has had no event from Claude Code for that long (its reader then ends).
			if (!this.active.has(s) && this.sessions.includes(s) && s.awaiting.size > 0) continue;
			this.drop(s);
		}
	}

	/**
	 * The account moved (accounts.ts): close the idle processes of every other account. The next
	 * request starts afresh on the new one with the conversation pi holds, its cache cold there.
	 */
	retire(keep: string): void {
		for (const s of [...this.sessions]) if (s.spec.account.name !== keep && !this.active.has(s)) this.drop(s);
	}

	shutdown(): void {
		for (const s of new Set([...this.sessions, ...this.all])) this.drop(s);
	}
}

function mapStop(reason: string | undefined): StopReason {
	switch (reason) {
		case "tool_use":
			return "toolUse";
		case "max_tokens":
		case "model_context_window_exceeded":
			return "length";
		default:
			return "stop";
	}
}

export type { TurnMessage };
