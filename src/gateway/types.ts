export type ApiFormat = "openai_chat" | "openai_responses" | "anthropic";
export type RoutingPolicy =
  "priority" | "round_robin" | "weighted_round_robin" | "least_outstanding";
export type Tab =
  "dashboard" | "providers" | "routes" | "logs" | "registry" | "settings";
export type ProxyMode = "follow_global" | "bypass" | "custom";

export interface CachedModel {
  id: string;
  ownedBy?: string | null;
  displayName?: string | null;
  metadata?: ModelMetadata;
}

export interface ProviderModel {
  alias: string;
  upstreamModel: string;
  apiFormat: ApiFormat;
  enabled: boolean;
  recordBodies?: boolean | null;
  metadata?: ModelMetadata;
}

export interface ModelMetadata {
  contextLength?: number | null;
  maxOutputTokens?: number | null;
  inputModalities?: string[];
  reasoningLevels?: string[];
}

export interface ModelCapabilityResolution {
  canonicalModel: string;
  metadata: ModelMetadata;
  defaultReasoningLevel?: string | null;
  source: "registry" | "unknown" | string;
}

export interface ModelRegistryEntry {
  canonicalModel: string;
  ids: string[];
  baseMetadata: ModelMetadata;
  metadata: ModelMetadata;
  defaultReasoningLevel?: string | null;
  overridden: boolean;
}

export interface ModelRegistrySnapshot {
  overrideFile: string;
  entries: ModelRegistryEntry[];
}

export interface GatewayProvider {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  enabled: boolean;
  authStyle: "auto" | "bearer" | "x-api-key";
  customUserAgent: string;
  modelsUrl: string;
  cachedModels: CachedModel[];
  modelsFetchedAt?: string | null;
  customHeaders: Record<string, string>;
  impersonateCodexClient: boolean;
  codexClientVersion: string;
  reasoningRequestMode: "auto" | "force" | "disabled";
  chatSchemaRequiredDefaults: boolean;
  chatReasoningProfile:
    "auto" | "openai" | "deepseek" | "openrouter" | "siliconflow" | "disabled";
  reasoningHistoryMode: "auto" | "reasoning_content" | "disabled";
  adaptiveThinkingDisplay: "auto" | "summarized" | "omitted";
  notes: string;
  recordBodies?: boolean;
  maxConcurrentRequests: number;
  queueLimit: number;
  queueTimeoutMs: number;
  models: ProviderModel[];
}

export interface ModelFetchResult {
  models: CachedModel[];
  fetchedAt: string;
}

export interface GatewayConfig {
  schemaVersion: number;
  revision: number;
  listenAddress: string;
  listenPort: number;
  requireAuth: boolean;
  localApiKey: string;
  autoStart: boolean;
  enableLogging: boolean;
  routingPolicies: Record<string, RoutingPolicy>;
  routingWeights: Record<string, Record<string, number>>;
  modelRegistryOverrides: Record<string, ModelMetadata>;
  providers: GatewayProvider[];
}

export interface ProxyStatus {
  running: boolean;
  address: string;
  port: number;
  active_connections: number;
  total_requests: number;
  success_requests: number;
  failed_requests: number;
  success_rate: number;
  uptime_seconds: number;
  current_provider?: string | null;
  last_error?: string | null;
  failover_count: number;
  upstream_failed_attempts: number;
  active_providers?: ActiveProvider[];
}

export interface ProviderRuntimeStatus {
  sourceProviderId: string;
  materializedProviderId: string;
  providerName: string;
  apiFormat: ApiFormat;
  appType: string;
  circuitState: "closed" | "open" | "half_open" | string;
  cooldownSeconds?: number | null;
  consecutiveFailures: number;
  totalRequests: number;
  maxConcurrentRequests: number;
  activeRequests: number;
  queuedRequests: number;
}

export interface ActiveProvider {
  app_type: string;
  provider_name: string;
  provider_id: string;
  request_count: number;
}

export interface GatewaySnapshot {
  config: GatewayConfig;
  status: ProxyStatus;
  providerRuntime: ProviderRuntimeStatus[];
}

export interface BuildInfo {
  version: string;
  commit: string;
  builtAt: string;
}

export interface StartupPreferences {
  launchOnStartup: boolean;
  startHiddenOnAutostart: boolean;
  minimizeToTrayOnClose: boolean;
}

export interface ProxyTestResult {
  success: boolean;
  latencyMs: number;
  error?: string | null;
}

export interface DetectedProxy {
  url: string;
  proxyType: string;
  port: number;
}

export interface ModelTestUsage {
  inputTokens?: number | null;
  outputTokens?: number | null;
  totalTokens?: number | null;
  cacheReadInputTokens?: number | null;
  cacheCreationInputTokens?: number | null;
  reasoningTokens?: number | null;
}

export interface ModelTestResult {
  ok: boolean;
  status: number;
  latencyMs: number;
  replyText: string;
  /** 上游单独返回的思考/推理内容；与正文分开，正文可能为空。 */
  reasoningText?: string;
  rawBody: string;
  rawRequest?: string;
  error?: string | null;
  pathUsed: string;
  proxyEffective?: string | null;
  finishReason?: string | null;
  lengthTruncated: boolean;
  usage?: ModelTestUsage | null;
}

export type ModelTestRole = "user" | "assistant";

export type TestThinkingLevel = "disabled" | "low" | "medium" | "high";

/** 流式测试期间后端推送的事件，与 gateway.rs 的 GatewayModelTestStreamEvent 对应。 */
export type ModelTestStreamEvent =
  | { kind: "start"; status: number }
  | { kind: "delta"; text: string; reasoning: string };

export interface ModelTestMessage {
  role: ModelTestRole;
  content: string;
  /** 助手消息：上游单独返回的思考内容（正文为空时仍予以展示）。 */
  reasoningContent?: string;
  /** 当次发送给上游/网关的整段请求 payload（用户与助手消息共享同一次调用）。 */
  rawRequest?: string;
  /** 助手消息：当次返回的原始响应体。 */
  rawResponse?: string;
}

export interface ModelTestContext {
  providerName: string;
  provider: GatewayProvider;
  modelIndex: number;
}
