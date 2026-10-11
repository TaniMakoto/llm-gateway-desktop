import { useEffect, useMemo, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { toast } from "sonner";
import {
  Activity,
  Check,
  ChevronDown,
  ChevronRight,
  ChevronUp,
  CircleStop,
  Copy,
  Database,
  ScrollText,
  Eye,
  EyeOff,
  FolderOpen,
  LayoutDashboard,
  Maximize2,
  Minus,
  Network,
  Play,
  Plus,
  RadioTower,
  RefreshCw,
  RotateCcw,
  Route,
  Save,
  Send,
  Server,
  Settings,
  ShieldCheck,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { RequestLogsPage } from "@/RequestLogsPage";
import type {
  ApiFormat,
  RoutingPolicy,
  Tab,
  CachedModel,
  ProviderModel,
  ModelMetadata,
  ModelCapabilityResolution,
  ModelRegistrySnapshot,
  GatewayProvider,
  ModelFetchResult,
  GatewayConfig,
  ProxyStatus,
  ProviderRuntimeStatus,
  ActiveProvider,
  GatewaySnapshot,
  StartupPreferences,
  ProxyTestResult,
  DetectedProxy,
  ModelTestContext,
} from "@/gateway/types";
import {
  DEFAULT_CONFIG,
  formatLabels,
  COMMON_REASONING_LEVELS,
  COMMON_MODALITIES,
  hasMetadataValues,
  firstNonEmptyList,
  conservativeListCapability,
  conservativeLimit,
  formatTokenLimit,
  newId,
  formatDuration,
  maskKey,
  addressForUrl,
  allEnabledAliases,
} from "@/gateway/helpers";
import { ModelTestModal } from "@/components/ModelTestModal";
import { VersionInfo } from "@/components/VersionInfo";

function App() {
  const [tab, setTab] = useState<Tab>("dashboard");
  const [config, setConfig] = useState<GatewayConfig>(DEFAULT_CONFIG);
  const [savedConfig, setSavedConfig] = useState<GatewayConfig>(DEFAULT_CONFIG);
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [providerRuntime, setProviderRuntime] = useState<
    ProviderRuntimeStatus[]
  >([]);
  const [modelRegistry, setModelRegistry] =
    useState<ModelRegistrySnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [startupPreferences, setStartupPreferences] =
    useState<StartupPreferences | null>(null);
  const [startupSettingsBusy, setStartupSettingsBusy] = useState(false);
  const [showLocalKey, setShowLocalKey] = useState(false);
  const [providerEditor, setProviderEditor] = useState<GatewayProvider | null>(
    null,
  );
  const [editingProviderId, setEditingProviderId] = useState<string | null>(
    null,
  );
  const [showProviderKey, setShowProviderKey] = useState(false);
  const [headersText, setHeadersText] = useState("");
  const [fetchingModels, setFetchingModels] = useState(false);
  const [modelsFetchFormat, setModelsFetchFormat] =
    useState<ApiFormat>("openai_chat");
  const [expandedProviders, setExpandedProviders] = useState<
    Record<string, boolean>
  >({});
  const [testCtx, setTestCtx] = useState<ModelTestContext | null>(null);

  // 全局出口代理（独立于网关配置持久化）
  const [globalProxyUrl, setGlobalProxyUrl] = useState<string>("");
  const [savedGlobalProxyUrl, setSavedGlobalProxyUrl] = useState<string>("");
  const [proxyTesting, setProxyTesting] = useState(false);
  const [proxyApplying, setProxyApplying] = useState(false);
  const [detectedProxies, setDetectedProxies] = useState<DetectedProxy[]>([]);
  const [scanningProxies, setScanningProxies] = useState(false);

  const dirty = useMemo(
    () => JSON.stringify(config) !== JSON.stringify(savedConfig),
    [config, savedConfig],
  );
  const proxyDirty = globalProxyUrl !== savedGlobalProxyUrl;
  const baseUrl = `http://${addressForUrl(config.listenAddress)}:${config.listenPort}`;

  const loadSnapshot = async (showSpinner = false) => {
    if (showSpinner) setLoading(true);
    try {
      const snapshot = await invoke<GatewaySnapshot>("get_gateway_snapshot");
      setConfig(snapshot.config);
      setSavedConfig(snapshot.config);
      setStatus(snapshot.status);
      setProviderRuntime(snapshot.providerRuntime ?? []);
    } catch (error) {
      toast.error(`读取网关配置失败：${String(error)}`);
    } finally {
      if (showSpinner) setLoading(false);
    }
  };

  const loadModelRegistry = async (notifyOnError = false) => {
    try {
      const snapshot = await invoke<ModelRegistrySnapshot>(
        "get_gateway_model_registry",
      );
      setModelRegistry(snapshot);
    } catch (error) {
      console.warn("Failed to load model registry", error);
      if (notifyOnError) {
        toast.error(`读取模型元数据失败：${String(error)}`);
      }
    }
  };

  const loadGlobalProxy = async () => {
    try {
      const url = await invoke<string | null>("get_global_proxy_url");
      const value = url ?? "";
      setGlobalProxyUrl(value);
      setSavedGlobalProxyUrl(value);
    } catch (error) {
      console.warn("Failed to load global proxy", error);
    }
  };

  const loadStartupPreferences = async () => {
    try {
      const preferences = await invoke<StartupPreferences>(
        "get_startup_preferences",
      );
      setStartupPreferences(preferences);
    } catch (error) {
      console.warn("Failed to load startup preferences", error);
    }
  };

  const updateStartupPreference = async (
    command:
      | "set_auto_launch"
      | "set_start_hidden_on_autostart"
      | "set_minimize_to_tray_on_close",
    enabled: boolean,
  ) => {
    setStartupSettingsBusy(true);
    try {
      const preferences = await invoke<StartupPreferences>(command, {
        enabled,
      });
      setStartupPreferences(preferences);
      if (command === "set_auto_launch") {
        toast.success(
          enabled
            ? "已启用开机自启动；如需开机后直接提供 API，请同时开启自动启动网关"
            : "已关闭开机自启动",
        );
      } else {
        toast.success("启动与后台设置已保存");
      }
    } catch (error) {
      toast.error(`保存启动设置失败：${String(error)}`);
      await loadStartupPreferences();
    } finally {
      setStartupSettingsBusy(false);
    }
  };

  const refreshStatus = async () => {
    try {
      const snapshot = await invoke<GatewaySnapshot>("get_gateway_snapshot");
      setStatus(snapshot.status);
      setProviderRuntime(snapshot.providerRuntime ?? []);
    } catch {
      // 静默
    }
  };

  useEffect(() => {
    void loadSnapshot(true);
    void loadGlobalProxy();
    void loadStartupPreferences();
    void loadModelRegistry();
    const timer = window.setInterval(() => void refreshStatus(), 2000);
    return () => window.clearInterval(timer);
  }, []);

  const save = async (nextConfig = config, silent = false) => {
    setSaving(true);
    try {
      const saved = await invoke<GatewayConfig>("save_gateway_config", {
        config: nextConfig,
      });
      setConfig(saved);
      setSavedConfig(saved);
      void loadModelRegistry();
      if (!silent) toast.success("网关配置已保存");
      return true;
    } catch (error) {
      toast.error(`保存失败：${String(error)}`);
      return false;
    } finally {
      setSaving(false);
    }
  };

  const applyGlobalProxy = async () => {
    setProxyApplying(true);
    try {
      await invoke("set_global_proxy_url", { url: globalProxyUrl });
      setSavedGlobalProxyUrl(globalProxyUrl);
      toast.success(globalProxyUrl.trim() ? "出口代理已应用" : "已切换为直连");
    } catch (error) {
      toast.error(`应用代理失败：${String(error)}`);
    } finally {
      setProxyApplying(false);
    }
  };

  const testGlobalProxy = async () => {
    const url = globalProxyUrl.trim();
    if (!url) {
      toast.error("请输入代理地址");
      return;
    }
    setProxyTesting(true);
    try {
      const result = await invoke<ProxyTestResult>("test_proxy_url", { url });
      if (result.success) {
        toast.success(`代理可用 · ${result.latencyMs}ms`);
      } else {
        toast.error(`代理不可用：${result.error ?? "unknown"}`);
      }
    } catch (error) {
      toast.error(`测试失败：${String(error)}`);
    } finally {
      setProxyTesting(false);
    }
  };

  const scanProxies = async () => {
    setScanningProxies(true);
    try {
      const list = await invoke<DetectedProxy[]>("scan_local_proxies");
      setDetectedProxies(list);
      if (!list.length) toast.info("未在常见端口检测到代理");
    } catch (error) {
      toast.error(`扫描失败：${String(error)}`);
    } finally {
      setScanningProxies(false);
    }
  };

  const startGateway = async () => {
    if (!(await save(config, true))) return;
    try {
      await invoke("start_gateway");
      await refreshStatus();
      toast.success(`网关已启动：${baseUrl}`);
    } catch (error) {
      toast.error(`启动失败：${String(error)}`);
    }
  };

  const stopGateway = async () => {
    try {
      await invoke("stop_gateway");
      await refreshStatus();
      toast.success("网关已停止");
    } catch (error) {
      toast.error(`停止失败：${String(error)}`);
    }
  };

  const regenerateKey = async () => {
    try {
      const key = await invoke<string>("generate_gateway_api_key");
      setConfig((current) => ({ ...current, localApiKey: key }));
      setShowLocalKey(true);
    } catch (error) {
      toast.error(`生成密钥失败：${String(error)}`);
    }
  };

  const copyText = async (text: string, label: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(`${label}已复制`);
    } catch {
      toast.error("复制失败");
    }
  };

  const openNewProvider = () => {
    const provider: GatewayProvider = {
      id: newId("provider"),
      name: "",
      baseUrl: "",
      apiKey: "",
      enabled: true,
      authStyle: "auto",
      customUserAgent: "",
      modelsUrl: "",
      cachedModels: [],
      modelsFetchedAt: null,
      customHeaders: {},
      impersonateCodexClient: false,
      codexClientVersion: "",
      reasoningRequestMode: "auto",
      chatReasoningProfile: "auto",
      chatSchemaRequiredDefaults: false,
      reasoningHistoryMode: "auto",
      adaptiveThinkingDisplay: "auto",
      notes: "",
      recordBodies: false,
      maxConcurrentRequests: 0,
      queueLimit: 32,
      queueTimeoutMs: 30000,
      models: [],
    };
    setEditingProviderId(null);
    setProviderEditor(provider);
    setHeadersText("");
    setShowProviderKey(false);
    setModelsFetchFormat("openai_chat");
  };

  const openProvider = (provider: GatewayProvider) => {
    setEditingProviderId(provider.id);
    setProviderEditor({
      ...structuredClone(provider),
      customUserAgent: provider.customUserAgent ?? "",
      modelsUrl: provider.modelsUrl ?? "",
      cachedModels: provider.cachedModels ?? [],
      modelsFetchedAt: provider.modelsFetchedAt ?? null,
      impersonateCodexClient: provider.impersonateCodexClient ?? false,
      codexClientVersion: provider.codexClientVersion ?? "",
      reasoningRequestMode: provider.reasoningRequestMode ?? "auto",
      chatReasoningProfile: provider.chatReasoningProfile ?? "auto",
      chatSchemaRequiredDefaults: provider.chatSchemaRequiredDefaults ?? false,
      reasoningHistoryMode: provider.reasoningHistoryMode ?? "auto",
      adaptiveThinkingDisplay: provider.adaptiveThinkingDisplay ?? "auto",
      recordBodies: provider.recordBodies ?? false,
      maxConcurrentRequests: provider.maxConcurrentRequests ?? 0,
      queueLimit: provider.queueLimit ?? 32,
      queueTimeoutMs: provider.queueTimeoutMs ?? 30000,
      models: provider.models ?? [],
    });
    setHeadersText(
      Object.entries(provider.customHeaders)
        .map(([key, value]) => `${key}: ${value}`)
        .join("\n"),
    );
    setShowProviderKey(false);
    setModelsFetchFormat("openai_chat");
  };

  const parseHeaders = (raw: string): Record<string, string> => {
    const result: Record<string, string> = {};
    for (const line of raw.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      const index = trimmed.indexOf(":");
      if (index <= 0) throw new Error(`请求头格式错误：${line}`);
      result[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1).trim();
    }
    return result;
  };

  const fetchProviderModels = async () => {
    if (!providerEditor) return;
    if (!providerEditor.baseUrl.trim() || !providerEditor.apiKey.trim()) {
      toast.error("请先填写 Base URL 和 API Key");
      return;
    }

    let customHeaders: Record<string, string>;
    try {
      customHeaders = parseHeaders(headersText);
    } catch (error) {
      toast.error(String(error));
      return;
    }

    setFetchingModels(true);
    try {
      const result = await invoke<ModelFetchResult>(
        "fetch_gateway_provider_models",
        {
          request: {
            provider: { ...providerEditor, customHeaders },
            apiFormat: modelsFetchFormat,
          },
        },
      );
      setProviderEditor({
        ...providerEditor,
        customHeaders,
        cachedModels: result.models,
        modelsFetchedAt: result.fetchedAt,
      });
      toast.success(`已获取 ${result.models.length} 个上游模型`);
    } catch (error) {
      toast.error(`获取模型失败：${String(error)}`);
    } finally {
      setFetchingModels(false);
    }
  };

  const commitProvider = () => {
    if (!providerEditor) return;
    if (!providerEditor.name.trim() || !providerEditor.baseUrl.trim()) {
      toast.error("请填写供应商名称和 Base URL");
      return;
    }
    let customHeaders: Record<string, string>;
    try {
      customHeaders = parseHeaders(headersText);
    } catch (error) {
      toast.error(String(error));
      return;
    }
    const next = { ...providerEditor, customHeaders };
    setConfig((current) => ({
      ...current,
      providers: editingProviderId
        ? current.providers.map((provider) =>
            provider.id === editingProviderId ? next : provider,
          )
        : [...current.providers, next],
    }));
    setProviderEditor(null);
  };

  const deleteProvider = (id: string) => {
    setConfig((current) => ({
      ...current,
      providers: current.providers.filter((provider) => provider.id !== id),
    }));
  };

  const patchModelRegistryOverride = (
    canonicalModel: string,
    metadata: ModelMetadata | null,
  ) => {
    if (!canonicalModel) return;
    setConfig((current) => {
      const next = { ...(current.modelRegistryOverrides ?? {}) };
      if (!metadata || !hasMetadataValues(metadata)) {
        delete next[canonicalModel];
      } else {
        next[canonicalModel] = metadata;
      }
      return { ...current, modelRegistryOverrides: next };
    });
  };

  const moveProvider = (index: number, direction: -1 | 1) => {
    setConfig((current) => {
      const next = structuredClone(current);
      const target = index + direction;
      if (target < 0 || target >= next.providers.length) return current;
      [next.providers[index], next.providers[target]] = [
        next.providers[target],
        next.providers[index],
      ];
      return next;
    });
  };

  const patchProvider = (
    providerId: string,
    patch: Partial<GatewayProvider>,
  ) => {
    setConfig((current) => ({
      ...current,
      providers: current.providers.map((provider) =>
        provider.id === providerId ? { ...provider, ...patch } : provider,
      ),
    }));
  };

  const patchProviderModel = (
    providerId: string,
    modelIndex: number,
    patch: Partial<ProviderModel>,
  ) => {
    setConfig((current) => ({
      ...current,
      providers: current.providers.map((provider) =>
        provider.id === providerId
          ? {
              ...provider,
              models: provider.models.map((model, index) =>
                index === modelIndex ? { ...model, ...patch } : model,
              ),
            }
          : provider,
      ),
    }));
  };

  const addProviderModel = (
    providerId: string,
    initial?: Partial<ProviderModel>,
  ) => {
    setConfig((current) => ({
      ...current,
      providers: current.providers.map((provider) =>
        provider.id === providerId
          ? {
              ...provider,
              models: [
                ...provider.models,
                {
                  alias: initial?.alias ?? "",
                  upstreamModel: initial?.upstreamModel ?? "",
                  apiFormat: initial?.apiFormat ?? "openai_chat",
                  enabled: initial?.enabled ?? true,
                  recordBodies: initial?.recordBodies ?? null,
                  metadata: initial?.metadata ?? {
                    inputModalities: [],
                    reasoningLevels: [],
                  },
                },
              ],
            }
          : provider,
      ),
    }));
    setExpandedProviders((current) => ({ ...current, [providerId]: true }));
  };

  const removeProviderModel = (providerId: string, modelIndex: number) => {
    setConfig((current) => ({
      ...current,
      providers: current.providers.map((provider) =>
        provider.id === providerId
          ? {
              ...provider,
              models: provider.models.filter(
                (_, index) => index !== modelIndex,
              ),
            }
          : provider,
      ),
    }));
  };

  const appWindow = getCurrentWindow();
  const enabledAliases = useMemo(() => allEnabledAliases(config), [config]);

  if (loading) {
    return (
      <div className="flex h-screen items-center justify-center bg-background">
        <RefreshCw className="h-6 w-6 animate-spin text-primary" />
      </div>
    );
  }

  return (
    <div className="flex h-screen flex-col overflow-hidden bg-background text-foreground">
      <header
        data-tauri-drag-region
        className="flex h-12 shrink-0 items-center justify-between border-b bg-card px-4"
      >
        <div
          data-tauri-drag-region
          className="flex items-center gap-2 font-semibold"
        >
          <div className="flex h-7 w-7 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <Network className="h-4 w-4" />
          </div>
          <span>LLM Gateway Desktop</span>
          <span className="rounded bg-muted px-2 py-0.5 text-[10px] font-medium text-muted-foreground">
            0.1.0
          </span>
        </div>
        <div className="no-drag flex items-center">
          <button
            className="window-button"
            onClick={() => void appWindow.minimize()}
          >
            <Minus className="h-4 w-4" />
          </button>
          <button
            className="window-button"
            onClick={() => void appWindow.toggleMaximize()}
          >
            <Maximize2 className="h-3.5 w-3.5" />
          </button>
          <button
            className="window-button hover:bg-destructive hover:text-destructive-foreground"
            onClick={() => void appWindow.close()}
          >
            <X className="h-4 w-4" />
          </button>
        </div>
      </header>

      <div className="flex min-h-0 flex-1">
        <aside className="flex w-56 shrink-0 flex-col border-r bg-card/50 p-3">
          <nav className="space-y-1">
            <NavButton
              icon={LayoutDashboard}
              label="运行状态"
              active={tab === "dashboard"}
              onClick={() => setTab("dashboard")}
            />
            <NavButton
              icon={Server}
              label="上游供应商"
              active={tab === "providers"}
              onClick={() => setTab("providers")}
              badge={config.providers.length}
            />
            <NavButton
              icon={Route}
              label="模型路由"
              active={tab === "routes"}
              onClick={() => setTab("routes")}
              badge={enabledAliases.length}
            />
            <NavButton
              icon={ScrollText}
              label="请求日志"
              active={tab === "logs"}
              onClick={() => setTab("logs")}
            />
            <NavButton
              icon={Database}
              label="模型元数据"
              active={tab === "registry"}
              onClick={() => setTab("registry")}
              badge={modelRegistry?.entries.length}
            />
            <NavButton
              icon={Settings}
              label="网关设置"
              active={tab === "settings"}
              onClick={() => setTab("settings")}
            />
          </nav>

          <div className="mt-auto rounded-xl border bg-background p-3">
            <div className="flex items-center gap-2">
              <span
                className={cn(
                  "h-2.5 w-2.5 rounded-full",
                  status?.running ? "bg-emerald-500" : "bg-muted-foreground/40",
                )}
              />
              <span className="text-xs font-medium">
                {status?.running ? "正在运行" : "已停止"}
              </span>
            </div>
            <div className="mt-2 truncate font-mono text-[10px] text-muted-foreground">
              {baseUrl}
            </div>
          </div>
        </aside>

        <main className="min-w-0 flex-1 overflow-y-auto p-6">
          {tab === "dashboard" && (
            <Dashboard
              status={status}
              baseUrl={baseUrl}
              config={config}
              enabledAliases={enabledAliases}
              onStart={startGateway}
              onStop={stopGateway}
              onCopy={copyText}
              onNavigate={setTab}
            />
          )}

          {tab === "providers" && (
            <section className="mx-auto max-w-5xl">
              <PageHeader
                title="上游供应商"
                description="集中保存第三方 API 的地址和 Key；协议在“模型路由”中按模型独立指定。"
                action={
                  <button className="primary-button" onClick={openNewProvider}>
                    <Plus className="h-4 w-4" />
                    添加供应商
                  </button>
                }
              />
              {config.providers.length === 0 ? (
                <EmptyState
                  icon={Server}
                  title="还没有供应商"
                  description="先添加一个 OpenAI、Anthropic 或兼容中转 API。"
                  action="添加第一个供应商"
                  onAction={openNewProvider}
                />
              ) : (
                <div className="grid gap-4 lg:grid-cols-2">
                  {config.providers.map((provider) => (
                    <div key={provider.id} className="panel p-5">
                      <div className="flex items-start justify-between gap-3">
                        <div className="min-w-0">
                          <div className="flex items-center gap-2">
                            <span
                              className={cn(
                                "h-2.5 w-2.5 rounded-full",
                                provider.enabled
                                  ? "bg-emerald-500"
                                  : "bg-muted-foreground/40",
                              )}
                            />
                            <h3 className="truncate font-semibold">
                              {provider.name}
                            </h3>
                          </div>
                          <p className="mt-1 truncate text-xs text-muted-foreground">
                            {provider.baseUrl}
                          </p>
                        </div>
                        <div className="flex gap-1">
                          <button
                            className="icon-button"
                            onClick={() => openProvider(provider)}
                          >
                            <Settings className="h-4 w-4" />
                          </button>
                          <button
                            className="icon-button text-destructive"
                            onClick={() => deleteProvider(provider.id)}
                          >
                            <Trash2 className="h-4 w-4" />
                          </button>
                        </div>
                      </div>
                      <div className="mt-4 flex flex-wrap gap-2 text-xs">
                        <span className="tag">
                          {provider.authStyle === "auto"
                            ? "自动鉴权"
                            : provider.authStyle}
                        </span>
                        <span className="tag font-mono">
                          {maskKey(provider.apiKey)}
                        </span>
                        <span className="tag">
                          {provider.models.length} 个模型
                        </span>
                        {(provider.cachedModels?.length ?? 0) > 0 && (
                          <span className="tag">
                            缓存 {provider.cachedModels.length}
                          </span>
                        )}
                      </div>
                      {provider.notes && (
                        <p className="mt-4 line-clamp-2 text-xs text-muted-foreground">
                          {provider.notes}
                        </p>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </section>
          )}

          {tab === "logs" && <RequestLogsPage providers={config.providers} />}

          {tab === "routes" && (
            <section className="mx-auto max-w-5xl">
              <PageHeader
                title="模型路由"
                description="按上游供应商分组管理；每个模型独立选择协议、上游模型名、对外别名。"
                action={
                  <button
                    className="primary-button"
                    disabled={!config.providers.length}
                    onClick={() =>
                      config.providers[0] &&
                      addProviderModel(config.providers[0].id)
                    }
                  >
                    <Plus className="h-4 w-4" />
                    快速添加
                  </button>
                }
              />
              {config.providers.length === 0 ? (
                <EmptyState
                  icon={Route}
                  title="先添加供应商"
                  description="模型路由从供应商派生，需要先在“上游供应商”里添加一个。"
                  action="去添加供应商"
                  onAction={() => setTab("providers")}
                />
              ) : (
                <div className="space-y-4">
                  {enabledAliases.length > 0 && (
                    <div className="panel p-4">
                      <div className="mb-3 flex items-center justify-between gap-3">
                        <div>
                          <div className="text-sm font-semibold">路由策略</div>
                          <div className="mt-0.5 text-xs text-muted-foreground">
                            策略只决定新会话的首选目标；已有会话仍优先保持
                            Session Affinity。
                          </div>
                        </div>
                      </div>
                      <div className="grid gap-2 md:grid-cols-2">
                        {enabledAliases.map((alias) => {
                          const policy =
                            config.routingPolicies?.[alias] ?? "priority";
                          const targets = config.providers.filter(
                            (provider) =>
                              provider.enabled &&
                              provider.models.some(
                                (model) =>
                                  model.enabled && model.alias === alias,
                              ),
                          );
                          return (
                            <div
                              key={alias}
                              className="rounded-md border px-3 py-2"
                            >
                              <div className="flex items-center gap-3">
                                <span className="min-w-0 flex-1 truncate font-mono text-xs">
                                  {alias}
                                </span>
                                <select
                                  className="input w-52"
                                  value={policy}
                                  onChange={(event) =>
                                    setConfig({
                                      ...config,
                                      routingPolicies: {
                                        ...(config.routingPolicies ?? {}),
                                        [alias]: event.target
                                          .value as RoutingPolicy,
                                      },
                                    })
                                  }
                                >
                                  <option value="priority">Priority</option>
                                  <option value="round_robin">
                                    Round Robin
                                  </option>
                                  <option value="weighted_round_robin">
                                    Weighted Round Robin
                                  </option>
                                  <option value="least_outstanding">
                                    Least Outstanding
                                  </option>
                                </select>
                              </div>
                              {policy === "weighted_round_robin" && (
                                <div className="mt-2 grid gap-2 border-t pt-2 sm:grid-cols-2">
                                  {targets.map((provider) => (
                                    <label
                                      key={provider.id}
                                      className="flex items-center gap-2 text-[11px] text-muted-foreground"
                                    >
                                      <span className="min-w-0 flex-1 truncate">
                                        {provider.name || provider.id}
                                      </span>
                                      <input
                                        className="input w-20 font-mono"
                                        type="number"
                                        min={1}
                                        max={1000}
                                        value={
                                          config.routingWeights?.[alias]?.[
                                            provider.id
                                          ] ?? 1
                                        }
                                        onChange={(event) => {
                                          const value = Math.max(
                                            1,
                                            Math.min(
                                              1000,
                                              Number(event.target.value) || 1,
                                            ),
                                          );
                                          setConfig({
                                            ...config,
                                            routingWeights: {
                                              ...(config.routingWeights ?? {}),
                                              [alias]: {
                                                ...(config.routingWeights?.[
                                                  alias
                                                ] ?? {}),
                                                [provider.id]: value,
                                              },
                                            },
                                          });
                                        }}
                                      />
                                    </label>
                                  ))}
                                </div>
                              )}
                            </div>
                          );
                        })}
                      </div>
                    </div>
                  )}
                  {config.providers.map((provider, providerIndex) => (
                    <ProviderRoutesCard
                      key={provider.id}
                      provider={provider}
                      modelRegistryOverrides={
                        config.modelRegistryOverrides ?? {}
                      }
                      onPatchModelRegistryOverride={patchModelRegistryOverride}
                      runtime={providerRuntime.filter(
                        (entry) => entry.sourceProviderId === provider.id,
                      )}
                      providerIndex={providerIndex}
                      canMoveUp={providerIndex > 0}
                      canMoveDown={providerIndex < config.providers.length - 1}
                      expanded={expandedProviders[provider.id] !== false}
                      onToggleExpand={() =>
                        setExpandedProviders((c) => ({
                          ...c,
                          [provider.id]: c[provider.id] === false,
                        }))
                      }
                      onPatch={(patch) => patchProvider(provider.id, patch)}
                      onMove={(direction) =>
                        moveProvider(providerIndex, direction)
                      }
                      onEdit={() => openProvider(provider)}
                      onAddModel={(initial) =>
                        addProviderModel(provider.id, initial)
                      }
                      onPatchModel={(index, patch) =>
                        patchProviderModel(provider.id, index, patch)
                      }
                      onRemoveModel={(index) =>
                        removeProviderModel(provider.id, index)
                      }
                      onTestModel={(modelIndex) =>
                        setTestCtx({
                          providerName: provider.name,
                          provider,
                          modelIndex,
                        })
                      }
                    />
                  ))}
                </div>
              )}
            </section>
          )}

          {tab === "registry" && (
            <ModelRegistryView
              snapshot={modelRegistry}
              overrides={config.modelRegistryOverrides ?? {}}
              onPatchOverride={patchModelRegistryOverride}
              onReload={() => void loadModelRegistry(true)}
            />
          )}

          {tab === "settings" && (
            <section className="mx-auto max-w-4xl">
              <PageHeader
                title="网关设置"
                description="默认只监听本机回环地址，避免 API 暴露到局域网。"
              />
              <VersionInfo onCopy={copyText} />
              <div className="panel divide-y">
                <SettingRow
                  title="监听地址"
                  description="推荐保持 127.0.0.1。只有明确需要局域网访问时才改为 0.0.0.0。"
                >
                  <input
                    className="input w-52 font-mono"
                    value={config.listenAddress}
                    onChange={(event) =>
                      setConfig({
                        ...config,
                        listenAddress: event.target.value,
                      })
                    }
                  />
                </SettingRow>
                <SettingRow
                  title="监听端口"
                  description="其他程序将通过这个端口调用统一 API。"
                >
                  <input
                    className="input w-32 font-mono"
                    type="number"
                    min={1}
                    max={65535}
                    value={config.listenPort}
                    onChange={(event) =>
                      setConfig({
                        ...config,
                        listenPort: Number(event.target.value),
                      })
                    }
                  />
                </SettingRow>
                <SettingRow
                  title="本地访问鉴权"
                  description="OpenAI 客户端使用 Authorization: Bearer；Anthropic 客户端也可使用 x-api-key。"
                >
                  <label className="switch-label">
                    <input
                      type="checkbox"
                      checked={config.requireAuth}
                      onChange={(event) =>
                        setConfig({
                          ...config,
                          requireAuth: event.target.checked,
                        })
                      }
                    />
                    启用
                  </label>
                </SettingRow>
                <SettingRow
                  title="本地 API Key"
                  description="这个 Key 只用于访问本地网关，不会发送给上游。"
                >
                  <div className="flex items-center gap-2">
                    <div className="relative">
                      <input
                        className="input w-80 pr-9 font-mono"
                        type={showLocalKey ? "text" : "password"}
                        value={config.localApiKey}
                        onChange={(event) =>
                          setConfig({
                            ...config,
                            localApiKey: event.target.value,
                          })
                        }
                      />
                      <button
                        className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground"
                        onClick={() => setShowLocalKey(!showLocalKey)}
                      >
                        {showLocalKey ? (
                          <EyeOff className="h-4 w-4" />
                        ) : (
                          <Eye className="h-4 w-4" />
                        )}
                      </button>
                    </div>
                    <button
                      className="secondary-button"
                      onClick={regenerateKey}
                    >
                      <RotateCcw className="h-4 w-4" />
                      重新生成
                    </button>
                  </div>
                </SettingRow>
                <SettingRow
                  title="开机自启动"
                  description="登录系统时自动启动应用。安装版和便携版均支持；便携版移动后，手动运行一次即可自动修复启动路径。"
                >
                  <label className="switch-label">
                    <input
                      type="checkbox"
                      disabled={!startupPreferences || startupSettingsBusy}
                      checked={startupPreferences?.launchOnStartup ?? false}
                      onChange={(event) =>
                        void updateStartupPreference(
                          "set_auto_launch",
                          event.target.checked,
                        )
                      }
                    />
                    启用
                  </label>
                </SettingRow>
                <SettingRow
                  title="以最小化启动"
                  description="仅在“开机自启动”开启时生效。登录后直接进入托盘，不弹出主窗口；手动双击软件仍会正常显示。"
                  nested
                >
                  <label className="switch-label">
                    <input
                      type="checkbox"
                      disabled={
                        !startupPreferences?.launchOnStartup ||
                        startupSettingsBusy
                      }
                      checked={
                        startupPreferences?.startHiddenOnAutostart ?? false
                      }
                      onChange={(event) =>
                        void updateStartupPreference(
                          "set_start_hidden_on_autostart",
                          event.target.checked,
                        )
                      }
                    />
                    启用
                  </label>
                </SettingRow>
                <SettingRow
                  title="应用启动后自动启动网关"
                  description="启动桌面软件后自动监听本地端口，但不会修改或接管其他程序的配置。"
                >
                  <label className="switch-label">
                    <input
                      type="checkbox"
                      checked={config.autoStart}
                      onChange={(event) =>
                        setConfig({
                          ...config,
                          autoStart: event.target.checked,
                        })
                      }
                    />
                    启用
                  </label>
                </SettingRow>
                <SettingRow
                  title="关闭窗口后继续运行"
                  description="点击右上角关闭按钮时隐藏到托盘；关闭后可从托盘菜单重新打开或彻底退出。"
                >
                  <label className="switch-label">
                    <input
                      type="checkbox"
                      disabled={!startupPreferences || startupSettingsBusy}
                      checked={
                        startupPreferences?.minimizeToTrayOnClose ?? true
                      }
                      onChange={(event) =>
                        void updateStartupPreference(
                          "set_minimize_to_tray_on_close",
                          event.target.checked,
                        )
                      }
                    />
                    启用
                  </label>
                </SettingRow>
                <SettingRow
                  title="请求日志"
                  description="记录模型、供应商、延迟、Token 和状态；默认不记录提示词正文。"
                >
                  <label className="switch-label">
                    <input
                      type="checkbox"
                      checked={config.enableLogging}
                      onChange={(event) =>
                        setConfig({
                          ...config,
                          enableLogging: event.target.checked,
                        })
                      }
                    />
                    启用
                  </label>
                </SettingRow>
              </div>

              <div className="mt-6">
                <div className="mb-3 flex items-center gap-2 text-sm font-semibold">
                  <RadioTower className="h-4 w-4 text-primary" />
                  出口代理
                  {savedGlobalProxyUrl ? (
                    <span className="tag text-[10px]">已启用</span>
                  ) : (
                    <span className="tag text-[10px]">直连</span>
                  )}
                </div>
                <div className="panel space-y-3 p-5">
                  <p className="text-xs text-muted-foreground">
                    向上游转发时使用的代理。留空 = 直连。支持 http / https /
                    socks5 / socks5h。
                  </p>
                  <div className="flex items-center gap-2">
                    <input
                      className="input flex-1 font-mono"
                      value={globalProxyUrl}
                      onChange={(event) =>
                        setGlobalProxyUrl(event.target.value)
                      }
                      placeholder="http://127.0.0.1:7890 或 socks5://127.0.0.1:1080"
                    />
                    <button
                      className="secondary-button"
                      disabled={proxyTesting}
                      onClick={() => void testGlobalProxy()}
                    >
                      <Zap
                        className={cn(
                          "h-4 w-4",
                          proxyTesting && "animate-pulse",
                        )}
                      />
                      测试
                    </button>
                    <button
                      className="primary-button"
                      disabled={!proxyDirty || proxyApplying}
                      onClick={() => void applyGlobalProxy()}
                    >
                      {proxyApplying ? (
                        <RefreshCw className="h-4 w-4 animate-spin" />
                      ) : (
                        <Check className="h-4 w-4" />
                      )}
                      应用
                    </button>
                  </div>
                  <div className="flex flex-wrap items-center gap-2">
                    <button
                      className="secondary-button"
                      disabled={scanningProxies}
                      onClick={() => void scanProxies()}
                    >
                      <RefreshCw
                        className={cn(
                          "h-4 w-4",
                          scanningProxies && "animate-spin",
                        )}
                      />
                      扫描本地代理
                    </button>
                    {detectedProxies.map((detected) => (
                      <button
                        key={detected.url}
                        className="tag hover:bg-primary hover:text-primary-foreground"
                        onClick={() => setGlobalProxyUrl(detected.url)}
                      >
                        {detected.url}
                      </button>
                    ))}
                    {globalProxyUrl && (
                      <button
                        className="tag hover:bg-destructive hover:text-destructive-foreground"
                        onClick={() => setGlobalProxyUrl("")}
                      >
                        清空
                      </button>
                    )}
                  </div>
                </div>
              </div>
            </section>
          )}
        </main>
      </div>

      <footer className="flex h-14 shrink-0 items-center justify-between border-t bg-card px-5">
        <div className="text-xs text-muted-foreground">
          {dirty ? "有尚未保存的修改" : "所有修改已保存"}
        </div>
        <div className="flex gap-2">
          {dirty && (
            <button
              className="secondary-button"
              onClick={() => setConfig(savedConfig)}
            >
              <RotateCcw className="h-4 w-4" />
              撤销
            </button>
          )}
          <button
            className="primary-button"
            disabled={!dirty || saving}
            onClick={() => void save()}
          >
            {saving ? (
              <RefreshCw className="h-4 w-4 animate-spin" />
            ) : (
              <Save className="h-4 w-4" />
            )}
            保存配置
          </button>
        </div>
      </footer>

      {providerEditor && (
        <ProviderEditorModal
          provider={providerEditor}
          onChange={setProviderEditor}
          onClose={() => setProviderEditor(null)}
          onSubmit={commitProvider}
          isEditing={!!editingProviderId}
          headersText={headersText}
          onHeadersText={setHeadersText}
          showProviderKey={showProviderKey}
          onToggleProviderKey={() => setShowProviderKey(!showProviderKey)}
          modelsFetchFormat={modelsFetchFormat}
          onModelsFetchFormat={setModelsFetchFormat}
          fetchingModels={fetchingModels}
          onFetchModels={() => void fetchProviderModels()}
        />
      )}

      {testCtx && (
        <ModelTestModal
          ctx={testCtx}
          gatewayRunning={!!status?.running}
          savedProviders={savedConfig.providers}
          onCopy={copyText}
          onClose={() => setTestCtx(null)}
        />
      )}
    </div>
  );
}

function ModelRegistryView({
  snapshot,
  overrides,
  onPatchOverride,
  onReload,
}: {
  snapshot: ModelRegistrySnapshot | null;
  overrides: Record<string, ModelMetadata>;
  onPatchOverride: (
    canonicalModel: string,
    metadata: ModelMetadata | null,
  ) => void;
  onReload: () => void;
}) {
  return (
    <section className="mx-auto max-w-5xl">
      <PageHeader
        title="模型元数据"
        description="统一维护模型上下文、输出限制、输入模态和 Reasoning 档位。内置 Registry 作为默认值，用户修改仅保存为全局 override。"
        action={
          <button className="secondary-button" onClick={onReload}>
            <RefreshCw className="h-4 w-4" />
            重新读取
          </button>
        }
      />

      {!snapshot ? (
        <div className="panel flex items-center gap-2 p-5 text-sm text-muted-foreground">
          <RefreshCw className="h-4 w-4 animate-spin" />
          正在读取 Model Registry…
        </div>
      ) : (
        <div className="space-y-4">
          <div className="panel p-4">
            <div className="text-xs font-medium">Override 文件</div>
            <div className="mt-1 break-all font-mono text-[11px] text-muted-foreground">
              {snapshot.overrideFile}
            </div>
            <div className="mt-2 text-[11px] leading-5 text-muted-foreground">
              便携模式下该路径位于可执行文件旁的 data
              目录；安装版使用应用数据目录。软件升级只更新内置默认值，不会覆盖这里的用户修改。
            </div>
          </div>

          {snapshot.entries.map((entry) => {
            const override = overrides[entry.canonicalModel];
            const base = entry.baseMetadata ?? {};
            const effective: ModelMetadata = {
              contextLength:
                override?.contextLength ?? base.contextLength ?? null,
              maxOutputTokens:
                override?.maxOutputTokens ?? base.maxOutputTokens ?? null,
              inputModalities: override?.inputModalities?.length
                ? override.inputModalities
                : (base.inputModalities ?? []),
              reasoningLevels: override?.reasoningLevels?.length
                ? override.reasoningLevels
                : (base.reasoningLevels ?? []),
            };
            const patch = (next: Partial<ModelMetadata>) =>
              onPatchOverride(entry.canonicalModel, {
                ...override,
                ...next,
              });
            const reasoningChoices = Array.from(
              new Set([
                ...COMMON_REASONING_LEVELS,
                ...(base.reasoningLevels ?? []),
                ...(override?.reasoningLevels ?? []),
              ]),
            );

            return (
              <div key={entry.canonicalModel} className="panel p-5">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <h3 className="font-mono text-sm font-semibold">
                        {entry.canonicalModel}
                      </h3>
                      <span className="tag text-[10px]">
                        {hasMetadataValues(override)
                          ? "全局覆盖"
                          : hasMetadataValues(base)
                            ? "Built-in"
                            : "未知 · 可补全"}
                      </span>
                      {entry.defaultReasoningLevel && (
                        <span className="tag text-[10px]">
                          默认 reasoning: {entry.defaultReasoningLevel}
                        </span>
                      )}
                    </div>
                    {entry.ids.length > 1 && (
                      <div className="mt-1 break-all text-[10px] text-muted-foreground">
                        aliases: {entry.ids.join(", ")}
                      </div>
                    )}
                  </div>
                  {hasMetadataValues(override) && (
                    <button
                      className="secondary-button"
                      onClick={() =>
                        onPatchOverride(entry.canonicalModel, null)
                      }
                    >
                      <RotateCcw className="h-4 w-4" />
                      恢复默认
                    </button>
                  )}
                </div>

                <div className="mt-4 grid gap-3 md:grid-cols-2">
                  <Field label="Context length">
                    <input
                      className="input w-full font-mono"
                      type="number"
                      min={1}
                      value={override?.contextLength ?? ""}
                      placeholder={
                        base.contextLength ? String(base.contextLength) : "未知"
                      }
                      onChange={(event) => {
                        const value = event.target.value.trim();
                        patch({ contextLength: value ? Number(value) : null });
                      }}
                    />
                  </Field>
                  <Field label="Max output tokens">
                    <input
                      className="input w-full font-mono"
                      type="number"
                      min={1}
                      value={override?.maxOutputTokens ?? ""}
                      placeholder={
                        base.maxOutputTokens
                          ? String(base.maxOutputTokens)
                          : "未知"
                      }
                      onChange={(event) => {
                        const value = event.target.value.trim();
                        patch({
                          maxOutputTokens: value ? Number(value) : null,
                        });
                      }}
                    />
                  </Field>
                </div>

                <div className="mt-4">
                  <div className="mb-1 text-[11px] text-muted-foreground">
                    输入模态
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {COMMON_MODALITIES.map((value) => {
                      const current = effective.inputModalities ?? [];
                      const active = current.includes(value);
                      return (
                        <button
                          key={value}
                          className={cn(
                            "tag",
                            active && "bg-primary text-primary-foreground",
                          )}
                          onClick={() =>
                            patch({
                              inputModalities: active
                                ? current.filter((item) => item !== value)
                                : [...current, value],
                            })
                          }
                        >
                          {value}
                        </button>
                      );
                    })}
                  </div>
                </div>

                <div className="mt-4">
                  <div className="mb-1 text-[11px] text-muted-foreground">
                    Reasoning 档位
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {reasoningChoices.map((value) => {
                      const current = effective.reasoningLevels ?? [];
                      const active = current.includes(value);
                      return (
                        <button
                          key={value}
                          className={cn(
                            "tag",
                            active && "bg-primary text-primary-foreground",
                          )}
                          onClick={() =>
                            patch({
                              reasoningLevels: active
                                ? current.filter((item) => item !== value)
                                : [...current, value],
                            })
                          }
                        >
                          {value}
                        </button>
                      );
                    })}
                  </div>
                </div>

                <div className="mt-4 flex flex-wrap gap-2 text-[10px] text-muted-foreground">
                  <span className="tag">
                    Effective Context{" "}
                    {formatTokenLimit(effective.contextLength)}
                  </span>
                  <span className="tag">
                    Effective Output{" "}
                    {formatTokenLimit(effective.maxOutputTokens)}
                  </span>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

function ProviderRoutesCard({
  provider,
  modelRegistryOverrides,
  onPatchModelRegistryOverride,
  runtime,
  providerIndex,
  canMoveUp,
  canMoveDown,
  expanded,
  onToggleExpand,
  onPatch,
  onMove,
  onEdit,
  onAddModel,
  onPatchModel,
  onRemoveModel,
  onTestModel,
}: {
  provider: GatewayProvider;
  modelRegistryOverrides: Record<string, ModelMetadata>;
  onPatchModelRegistryOverride: (
    canonicalModel: string,
    metadata: ModelMetadata | null,
  ) => void;
  runtime: ProviderRuntimeStatus[];
  providerIndex: number;
  canMoveUp: boolean;
  canMoveDown: boolean;
  expanded: boolean;
  onToggleExpand: () => void;
  onPatch: (patch: Partial<GatewayProvider>) => void;
  onMove: (direction: -1 | 1) => void;
  onEdit: () => void;
  onAddModel: (initial?: Partial<ProviderModel>) => void;
  onPatchModel: (modelIndex: number, patch: Partial<ProviderModel>) => void;
  onRemoveModel: (modelIndex: number) => void;
  onTestModel: (modelIndex: number) => void;
}) {
  const abnormalRuntime = runtime.filter(
    (entry) => entry.cooldownSeconds || entry.circuitState !== "closed",
  );
  const capacityRuntime = runtime.find(
    (entry) => entry.maxConcurrentRequests > 0,
  );
  const hasQueuedRequests = (capacityRuntime?.queuedRequests ?? 0) > 0;
  return (
    <div className="panel overflow-hidden">
      <div className="flex items-center gap-3 border-b p-4">
        <button
          className="icon-button"
          onClick={onToggleExpand}
          aria-label="展开/收起"
        >
          {expanded ? (
            <ChevronDown className="h-4 w-4" />
          ) : (
            <ChevronRight className="h-4 w-4" />
          )}
        </button>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span
              className={cn(
                "h-2.5 w-2.5 rounded-full",
                provider.enabled ? "bg-emerald-500" : "bg-muted-foreground/40",
              )}
            />
            <span className="truncate font-semibold">
              {provider.name || "(未命名)"}
            </span>
            <span className="text-[11px] text-muted-foreground">
              #{providerIndex + 1}
            </span>
            {abnormalRuntime.length === 0 &&
            runtime.length > 0 &&
            !hasQueuedRequests ? (
              <span className="tag text-[10px] text-emerald-600">健康</span>
            ) : (
              abnormalRuntime.slice(0, 4).map((entry) => (
                <span
                  key={`${entry.appType}:${entry.materializedProviderId}`}
                  className="tag text-[10px] text-amber-600"
                  title={`${entry.appType} · ${formatLabels[entry.apiFormat]} · failures ${entry.consecutiveFailures} · requests ${entry.totalRequests}`}
                >
                  {entry.appType} · {formatLabels[entry.apiFormat]} ·{" "}
                  {entry.cooldownSeconds
                    ? `冷却 ${entry.cooldownSeconds}s`
                    : entry.circuitState}
                </span>
              ))
            )}
            {capacityRuntime && (
              <span
                className={cn(
                  "tag text-[10px]",
                  capacityRuntime.queuedRequests > 0 ||
                    capacityRuntime.activeRequests >=
                      capacityRuntime.maxConcurrentRequests
                    ? "text-amber-600"
                    : "text-muted-foreground",
                )}
                title={`并发 ${capacityRuntime.activeRequests}/${capacityRuntime.maxConcurrentRequests} · 排队 ${capacityRuntime.queuedRequests}`}
              >
                并发 {capacityRuntime.activeRequests}/
                {capacityRuntime.maxConcurrentRequests}
                {capacityRuntime.queuedRequests > 0
                  ? ` · 排队 ${capacityRuntime.queuedRequests}`
                  : ""}
              </span>
            )}
          </div>
          <div className="mt-0.5 truncate text-[11px] text-muted-foreground">
            {provider.baseUrl}
          </div>
        </div>
        <label className="switch-label">
          <input
            type="checkbox"
            checked={provider.enabled}
            onChange={(event) => onPatch({ enabled: event.target.checked })}
          />
          启用
        </label>
        <button
          className="icon-button"
          disabled={!canMoveUp}
          onClick={() => onMove(-1)}
        >
          <ChevronUp className="h-4 w-4" />
        </button>
        <button
          className="icon-button"
          disabled={!canMoveDown}
          onClick={() => onMove(1)}
        >
          <ChevronDown className="h-4 w-4" />
        </button>
        <button className="icon-button" onClick={onEdit} title="编辑供应商">
          <Settings className="h-4 w-4" />
        </button>
      </div>

      {expanded && (
        <div className="space-y-2 p-4">
          {provider.models.length === 0 ? (
            <div className="rounded-lg border border-dashed p-4 text-center text-xs text-muted-foreground">
              还没有模型条目。点击下方“添加模型”从上游模型列表中选择，或手动填写。
            </div>
          ) : (
            provider.models.map((model, index) => (
              <ModelRow
                key={index}
                model={model}
                cachedModels={provider.cachedModels ?? []}
                modelRegistryOverrides={modelRegistryOverrides}
                onPatchModelRegistryOverride={onPatchModelRegistryOverride}
                onPatch={(patch) => onPatchModel(index, patch)}
                onRemove={() => onRemoveModel(index)}
                onTest={() => onTestModel(index)}
              />
            ))
          )}

          <div className="flex flex-wrap gap-2 pt-2">
            <button className="secondary-button" onClick={() => onAddModel()}>
              <Plus className="h-4 w-4" />
              添加模型
            </button>
            {(provider.cachedModels ?? []).slice(0, 6).map((cached) => (
              <button
                key={cached.id}
                className="tag hover:bg-primary hover:text-primary-foreground"
                onClick={() =>
                  onAddModel({
                    upstreamModel: cached.id,
                    alias: cached.id,
                  })
                }
                title={cached.displayName || cached.id}
              >
                + {cached.id}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function ModelRow({
  model,
  cachedModels,
  modelRegistryOverrides,
  onPatchModelRegistryOverride,
  onPatch,
  onRemove,
  onTest,
}: {
  model: ProviderModel;
  cachedModels: CachedModel[];
  modelRegistryOverrides: Record<string, ModelMetadata>;
  onPatchModelRegistryOverride: (
    canonicalModel: string,
    metadata: ModelMetadata | null,
  ) => void;
  onPatch: (patch: Partial<ProviderModel>) => void;
  onRemove: () => void;
  onTest: () => void;
}) {
  const listId = useMemo(() => `cached-models-${newId("dl")}`, []);
  const [registryCapability, setRegistryCapability] =
    useState<ModelCapabilityResolution | null>(null);
  const [showMetadataOverride, setShowMetadataOverride] = useState(false);
  const cachedMetadata = useMemo(
    () =>
      cachedModels.find((item) => item.id === model.upstreamModel)?.metadata,
    [cachedModels, model.upstreamModel],
  );

  useEffect(() => {
    const modelId = model.upstreamModel.trim();
    if (!modelId) {
      setRegistryCapability(null);
      return;
    }
    let cancelled = false;
    void invoke<ModelCapabilityResolution>(
      "resolve_gateway_model_capabilities",
      {
        request: { modelId, apiFormat: model.apiFormat },
      },
    )
      .then((result) => {
        if (!cancelled) setRegistryCapability(result);
      })
      .catch(() => {
        if (!cancelled) setRegistryCapability(null);
      });
    return () => {
      cancelled = true;
    };
  }, [model.upstreamModel, model.apiFormat]);

  const canonicalModel = registryCapability?.canonicalModel ?? "";
  const globalOverride = canonicalModel
    ? modelRegistryOverrides[canonicalModel]
    : undefined;
  const globalAutoMetadata: ModelMetadata = {
    contextLength:
      registryCapability?.metadata.contextLength ??
      cachedMetadata?.contextLength ??
      null,
    maxOutputTokens:
      registryCapability?.metadata.maxOutputTokens ??
      cachedMetadata?.maxOutputTokens ??
      null,
    inputModalities: firstNonEmptyList(
      registryCapability?.metadata.inputModalities,
      cachedMetadata?.inputModalities,
    ),
    reasoningLevels: firstNonEmptyList(
      registryCapability?.metadata.reasoningLevels,
      cachedMetadata?.reasoningLevels,
    ),
  };
  const registryOrGlobalMetadata: ModelMetadata = {
    contextLength:
      globalOverride?.contextLength ?? globalAutoMetadata.contextLength,
    maxOutputTokens:
      globalOverride?.maxOutputTokens ?? globalAutoMetadata.maxOutputTokens,
    inputModalities: firstNonEmptyList(
      globalOverride?.inputModalities,
      globalAutoMetadata.inputModalities,
    ),
    reasoningLevels: firstNonEmptyList(
      globalOverride?.reasoningLevels,
      globalAutoMetadata.reasoningLevels,
    ),
  };

  const effectiveMetadata: ModelMetadata = {
    contextLength: conservativeLimit(
      model.metadata?.contextLength,
      cachedMetadata?.contextLength,
      registryOrGlobalMetadata.contextLength,
    ),
    maxOutputTokens: conservativeLimit(
      model.metadata?.maxOutputTokens,
      cachedMetadata?.maxOutputTokens,
      registryOrGlobalMetadata.maxOutputTokens,
    ),
    inputModalities: conservativeListCapability(
      model.metadata?.inputModalities,
      cachedMetadata?.inputModalities,
      registryOrGlobalMetadata.inputModalities,
    ),
    reasoningLevels: conservativeListCapability(
      model.metadata?.reasoningLevels,
      cachedMetadata?.reasoningLevels,
      registryOrGlobalMetadata.reasoningLevels,
    ),
  };
  const legacyRouteOverrideActive = hasMetadataValues(model.metadata);
  const globalOverrideActive = hasMetadataValues(globalOverride);
  const capabilityKnown = hasMetadataValues(effectiveMetadata);
  const capabilitySource = legacyRouteOverrideActive
    ? "旧路由覆盖"
    : globalOverrideActive
      ? "全局覆盖"
      : hasMetadataValues(cachedMetadata)
        ? "自动（上游 / Registry）"
        : registryCapability?.source === "registry"
          ? "Registry"
          : "未知";
  const reasoningChoices = Array.from(
    new Set([
      ...COMMON_REASONING_LEVELS,
      ...(effectiveMetadata.reasoningLevels ?? []),
      ...(globalOverride?.reasoningLevels ?? []),
    ]),
  );

  const patchGlobalMetadata = (patch: Partial<ModelMetadata>) => {
    if (!canonicalModel) return;
    onPatchModelRegistryOverride(canonicalModel, {
      ...globalOverride,
      ...patch,
    });
  };

  return (
    <div className="flex flex-wrap items-center gap-2">
      <input
        className="input min-w-[130px] flex-1 font-mono"
        value={model.alias}
        onChange={(event) => onPatch({ alias: event.target.value })}
        placeholder="本地别名，如 my-best-code"
      />
      <div className="min-w-[150px] flex-[1.2]">
        <input
          className="input w-full font-mono"
          list={listId}
          value={model.upstreamModel}
          onChange={(event) => {
            const upstreamModel = event.target.value;
            onPatch({
              upstreamModel,
              metadata: {},
            });
          }}
          placeholder="上游真实模型名"
        />
        <datalist id={listId}>
          {cachedModels.map((cached) => (
            <option key={cached.id} value={cached.id}>
              {cached.displayName || cached.ownedBy || cached.id}
            </option>
          ))}
        </datalist>
      </div>
      <select
        className="input min-w-[140px] flex-[0.9]"
        value={model.apiFormat}
        onChange={(event) =>
          onPatch({ apiFormat: event.target.value as ApiFormat })
        }
      >
        <option value="openai_chat">OpenAI Chat</option>
        <option value="openai_responses">OpenAI Responses</option>
        <option value="anthropic">Anthropic Messages</option>
      </select>
      <label className="switch-label">
        <input
          type="checkbox"
          checked={model.enabled}
          onChange={(event) => onPatch({ enabled: event.target.checked })}
        />
        启用
      </label>
      <select
        className="input"
        title="单独控制此模型的请求/响应体录制：跟随供应商 / 强制开启 / 强制排除"
        value={
          model.recordBodies === true
            ? "on"
            : model.recordBodies === false
              ? "off"
              : "follow"
        }
        onChange={(event) => {
          // 三态直映后端 Option<bool>：null=跟随供应商，true=强制开，false=强制排除。
          const next = event.target.value;
          onPatch({
            recordBodies: next === "on" ? true : next === "off" ? false : null,
          });
        }}
      >
        <option value="follow">录制：跟随供应商</option>
        <option value="on">录制：强制开启</option>
        <option value="off">录制：强制排除</option>
      </select>
      <div className="ml-auto flex gap-1">
        <button className="icon-button" onClick={onTest} title="测试此模型">
          <Send className="h-4 w-4" />
        </button>
        <button
          className="icon-button text-destructive"
          onClick={onRemove}
          title="删除"
        >
          <Trash2 className="h-4 w-4" />
        </button>
      </div>
      <div className="basis-full rounded-md border bg-muted/30 p-3">
        <div className="flex flex-wrap items-center gap-2">
          <div className="text-[11px] font-medium text-muted-foreground">
            模型能力
          </div>
          <span className="tag text-[10px]">{capabilitySource}</span>
          {registryCapability?.canonicalModel && (
            <span className="text-[10px] text-muted-foreground">
              canonical: {registryCapability.canonicalModel}
            </span>
          )}
          <button
            className="ml-auto text-[11px] text-muted-foreground underline-offset-2 hover:underline"
            onClick={() => setShowMetadataOverride((value) => !value)}
          >
            {showMetadataOverride ? "收起高级覆盖" : "高级覆盖"}
          </button>
        </div>

        {capabilityKnown ? (
          <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
            <span className="tag">
              Context {formatTokenLimit(effectiveMetadata.contextLength)}
            </span>
            <span className="tag">
              Max output {formatTokenLimit(effectiveMetadata.maxOutputTokens)}
            </span>
            {(effectiveMetadata.inputModalities ?? []).map((value) => (
              <span key={`modality-${value}`} className="tag">
                {value}
              </span>
            ))}
            {(effectiveMetadata.reasoningLevels ?? []).length > 0 && (
              <span className="flex flex-wrap items-center gap-1">
                <span className="text-muted-foreground">Reasoning:</span>
                {(effectiveMetadata.reasoningLevels ?? []).map((value) => (
                  <span key={`reasoning-${value}`} className="tag">
                    {value}
                  </span>
                ))}
              </span>
            )}
            {registryCapability?.defaultReasoningLevel &&
              (effectiveMetadata.reasoningLevels ?? []).includes(
                registryCapability.defaultReasoningLevel,
              ) && (
                <span className="tag">
                  默认 reasoning: {registryCapability.defaultReasoningLevel}
                </span>
              )}
          </div>
        ) : (
          <div className="mt-2 text-[11px] text-muted-foreground">
            Registry
            与上游模型列表都没有提供能力信息。可以保持未知，只有确实需要时再使用高级覆盖。
          </div>
        )}

        {showMetadataOverride && (
          <div className="mt-3 space-y-3 rounded-md border bg-background p-3">
            <div className="flex items-center justify-between gap-3">
              <div className="text-[11px] text-muted-foreground">
                全局覆盖 canonical 模型 {canonicalModel || "（尚未识别）"}；所有
                Provider 共用。正常情况下保持自动即可。
              </div>
              <div className="flex flex-wrap gap-2">
                {globalOverrideActive && canonicalModel && (
                  <button
                    className="secondary-button"
                    onClick={() =>
                      onPatchModelRegistryOverride(canonicalModel, null)
                    }
                  >
                    清除全局覆盖
                  </button>
                )}
                {legacyRouteOverrideActive && (
                  <button
                    className="secondary-button"
                    onClick={() => onPatch({ metadata: {} })}
                    title="旧版配置可能在每条路由保存 metadata；清除后改用全局 Registry"
                  >
                    清除旧路由覆盖
                  </button>
                )}
              </div>
            </div>
            <div className="grid gap-2 md:grid-cols-2">
              <input
                className="input font-mono"
                type="number"
                min={1}
                value={globalOverride?.contextLength ?? ""}
                onChange={(event) => {
                  const value = event.target.value.trim();
                  patchGlobalMetadata({
                    contextLength: value ? Number(value) : null,
                  });
                }}
                placeholder={`Context；Registry ${formatTokenLimit(globalAutoMetadata.contextLength)}`}
              />
              <input
                className="input font-mono"
                type="number"
                min={1}
                value={globalOverride?.maxOutputTokens ?? ""}
                onChange={(event) => {
                  const value = event.target.value.trim();
                  patchGlobalMetadata({
                    maxOutputTokens: value ? Number(value) : null,
                  });
                }}
                placeholder={`Max output；Registry ${formatTokenLimit(globalAutoMetadata.maxOutputTokens)}`}
              />
            </div>

            <div>
              <div className="mb-1 text-[11px] text-muted-foreground">
                输入模态
              </div>
              <div className="flex flex-wrap gap-1">
                {COMMON_MODALITIES.map((value) => {
                  const current = globalOverride?.inputModalities?.length
                    ? globalOverride.inputModalities
                    : (globalAutoMetadata.inputModalities ?? []);
                  const active = current.includes(value);
                  return (
                    <button
                      key={value}
                      className={cn(
                        "tag",
                        active && "bg-primary text-primary-foreground",
                      )}
                      onClick={() => {
                        const next = active
                          ? current.filter((item) => item !== value)
                          : [...current, value];
                        patchGlobalMetadata({ inputModalities: next });
                      }}
                    >
                      {value}
                    </button>
                  );
                })}
              </div>
            </div>

            <div>
              <div className="mb-1 text-[11px] text-muted-foreground">
                Reasoning 档位（无需手写）
              </div>
              <div className="flex flex-wrap gap-1">
                {reasoningChoices.map((value) => {
                  const current = globalOverride?.reasoningLevels?.length
                    ? globalOverride.reasoningLevels
                    : (globalAutoMetadata.reasoningLevels ?? []);
                  const active = current.includes(value);
                  return (
                    <button
                      key={value}
                      className={cn(
                        "tag",
                        active && "bg-primary text-primary-foreground",
                      )}
                      onClick={() => {
                        const next = active
                          ? current.filter((item) => item !== value)
                          : [...current, value];
                        patchGlobalMetadata({ reasoningLevels: next });
                      }}
                    >
                      {value}
                    </button>
                  );
                })}
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function ProviderEditorModal({
  provider,
  onChange,
  onClose,
  onSubmit,
  isEditing,
  headersText,
  onHeadersText,
  showProviderKey,
  onToggleProviderKey,
  modelsFetchFormat,
  onModelsFetchFormat,
  fetchingModels,
  onFetchModels,
}: {
  provider: GatewayProvider;
  onChange: (next: GatewayProvider) => void;
  onClose: () => void;
  onSubmit: () => void;
  isEditing: boolean;
  headersText: string;
  onHeadersText: (value: string) => void;
  showProviderKey: boolean;
  onToggleProviderKey: () => void;
  modelsFetchFormat: ApiFormat;
  onModelsFetchFormat: (value: ApiFormat) => void;
  fetchingModels: boolean;
  onFetchModels: () => void;
}) {
  const openRecordingFolder = async () => {
    try {
      await invoke("open_gateway_recording_folder");
    } catch (error) {
      toast.error(`打开录制目录失败：${String(error)}`);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-6"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="panel max-h-[90vh] w-full max-w-2xl overflow-y-auto p-6 shadow-2xl">
        <div className="mb-5 flex items-center justify-between">
          <div>
            <h2 className="text-lg font-semibold">
              {isEditing ? "编辑供应商" : "添加供应商"}
            </h2>
            <p className="mt-1 text-xs text-muted-foreground">
              协议不再绑定在供应商上，请在“模型路由”里为每个模型独立选择。
            </p>
          </div>
          <button className="icon-button" onClick={onClose}>
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="名称">
            <input
              className="input w-full"
              value={provider.name}
              onChange={(event) =>
                onChange({ ...provider, name: event.target.value })
              }
              placeholder="例如 OpenRouter"
            />
          </Field>
          <Field label="状态">
            <label className="switch-label h-10">
              <input
                type="checkbox"
                checked={provider.enabled}
                onChange={(event) =>
                  onChange({ ...provider, enabled: event.target.checked })
                }
              />
              启用供应商
            </label>
          </Field>
          <div className="sm:col-span-2">
            <Field label="Base URL">
              <input
                className="input w-full font-mono"
                value={provider.baseUrl}
                onChange={(event) =>
                  onChange({ ...provider, baseUrl: event.target.value })
                }
                placeholder="https://api.example.com/v1"
              />
            </Field>
          </div>
          <div className="sm:col-span-2">
            <Field label="API Key">
              <div className="relative">
                <input
                  className="input w-full pr-10 font-mono"
                  type={showProviderKey ? "text" : "password"}
                  value={provider.apiKey}
                  onChange={(event) =>
                    onChange({ ...provider, apiKey: event.target.value })
                  }
                  placeholder="sk-..."
                />
                <button
                  className="absolute right-3 top-1/2 -translate-y-1/2 text-muted-foreground"
                  onClick={onToggleProviderKey}
                >
                  {showProviderKey ? (
                    <EyeOff className="h-4 w-4" />
                  ) : (
                    <Eye className="h-4 w-4" />
                  )}
                </button>
              </div>
            </Field>
          </div>
          <Field label="鉴权方式">
            <select
              className="input w-full"
              value={provider.authStyle}
              onChange={(event) =>
                onChange({
                  ...provider,
                  authStyle: event.target.value as GatewayProvider["authStyle"],
                })
              }
            >
              <option value="auto">自动</option>
              <option value="bearer">Authorization: Bearer</option>
              <option value="x-api-key">x-api-key</option>
            </select>
          </Field>
          <Field label="模型列表 URL（可选）">
            <input
              className="input w-full font-mono"
              value={provider.modelsUrl}
              onChange={(event) =>
                onChange({ ...provider, modelsUrl: event.target.value })
              }
              placeholder="留空时自动尝试 /v1/models"
            />
          </Field>
          <div className="sm:col-span-2">
            <Field label="自定义 User-Agent（可选）">
              <input
                className="input w-full font-mono"
                value={provider.customUserAgent}
                onChange={(event) =>
                  onChange({ ...provider, customUserAgent: event.target.value })
                }
                placeholder="例如 MyClient/1.0"
              />
            </Field>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              用于上游公开兼容要求；不会注入官方客户端私有令牌、身份提示词或设备指纹。
            </p>
          </div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <label className="switch-label">
              <input
                type="checkbox"
                checked={provider.impersonateCodexClient}
                onChange={(event) =>
                  onChange({
                    ...provider,
                    impersonateCodexClient: event.target.checked,
                  })
                }
              />
              以 Codex 客户端身份转发
            </label>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              部分上游会校验客户端指纹（返回{" "}
              <code>unauthorized client detected</code>）。开启后转发时发送{" "}
              <code>codex_cli_rs</code> 的 User-Agent 及成对的{" "}
              <code>originator</code>/<code>version</code> 兼容标识。
            </p>
            {provider.impersonateCodexClient && (
              <div className="mt-2">
                <Field label="Codex 版本（可选）">
                  <input
                    className="input w-full font-mono"
                    value={provider.codexClientVersion}
                    onChange={(event) =>
                      onChange({
                        ...provider,
                        codexClientVersion: event.target.value,
                      })
                    }
                    placeholder="留空使用默认 0.144.1"
                  />
                </Field>
              </div>
            )}
          </div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <div className="text-xs font-medium">容量与背压</div>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              并发上限为 0 时不限制。达到上限后优先尝试其它可用
              Provider；全部满载时才进入有界队列。
            </p>
            <div className="mt-3 grid gap-3 sm:grid-cols-3">
              <Field label="最大并发">
                <input
                  className="input w-full font-mono"
                  type="number"
                  min={0}
                  max={10000}
                  value={provider.maxConcurrentRequests ?? 0}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      maxConcurrentRequests: Math.max(
                        0,
                        Number(event.target.value) || 0,
                      ),
                    })
                  }
                  placeholder="0 = 不限"
                />
              </Field>
              <Field label="排队上限">
                <input
                  className="input w-full font-mono"
                  type="number"
                  min={0}
                  max={100000}
                  value={provider.queueLimit ?? 32}
                  disabled={(provider.maxConcurrentRequests ?? 0) === 0}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      queueLimit: Math.max(0, Number(event.target.value) || 0),
                    })
                  }
                />
              </Field>
              <Field label="排队超时 (ms)">
                <input
                  className="input w-full font-mono"
                  type="number"
                  min={0}
                  max={3600000}
                  value={provider.queueTimeoutMs ?? 30000}
                  disabled={(provider.maxConcurrentRequests ?? 0) === 0}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      queueTimeoutMs: Math.max(
                        0,
                        Number(event.target.value) || 0,
                      ),
                    })
                  }
                />
              </Field>
            </div>
          </div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <div className="text-xs font-medium">推理兼容（高级）</div>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              默认保持自动判断。只有第三方接口不返回思考、错误发送推理参数导致
              400，或原生 Claude adaptive thinking 不展示摘要时才需要调整。
            </p>
            <div className="mt-3 grid gap-3 sm:grid-cols-3">
              <Field label="Chat 工具兼容">
                <label className="switch-label">
                  <input
                    type="checkbox"
                    checked={provider.chatSchemaRequiredDefaults}
                    onChange={(event) =>
                      onChange({
                        ...provider,
                        chatSchemaRequiredDefaults: event.target.checked,
                      })
                    }
                  />
                  补齐缺省 required 数组
                </label>
                <p className="mt-1 text-[11px] text-muted-foreground">
                  仅用于要求此字段的中转接口；保留已有约束与 strict 工具。
                </p>
              </Field>
              <Field label="Chat 推理接口">
                <select
                  className="input w-full"
                  value={provider.chatReasoningProfile}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      chatReasoningProfile: event.target
                        .value as GatewayProvider["chatReasoningProfile"],
                    })
                  }
                >
                  <option value="auto">自动（原生 Chat 保真）</option>
                  <option value="openai">OpenAI（reasoning_effort）</option>
                  <option value="deepseek">DeepSeek 官方格式</option>
                  <option value="openrouter">OpenRouter</option>
                  <option value="siliconflow">SiliconFlow</option>
                  <option value="disabled">不发送推理参数</option>
                </select>
              </Field>
              <Field label="思考请求处理">
                <select
                  className="input w-full"
                  value={provider.reasoningRequestMode}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      reasoningRequestMode: event.target
                        .value as GatewayProvider["reasoningRequestMode"],
                    })
                  }
                >
                  <option value="auto">自动</option>
                  <option value="force">强制映射（仍校验已知能力）</option>
                  <option value="disabled">不发送思考控制参数</option>
                </select>
              </Field>
              <Field label="历史推理回传">
                <select
                  className="input w-full"
                  value={provider.reasoningHistoryMode}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      reasoningHistoryMode: event.target
                        .value as GatewayProvider["reasoningHistoryMode"],
                    })
                  }
                >
                  <option value="auto">自动</option>
                  <option value="reasoning_content">reasoning_content</option>
                  <option value="disabled">关闭</option>
                </select>
              </Field>
              <Field label="Adaptive 展示">
                <select
                  className="input w-full"
                  value={provider.adaptiveThinkingDisplay}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      adaptiveThinkingDisplay: event.target
                        .value as GatewayProvider["adaptiveThinkingDisplay"],
                    })
                  }
                >
                  <option value="auto">自动（不改写）</option>
                  <option value="summarized">显示摘要</option>
                  <option value="omitted">不显示摘要</option>
                </select>
              </Field>
            </div>
          </div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <label className="switch-label">
                <input
                  type="checkbox"
                  checked={provider.recordBodies ?? false}
                  onChange={(event) =>
                    onChange({
                      ...provider,
                      recordBodies: event.target.checked,
                    })
                  }
                />
                录制请求/响应体（诊断）
              </label>
              <button
                className="secondary-button"
                onClick={() => void openRecordingFolder()}
              >
                <FolderOpen className="h-3.5 w-3.5" />
                打开录制目录
              </button>
            </div>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              把实际发往该供应商的最终请求体和上游响应体转储为 JSONL 文件
              （日志目录下 <code>request-bodies/</code>），用于核对中转站实际
              收到的内容。也可在下方单个模型上按“跟随供应商 / 强制开启 /
              强制排除”三态单独控制。含完整对话内容，排查完成后请及时关闭。
            </p>
          </div>
          <div className="sm:col-span-2">
            <Field label="自定义请求头（每行一个 Header: Value）">
              <textarea
                className="input min-h-24 w-full resize-y font-mono text-xs"
                value={headersText}
                onChange={(event) => onHeadersText(event.target.value)}
                placeholder={
                  "HTTP-Referer: https://example.com\nX-Title: My Gateway"
                }
              />
            </Field>
          </div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <div className="text-xs font-medium">
                  上游模型列表（供“模型路由”自动补全）
                </div>
                <div className="mt-1 text-[11px] text-muted-foreground">
                  {provider.cachedModels.length
                    ? `已缓存 ${provider.cachedModels.length} 个模型`
                    : "尚未获取"}
                  {provider.modelsFetchedAt
                    ? ` · ${new Date(provider.modelsFetchedAt).toLocaleString()}`
                    : ""}
                </div>
              </div>
              <div className="flex items-center gap-2">
                <select
                  className="input"
                  value={modelsFetchFormat}
                  onChange={(event) =>
                    onModelsFetchFormat(event.target.value as ApiFormat)
                  }
                >
                  <option value="openai_chat">OpenAI Chat</option>
                  <option value="openai_responses">OpenAI Responses</option>
                  <option value="anthropic">Anthropic Messages</option>
                </select>
                <button
                  className="secondary-button"
                  disabled={fetchingModels}
                  onClick={onFetchModels}
                >
                  <RefreshCw
                    className={cn("h-4 w-4", fetchingModels && "animate-spin")}
                  />
                  {fetchingModels ? "获取中" : "获取模型"}
                </button>
              </div>
            </div>
            {provider.cachedModels.length > 0 && (
              <div className="mt-3 max-h-36 overflow-y-auto rounded border bg-background p-2 font-mono text-[11px] leading-5">
                {provider.cachedModels.map((model) => (
                  <div
                    key={model.id}
                    className="truncate"
                    title={model.displayName || model.id}
                  >
                    {model.id}
                  </div>
                ))}
              </div>
            )}
          </div>
          <div className="sm:col-span-2">
            <Field label="备注">
              <textarea
                className="input min-h-20 w-full resize-y"
                value={provider.notes}
                onChange={(event) =>
                  onChange({ ...provider, notes: event.target.value })
                }
              />
            </Field>
          </div>
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <button className="secondary-button" onClick={onClose}>
            取消
          </button>
          <button className="primary-button" onClick={onSubmit}>
            <Check className="h-4 w-4" />
            确定
          </button>
        </div>
      </div>
    </div>
  );
}

function Dashboard({
  status,
  baseUrl,
  config,
  enabledAliases,
  onStart,
  onStop,
  onCopy,
  onNavigate,
}: {
  status: ProxyStatus | null;
  baseUrl: string;
  config: GatewayConfig;
  enabledAliases: string[];
  onStart: () => void;
  onStop: () => void;
  onCopy: (text: string, label: string) => void;
  onNavigate: (tab: Tab) => void;
}) {
  const endpoint = `${baseUrl}/v1`;
  const sampleAlias = enabledAliases[0] || "local-model";
  const sample = `curl ${baseUrl}/v1/chat/completions \\\n  -H "Authorization: Bearer ${config.localApiKey || "LOCAL_API_KEY"}" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model":"${sampleAlias}","messages":[{"role":"user","content":"Hello"}]}'`;
  const hasModels = config.providers.some(
    (p) => p.enabled && p.models.some((m) => m.enabled),
  );
  const routingStatus = formatRoutingStatus(status);
  return (
    <section className="mx-auto max-w-5xl">
      <div className="mb-6 flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">
            本地统一 API 网关
          </h1>
          <p className="mt-1 text-sm text-muted-foreground">
            通过一个本地端口管理多个上游 API、模型别名和故障转移。
          </p>
        </div>
        {status?.running ? (
          <button className="danger-button" onClick={onStop}>
            <CircleStop className="h-4 w-4" />
            停止网关
          </button>
        ) : (
          <button className="primary-button" onClick={onStart}>
            <Play className="h-4 w-4" />
            启动网关
          </button>
        )}
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          icon={Activity}
          label="运行状态"
          value={status?.running ? "运行中" : "已停止"}
          detail={
            status?.running
              ? formatDuration(status.uptime_seconds)
              : "点击右上角启动"
          }
          active={status?.running}
        />
        <StatCard
          icon={Zap}
          label="请求总数"
          value={String(status?.total_requests ?? 0)}
          detail={`成功率 ${(status?.success_rate ?? 0).toFixed(1)}%`}
        />
        <StatCard
          icon={RefreshCw}
          label="上游路由"
          value={`${status?.failover_count ?? 0} 次回退`}
          detail={`上游失败尝试 ${status?.upstream_failed_attempts ?? 0} 次 · ${routingStatus.detail}`}
          detailTitle={routingStatus.title}
        />
        <StatCard
          icon={Database}
          label="模型别名"
          value={String(enabledAliases.length)}
          detail={`${config.providers.filter((provider) => provider.enabled).length} 个启用供应商`}
        />
      </div>

      <div className="mt-6 grid gap-4 lg:grid-cols-[1.15fr_0.85fr]">
        <div className="panel p-5">
          <div className="flex items-center justify-between">
            <div>
              <h2 className="font-semibold">接入信息</h2>
              <p className="mt-1 text-xs text-muted-foreground">
                适用于 OpenAI SDK、Anthropic SDK、酒馆和其他兼容客户端。
              </p>
            </div>
            <ShieldCheck className="h-5 w-5 text-emerald-500" />
          </div>
          <InfoLine
            label="OpenAI Base URL"
            value={`${endpoint}`}
            onCopy={() => onCopy(endpoint, "Base URL")}
          />
          <InfoLine
            label="Anthropic Base URL"
            value={baseUrl}
            onCopy={() => onCopy(baseUrl, "Base URL")}
          />
          <InfoLine
            label="本地 API Key"
            value={maskKey(config.localApiKey)}
            onCopy={() => onCopy(config.localApiKey, "API Key")}
          />
          <div className="mt-4 rounded-lg bg-muted p-3">
            <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11px] leading-5">
              {sample}
            </pre>
            <button
              className="secondary-button mt-3"
              onClick={() => onCopy(sample, "curl 示例")}
            >
              <Copy className="h-4 w-4" />
              复制示例
            </button>
          </div>
        </div>

        <div className="space-y-4">
          <div className="panel p-5">
            <h2 className="font-semibold">可用入口</h2>
            <div className="mt-3 space-y-2">
              <Endpoint
                method="POST"
                path="/v1/chat/completions"
                note="OpenAI Chat"
              />
              <Endpoint
                method="POST"
                path="/v1/responses"
                note="OpenAI Responses"
              />
              <Endpoint
                method="POST"
                path="/v1/messages"
                note="Anthropic Messages"
              />
              <Endpoint method="GET" path="/v1/models" note="本地模型列表" />
            </div>
          </div>
          {(config.providers.length === 0 || !hasModels) && (
            <div className="rounded-xl border border-amber-300 bg-amber-50 p-4 text-amber-900 dark:border-amber-900 dark:bg-amber-950/30 dark:text-amber-200">
              <h3 className="font-medium">还差一点配置</h3>
              <p className="mt-1 text-xs">
                至少需要一个供应商和一条启用的模型条目，API 才能转发请求。
              </p>
              <button
                className="mt-3 flex items-center gap-1 text-xs font-semibold"
                onClick={() =>
                  onNavigate(config.providers.length ? "routes" : "providers")
                }
              >
                继续配置
                <ChevronRight className="h-3.5 w-3.5" />
              </button>
            </div>
          )}
          {status?.last_error && (
            <div className="rounded-xl border border-destructive/30 bg-destructive/5 p-4">
              <h3 className="text-sm font-medium text-destructive">最近错误</h3>
              <p className="mt-1 break-words text-xs text-muted-foreground">
                {status.last_error}
              </p>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

function NavButton({
  icon: Icon,
  label,
  active,
  onClick,
  badge,
}: {
  icon: typeof Activity;
  label: string;
  active: boolean;
  onClick: () => void;
  badge?: number;
}) {
  return (
    <button
      className={cn(
        "flex w-full items-center gap-3 rounded-lg px-3 py-2.5 text-left text-sm transition",
        active
          ? "bg-primary text-primary-foreground shadow-sm"
          : "text-muted-foreground hover:bg-muted hover:text-foreground",
      )}
      onClick={onClick}
    >
      <Icon className="h-4 w-4" />
      <span className="flex-1">{label}</span>
      {badge !== undefined && (
        <span
          className={cn(
            "rounded-full px-1.5 text-[10px]",
            active ? "bg-white/20" : "bg-muted",
          )}
        >
          {badge}
        </span>
      )}
    </button>
  );
}

function PageHeader({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <div className="mb-6 flex items-start justify-between gap-4">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
        <p className="mt-1 text-sm text-muted-foreground">{description}</p>
      </div>
      {action}
    </div>
  );
}

function EmptyState({
  icon: Icon,
  title,
  description,
  action,
  onAction,
  disabled,
}: {
  icon: typeof Server;
  title: string;
  description: string;
  action: string;
  onAction: () => void;
  disabled?: boolean;
}) {
  return (
    <div className="panel flex min-h-80 flex-col items-center justify-center p-8 text-center">
      <div className="mb-4 flex h-14 w-14 items-center justify-center rounded-2xl bg-muted">
        <Icon className="h-7 w-7 text-muted-foreground" />
      </div>
      <h3 className="font-semibold">{title}</h3>
      <p className="mt-1 max-w-sm text-sm text-muted-foreground">
        {description}
      </p>
      <button
        className="primary-button mt-5"
        disabled={disabled}
        onClick={onAction}
      >
        <Plus className="h-4 w-4" />
        {action}
      </button>
    </div>
  );
}

function StatCard({
  icon: Icon,
  label,
  value,
  detail,
  detailTitle,
  active,
}: {
  icon: typeof Activity;
  label: string;
  value: string;
  detail: string;
  detailTitle?: string;
  active?: boolean;
}) {
  return (
    <div className="panel p-4">
      <div className="flex items-center justify-between">
        <span className="text-xs text-muted-foreground">{label}</span>
        <Icon
          className={cn(
            "h-4 w-4",
            active ? "text-emerald-500" : "text-muted-foreground",
          )}
        />
      </div>
      <div className="mt-3 text-2xl font-semibold">{value}</div>
      <div
        className="mt-1 truncate text-[11px] text-muted-foreground"
        title={detailTitle ?? detail}
      >
        {detail}
      </div>
    </div>
  );
}

function formatRoutingStatus(status: ProxyStatus | null): {
  detail: string;
  title: string;
} {
  const activeProviderMap = new Map<string, ActiveProvider>();
  for (const provider of status?.active_providers ?? []) {
    if (provider.request_count <= 0) continue;
    const key = `${provider.provider_id}\u0000${provider.provider_name}`;
    const existing = activeProviderMap.get(key);
    if (existing) {
      existing.request_count += provider.request_count;
    } else {
      activeProviderMap.set(key, { ...provider });
    }
  }
  const activeProviders = Array.from(activeProviderMap.values());
  const activeRequestCount = activeProviders.reduce(
    (total, provider) => total + provider.request_count,
    0,
  );

  if (activeProviders.length === 1) {
    const provider = activeProviders[0];
    const detail =
      activeRequestCount > 1
        ? `当前：${provider.provider_name} · ${activeRequestCount} 个请求`
        : `当前：${provider.provider_name}`;
    return { detail, title: detail };
  }

  if (activeProviders.length > 1) {
    return {
      detail: `当前：${activeProviders.length} 个上游 · ${activeRequestCount} 个请求`,
      title: activeProviders
        .map(
          (provider) =>
            `${provider.provider_name}：${provider.request_count} 个请求`,
        )
        .join("\n"),
    };
  }

  if (status?.current_provider) {
    const detail = `最近：${status.current_provider}`;
    return { detail, title: detail };
  }

  return { detail: "暂无请求", title: "暂无请求" };
}

function InfoLine({
  label,
  value,
  onCopy,
}: {
  label: string;
  value: string;
  onCopy: () => void;
}) {
  return (
    <div className="mt-4 flex items-center gap-3">
      <div className="w-32 shrink-0 text-xs text-muted-foreground">{label}</div>
      <code className="min-w-0 flex-1 truncate rounded bg-muted px-2.5 py-1.5 text-xs">
        {value}
      </code>
      <button className="icon-button" onClick={onCopy}>
        <Copy className="h-4 w-4" />
      </button>
    </div>
  );
}

function Endpoint({
  method,
  path,
  note,
}: {
  method: string;
  path: string;
  note: string;
}) {
  return (
    <div className="flex items-center gap-2 rounded-lg border px-3 py-2">
      <span className="w-10 text-[10px] font-bold text-primary">{method}</span>
      <code className="min-w-0 flex-1 truncate text-xs">{path}</code>
      <span className="text-[10px] text-muted-foreground">{note}</span>
    </div>
  );
}

function SettingRow({
  title,
  description,
  children,
  nested = false,
}: {
  title: string;
  description: string;
  children: ReactNode;
  nested?: boolean;
}) {
  return (
    <div
      className={cn(
        "flex items-center justify-between gap-8 p-5",
        nested && "bg-muted/20 pl-10",
      )}
    >
      <div className="max-w-lg">
        <h3 className="text-sm font-medium">{title}</h3>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          {description}
        </p>
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-xs font-medium text-muted-foreground">
        {label}
      </span>
      {children}
    </label>
  );
}

export default App;
