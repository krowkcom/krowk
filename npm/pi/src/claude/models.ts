// Copied from pi-claude-subscription 0.2.4 (MIT, Jarno Keskikangas; THIRD-PARTY-NOTICES).

import type { Api, Model } from "@earendil-works/pi-ai";
import type { ProviderModelConfig } from "@earendil-works/pi-coding-agent";
import { CATALOG } from "./catalog.ts";

type ProviderChatModelConfig = Extract<ProviderModelConfig, { reasoning: boolean }>;
type CatalogModel = Pick<Model<Api>, "id" | "name" | "reasoning" | "input" | "cost" | "contextWindow" | "maxTokens" | "thinkingLevelMap">;

/**
 * Chat models offered through Claude Code, as `gruntas/<id>`. Limits, prices and thinking levels come from pi's
 * Anthropic catalog: a snapshot at load (so `--model` works at startup), refreshed from pi's live
 * registry once a session starts (see provider.ts's installClaude). Dated aliases are skipped; Claude Code resolves
 * the undated ids itself.
 */
export function claudeModels(source: readonly CatalogModel[] = CATALOG as unknown as CatalogModel[]): ProviderChatModelConfig[] {
	return source
		.filter((m) => !/-\d{8}$/.test(m.id))
		.map((m) => ({
			id: m.id,
			name: `${m.name} (gruntas account)`,
			reasoning: m.reasoning,
			input: m.input,
			cost: m.cost,
			contextWindow: m.contextWindow,
			maxTokens: m.maxTokens,
			...(m.thinkingLevelMap ? { thinkingLevelMap: m.thinkingLevelMap } : {}),
			// No promptCache: Claude Code already writes one-hour cache entries, and pi's cache
			// warmer (a one-token replay of an earlier request) cannot be expressed through it.
		}));
}

/** pi's Anthropic models that can back this provider. */
export function anthropicChatModels(models: readonly (CatalogModel & { provider: string; type?: string })[]): CatalogModel[] {
	return models.filter((m) => m.provider === "anthropic" && (m.type === undefined || m.type === "chat"));
}

const EFFORTS = new Set(["low", "medium", "high", "xhigh", "max"]);

/**
 * Map a pi thinking level to Agent SDK thinking options. pi resolves the level through the
 * model's thinkingLevelMap before it reaches the provider, so the value is already the
 * provider-native effort when the model declares one.
 */
export function thinkingOptions(
	level: string | undefined,
	hasLevelMap: boolean,
): { thinking: { type: "adaptive" } | { type: "disabled" } | { type: "enabled"; budgetTokens: number }; effort?: "low" | "medium" | "high" | "xhigh" | "max" } {
	if (!level || level === "off") return { thinking: { type: "disabled" } };
	if (hasLevelMap) {
		const effort = EFFORTS.has(level) ? (level as any) : level === "minimal" ? "low" : "medium";
		return { thinking: { type: "adaptive" }, effort };
	}
	// Budget-based models (e.g. Haiku 4.5).
	const budgets: Record<string, number> = { minimal: 1024, low: 4096, medium: 10240, high: 20480, xhigh: 32000, max: 48000 };
	return { thinking: { type: "enabled", budgetTokens: budgets[level] ?? 10240 } };
}
