// Copied from pi-claude-subscription 0.2.4 (MIT, Jarno Keskikangas; THIRD-PARTY-NOTICES).

import { createHash } from "node:crypto";

/** Unbounded single-consumer async queue. `close()` ends iteration after buffered items drain. */
export class AsyncQueue<T> implements AsyncIterable<T> {
	private items: T[] = [];
	private waiters: ((r: IteratorResult<T>) => void)[] = [];
	private closed = false;

	push(item: T): void {
		if (this.closed) return;
		const w = this.waiters.shift();
		if (w) w({ value: item, done: false });
		else this.items.push(item);
	}

	close(): void {
		this.closed = true;
		for (const w of this.waiters.splice(0)) w({ value: undefined as never, done: true });
	}

	get isClosed(): boolean {
		return this.closed;
	}

	/** Next item, or undefined once closed and drained. Rejects on abort. */
	next(signal?: AbortSignal): Promise<T | undefined> {
		if (this.items.length > 0) return Promise.resolve(this.items.shift());
		if (this.closed) return Promise.resolve(undefined);
		return new Promise((resolve, reject) => {
			const onAbort = () => {
				const i = this.waiters.indexOf(waiter);
				if (i >= 0) this.waiters.splice(i, 1);
				reject(new Error("aborted"));
			};
			const waiter = (r: IteratorResult<T>) => {
				signal?.removeEventListener("abort", onAbort);
				resolve(r.done ? undefined : r.value);
			};
			if (signal?.aborted) return reject(new Error("aborted"));
			signal?.addEventListener("abort", onAbort, { once: true });
			this.waiters.push(waiter);
		});
	}

	async *[Symbol.asyncIterator](): AsyncIterator<T> {
		for (;;) {
			const item = await this.next();
			if (item === undefined && this.closed && this.items.length === 0) return;
			yield item as T;
		}
	}
}

/** JSON with sorted object keys, so equal values hash equally regardless of key order. */
export function stableStringify(value: unknown): string {
	if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
	if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
	const keys = Object.keys(value as object)
		.filter((k) => (value as Record<string, unknown>)[k] !== undefined)
		.sort();
	return `{${keys.map((k) => `${JSON.stringify(k)}:${stableStringify((value as Record<string, unknown>)[k])}`).join(",")}}`;
}

export function sha(text: string): string {
	return createHash("sha256").update(text).digest("hex").slice(0, 32);
}

export function errorText(e: unknown, stderr = ""): string {
	const msg = e instanceof Error ? e.message : String(e);
	return stderr.trim() ? `${msg}\n${stderr.trim().split("\n").slice(-5).join("\n")}` : msg;
}
