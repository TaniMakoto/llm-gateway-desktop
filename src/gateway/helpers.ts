import type { ApiFormat, ModelMetadata, GatewayConfig } from "./types";

export const DEFAULT_CONFIG: GatewayConfig = {
  schemaVersion: 1,
  revision: 0,
  listenAddress: "127.0.0.1",
  listenPort: 10888,
  requireAuth: true,
  localApiKey: "",
  autoStart: false,
  enableLogging: true,
  routingPolicies: {},
  routingWeights: {},
  modelRegistryOverrides: {},
  providers: [],
};

export const formatLabels: Record<ApiFormat, string> = {
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
  anthropic: "Anthropic Messages",
};

/** 模型测试页"最大输出 token"输入框的上限，需与后端 gateway.rs 的
 *  TEST_MAX_OUTPUT_TOKENS_MAX 保持一致。 */
export const MODEL_TEST_MAX_OUTPUT_TOKENS = 131072;
export const COMMON_REASONING_LEVELS = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];
export const COMMON_MODALITIES = ["text", "image", "audio", "video", "pdf"];

export function hasMetadataValues(metadata?: ModelMetadata): boolean {
  return Boolean(
    metadata?.contextLength ||
    metadata?.maxOutputTokens ||
    (metadata?.inputModalities?.length ?? 0) > 0 ||
    (metadata?.reasoningLevels?.length ?? 0) > 0,
  );
}

export function firstNonEmptyList(
  ...values: Array<string[] | undefined>
): string[] {
  return values.find((value) => (value?.length ?? 0) > 0) ?? [];
}

export function conservativeListCapability(
  routeOverride: string[] | undefined,
  provider: string[] | undefined,
  registry: string[] | undefined,
): string[] {
  if ((routeOverride?.length ?? 0) > 0) return routeOverride ?? [];
  const base = registry ?? [];
  if ((provider?.length ?? 0) === 0) return base;
  if (base.length === 0) return provider ?? [];
  return base.filter((value) => provider?.includes(value));
}

export function conservativeLimit(
  override: number | null | undefined,
  provider: number | null | undefined,
  registry: number | null | undefined,
): number | null {
  if (override != null) return override;
  const known = [provider, registry].filter(
    (value): value is number => typeof value === "number" && value > 0,
  );
  return known.length > 0 ? Math.min(...known) : null;
}

export function formatTokenLimit(value?: number | null): string {
  if (!value) return "未知";
  if (value >= 1_000_000)
    return `${(value / 1_000_000).toFixed(2).replace(/\.00$/, "")}M`;
  if (value >= 1_000) return `${Math.round(value / 1_000)}K`;
  return String(value);
}

export function newId(prefix: string): string {
  const value =
    globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`;
  return `${prefix}-${value}`;
}

export function formatDuration(seconds: number): string {
  if (!seconds) return "0 秒";
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;
  return [
    hours ? `${hours} 时` : "",
    minutes ? `${minutes} 分` : "",
    `${secs} 秒`,
  ]
    .filter(Boolean)
    .join(" ");
}

export function maskKey(value: string): string {
  if (!value) return "未设置";
  if (value.length <= 12) return "••••••••";
  return `${value.slice(0, 8)}••••••••${value.slice(-4)}`;
}

export function addressForUrl(address: string): string {
  return address.includes(":") && !address.startsWith("[")
    ? `[${address}]`
    : address;
}

export function allEnabledAliases(config: GatewayConfig): string[] {
  const set = new Set<string>();
  for (const provider of config.providers) {
    if (!provider.enabled) continue;
    for (const model of provider.models) {
      if (!model.enabled) continue;
      const alias = model.alias.trim();
      if (alias) set.add(alias);
    }
  }
  return Array.from(set).sort();
}
