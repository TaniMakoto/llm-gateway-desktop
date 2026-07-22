import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
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
  Eye,
  EyeOff,
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
  Shield,
  ShieldCheck,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import { cn } from "@/lib/utils";

type ApiFormat = "openai_chat" | "openai_responses" | "anthropic";
type Tab = "dashboard" | "providers" | "routes" | "settings";
type ProxyMode = "follow_global" | "bypass" | "custom";

interface CachedModel {
  id: string;
  ownedBy?: string | null;
  displayName?: string | null;
}

interface ProviderModel {
  alias: string;
  upstreamModel: string;
  apiFormat: ApiFormat;
  enabled: boolean;
}

interface GatewayProvider {
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
  reasoningHistoryMode: "auto" | "reasoning_content" | "disabled";
  adaptiveThinkingDisplay: "auto" | "summarized" | "omitted";
  notes: string;
  models: ProviderModel[];
}

interface ModelFetchResult {
  models: CachedModel[];
  fetchedAt: string;
}

interface GatewayConfig {
  listenAddress: string;
  listenPort: number;
  requireAuth: boolean;
  localApiKey: string;
  autoStart: boolean;
  enableLogging: boolean;
  providers: GatewayProvider[];
}

interface ProxyStatus {
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
}

interface GatewaySnapshot {
  config: GatewayConfig;
  status: ProxyStatus;
}

interface ProxyTestResult {
  success: boolean;
  latencyMs: number;
  error?: string | null;
}

interface DetectedProxy {
  url: string;
  proxyType: string;
  port: number;
}

interface ModelTestResult {
  ok: boolean;
  status: number;
  latencyMs: number;
  replyText: string;
  rawBodyPreview: string;
  error?: string | null;
  pathUsed: string;
  proxyEffective?: string | null;
}

interface ModelTestContext {
  providerName: string;
  provider: GatewayProvider;
  modelIndex: number;
}

const DEFAULT_CONFIG: GatewayConfig = {
  listenAddress: "127.0.0.1",
  listenPort: 10888,
  requireAuth: true,
  localApiKey: "",
  autoStart: false,
  enableLogging: true,
  providers: [],
};

const formatLabels: Record<ApiFormat, string> = {
  openai_chat: "OpenAI Chat",
  openai_responses: "OpenAI Responses",
  anthropic: "Anthropic Messages",
};

function newId(prefix: string): string {
  const value = globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`;
  return `${prefix}-${value}`;
}

function formatDuration(seconds: number): string {
  if (!seconds) return "0 秒";
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;
  return [hours ? `${hours} 时` : "", minutes ? `${minutes} 分` : "", `${secs} 秒`]
    .filter(Boolean)
    .join(" ");
}

function maskKey(value: string): string {
  if (!value) return "未设置";
  if (value.length <= 12) return "••••••••";
  return `${value.slice(0, 8)}••••••••${value.slice(-4)}`;
}

function addressForUrl(address: string): string {
  return address.includes(":") && !address.startsWith("[") ? `[${address}]` : address;
}

function allEnabledAliases(config: GatewayConfig): string[] {
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

function App() {
  const [tab, setTab] = useState<Tab>("dashboard");
  const [config, setConfig] = useState<GatewayConfig>(DEFAULT_CONFIG);
  const [savedConfig, setSavedConfig] = useState<GatewayConfig>(DEFAULT_CONFIG);
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [showLocalKey, setShowLocalKey] = useState(false);
  const [providerEditor, setProviderEditor] = useState<GatewayProvider | null>(null);
  const [editingProviderId, setEditingProviderId] = useState<string | null>(null);
  const [showProviderKey, setShowProviderKey] = useState(false);
  const [headersText, setHeadersText] = useState("");
  const [fetchingModels, setFetchingModels] = useState(false);
  const [modelsFetchFormat, setModelsFetchFormat] = useState<ApiFormat>("openai_chat");
  const [expandedProviders, setExpandedProviders] = useState<Record<string, boolean>>({});
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
    } catch (error) {
      toast.error(`读取网关配置失败：${String(error)}`);
    } finally {
      if (showSpinner) setLoading(false);
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

  const refreshStatus = async () => {
    try {
      const snapshot = await invoke<GatewaySnapshot>("get_gateway_snapshot");
      setStatus(snapshot.status);
    } catch {
      // 静默
    }
  };

  useEffect(() => {
    void loadSnapshot(true);
    void loadGlobalProxy();
    const timer = window.setInterval(() => void refreshStatus(), 2000);
    return () => window.clearInterval(timer);
  }, []);

  const save = async (nextConfig = config, silent = false) => {
    setSaving(true);
    try {
      await invoke("save_gateway_config", { config: nextConfig });
      setConfig(nextConfig);
      setSavedConfig(nextConfig);
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
      reasoningHistoryMode: "auto",
      adaptiveThinkingDisplay: "auto",
      notes: "",
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
      reasoningHistoryMode: provider.reasoningHistoryMode ?? "auto",
      adaptiveThinkingDisplay: provider.adaptiveThinkingDisplay ?? "auto",
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
      const result = await invoke<ModelFetchResult>("fetch_gateway_provider_models", {
        request: {
          provider: { ...providerEditor, customHeaders },
          apiFormat: modelsFetchFormat,
        },
      });
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

  const patchProvider = (providerId: string, patch: Partial<GatewayProvider>) => {
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

  const addProviderModel = (providerId: string, initial?: Partial<ProviderModel>) => {
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
              models: provider.models.filter((_, index) => index !== modelIndex),
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
        <div data-tauri-drag-region className="flex items-center gap-2 font-semibold">
          <div className="flex h-7 w-7 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <Network className="h-4 w-4" />
          </div>
          <span>LLM Gateway Desktop</span>
          <span className="rounded bg-muted px-2 py-0.5 text-[10px] font-medium text-muted-foreground">
            0.1.0
          </span>
        </div>
        <div className="no-drag flex items-center">
          <button className="window-button" onClick={() => void appWindow.minimize()}>
            <Minus className="h-4 w-4" />
          </button>
          <button className="window-button" onClick={() => void appWindow.toggleMaximize()}>
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
            <NavButton icon={LayoutDashboard} label="运行状态" active={tab === "dashboard"} onClick={() => setTab("dashboard")} />
            <NavButton icon={Server} label="上游供应商" active={tab === "providers"} onClick={() => setTab("providers")} badge={config.providers.length} />
            <NavButton icon={Route} label="模型路由" active={tab === "routes"} onClick={() => setTab("routes")} badge={enabledAliases.length} />
            <NavButton icon={Settings} label="网关设置" active={tab === "settings"} onClick={() => setTab("settings")} />
          </nav>

          <div className="mt-auto rounded-xl border bg-background p-3">
            <div className="flex items-center gap-2">
              <span className={cn("h-2.5 w-2.5 rounded-full", status?.running ? "bg-emerald-500" : "bg-muted-foreground/40")} />
              <span className="text-xs font-medium">{status?.running ? "正在运行" : "已停止"}</span>
            </div>
            <div className="mt-2 truncate font-mono text-[10px] text-muted-foreground">{baseUrl}</div>
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
                action={<button className="primary-button" onClick={openNewProvider}><Plus className="h-4 w-4" />添加供应商</button>}
              />
              {config.providers.length === 0 ? (
                <EmptyState icon={Server} title="还没有供应商" description="先添加一个 OpenAI、Anthropic 或兼容中转 API。" action="添加第一个供应商" onAction={openNewProvider} />
              ) : (
                <div className="grid gap-4 lg:grid-cols-2">
                  {config.providers.map((provider) => (
                    <div key={provider.id} className="panel p-5">
                      <div className="flex items-start justify-between gap-3">
                        <div className="min-w-0">
                          <div className="flex items-center gap-2">
                            <span className={cn("h-2.5 w-2.5 rounded-full", provider.enabled ? "bg-emerald-500" : "bg-muted-foreground/40")} />
                            <h3 className="truncate font-semibold">{provider.name}</h3>
                          </div>
                          <p className="mt-1 truncate text-xs text-muted-foreground">{provider.baseUrl}</p>
                        </div>
                        <div className="flex gap-1">
                          <button className="icon-button" onClick={() => openProvider(provider)}><Settings className="h-4 w-4" /></button>
                          <button className="icon-button text-destructive" onClick={() => deleteProvider(provider.id)}><Trash2 className="h-4 w-4" /></button>
                        </div>
                      </div>
                      <div className="mt-4 flex flex-wrap gap-2 text-xs">
                        <span className="tag">{provider.authStyle === "auto" ? "自动鉴权" : provider.authStyle}</span>
                        <span className="tag font-mono">{maskKey(provider.apiKey)}</span>
                        <span className="tag">{provider.models.length} 个模型</span>
                        {(provider.cachedModels?.length ?? 0) > 0 && <span className="tag">缓存 {provider.cachedModels.length}</span>}
                      </div>
                      {provider.notes && <p className="mt-4 line-clamp-2 text-xs text-muted-foreground">{provider.notes}</p>}
                    </div>
                  ))}
                </div>
              )}
            </section>
          )}

          {tab === "routes" && (
            <section className="mx-auto max-w-5xl">
              <PageHeader
                title="模型路由"
                description="按上游供应商分组管理；每个模型独立选择协议、上游模型名、对外别名。"
                action={<button className="primary-button" disabled={!config.providers.length} onClick={() => config.providers[0] && addProviderModel(config.providers[0].id)}><Plus className="h-4 w-4" />快速添加</button>}
              />
              {config.providers.length === 0 ? (
                <EmptyState icon={Route} title="先添加供应商" description="模型路由从供应商派生，需要先在“上游供应商”里添加一个。" action="去添加供应商" onAction={() => setTab("providers")} />
              ) : (
                <div className="space-y-4">
                  {config.providers.map((provider, providerIndex) => (
                    <ProviderRoutesCard
                      key={provider.id}
                      provider={provider}
                      providerIndex={providerIndex}
                      canMoveUp={providerIndex > 0}
                      canMoveDown={providerIndex < config.providers.length - 1}
                      expanded={expandedProviders[provider.id] !== false}
                      onToggleExpand={() => setExpandedProviders((c) => ({ ...c, [provider.id]: c[provider.id] === false }))}
                      onPatch={(patch) => patchProvider(provider.id, patch)}
                      onMove={(direction) => moveProvider(providerIndex, direction)}
                      onEdit={() => openProvider(provider)}
                      onAddModel={(initial) => addProviderModel(provider.id, initial)}
                      onPatchModel={(index, patch) => patchProviderModel(provider.id, index, patch)}
                      onRemoveModel={(index) => removeProviderModel(provider.id, index)}
                      onTestModel={(modelIndex) => setTestCtx({ providerName: provider.name, provider, modelIndex })}
                    />
                  ))}
                </div>
              )}
            </section>
          )}

          {tab === "settings" && (
            <section className="mx-auto max-w-4xl">
              <PageHeader title="网关设置" description="默认只监听本机回环地址，避免 API 暴露到局域网。" />
              <div className="panel divide-y">
                <SettingRow title="监听地址" description="推荐保持 127.0.0.1。只有明确需要局域网访问时才改为 0.0.0.0。">
                  <input className="input w-52 font-mono" value={config.listenAddress} onChange={(event) => setConfig({ ...config, listenAddress: event.target.value })} />
                </SettingRow>
                <SettingRow title="监听端口" description="其他程序将通过这个端口调用统一 API。">
                  <input className="input w-32 font-mono" type="number" min={1} max={65535} value={config.listenPort} onChange={(event) => setConfig({ ...config, listenPort: Number(event.target.value) })} />
                </SettingRow>
                <SettingRow title="本地访问鉴权" description="OpenAI 客户端使用 Authorization: Bearer；Anthropic 客户端也可使用 x-api-key。">
                  <label className="switch-label"><input type="checkbox" checked={config.requireAuth} onChange={(event) => setConfig({ ...config, requireAuth: event.target.checked })} />启用</label>
                </SettingRow>
                <SettingRow title="本地 API Key" description="这个 Key 只用于访问本地网关，不会发送给上游。">
                  <div className="flex items-center gap-2">
                    <div className="relative">
                      <input className="input w-80 pr-9 font-mono" type={showLocalKey ? "text" : "password"} value={config.localApiKey} onChange={(event) => setConfig({ ...config, localApiKey: event.target.value })} />
                      <button className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground" onClick={() => setShowLocalKey(!showLocalKey)}>{showLocalKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}</button>
                    </div>
                    <button className="secondary-button" onClick={regenerateKey}><RotateCcw className="h-4 w-4" />重新生成</button>
                  </div>
                </SettingRow>
                <SettingRow title="随软件自动启动网关" description="启动桌面软件后自动监听本地端口，但不会修改或接管其他程序的配置。">
                  <label className="switch-label"><input type="checkbox" checked={config.autoStart} onChange={(event) => setConfig({ ...config, autoStart: event.target.checked })} />启用</label>
                </SettingRow>
                <SettingRow title="请求日志" description="记录模型、供应商、延迟、Token 和状态；默认不记录提示词正文。">
                  <label className="switch-label"><input type="checkbox" checked={config.enableLogging} onChange={(event) => setConfig({ ...config, enableLogging: event.target.checked })} />启用</label>
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
                    向上游转发时使用的代理。留空 = 直连。支持 http / https / socks5 / socks5h。
                  </p>
                  <div className="flex items-center gap-2">
                    <input
                      className="input flex-1 font-mono"
                      value={globalProxyUrl}
                      onChange={(event) => setGlobalProxyUrl(event.target.value)}
                      placeholder="http://127.0.0.1:7890 或 socks5://127.0.0.1:1080"
                    />
                    <button className="secondary-button" disabled={proxyTesting} onClick={() => void testGlobalProxy()}>
                      <Zap className={cn("h-4 w-4", proxyTesting && "animate-pulse")} />测试
                    </button>
                    <button className="primary-button" disabled={!proxyDirty || proxyApplying} onClick={() => void applyGlobalProxy()}>
                      {proxyApplying ? <RefreshCw className="h-4 w-4 animate-spin" /> : <Check className="h-4 w-4" />}应用
                    </button>
                  </div>
                  <div className="flex flex-wrap items-center gap-2">
                    <button className="secondary-button" disabled={scanningProxies} onClick={() => void scanProxies()}>
                      <RefreshCw className={cn("h-4 w-4", scanningProxies && "animate-spin")} />扫描本地代理
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
                      <button className="tag hover:bg-destructive hover:text-destructive-foreground" onClick={() => setGlobalProxyUrl("")}>
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
          {dirty && <button className="secondary-button" onClick={() => setConfig(savedConfig)}><RotateCcw className="h-4 w-4" />撤销</button>}
          <button className="primary-button" disabled={!dirty || saving} onClick={() => void save()}>
            {saving ? <RefreshCw className="h-4 w-4 animate-spin" /> : <Save className="h-4 w-4" />}
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
          onClose={() => setTestCtx(null)}
        />
      )}
    </div>
  );
}

function ProviderRoutesCard({
  provider,
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
  return (
    <div className="panel overflow-hidden">
      <div className="flex items-center gap-3 border-b p-4">
        <button className="icon-button" onClick={onToggleExpand} aria-label="展开/收起">
          {expanded ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
        </button>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className={cn("h-2.5 w-2.5 rounded-full", provider.enabled ? "bg-emerald-500" : "bg-muted-foreground/40")} />
            <span className="truncate font-semibold">{provider.name || "(未命名)"}</span>
            <span className="text-[11px] text-muted-foreground">#{providerIndex + 1}</span>
          </div>
          <div className="mt-0.5 truncate text-[11px] text-muted-foreground">{provider.baseUrl}</div>
        </div>
        <label className="switch-label"><input type="checkbox" checked={provider.enabled} onChange={(event) => onPatch({ enabled: event.target.checked })} />启用</label>
        <button className="icon-button" disabled={!canMoveUp} onClick={() => onMove(-1)}><ChevronUp className="h-4 w-4" /></button>
        <button className="icon-button" disabled={!canMoveDown} onClick={() => onMove(1)}><ChevronDown className="h-4 w-4" /></button>
        <button className="icon-button" onClick={onEdit} title="编辑供应商"><Settings className="h-4 w-4" /></button>
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
                onPatch={(patch) => onPatchModel(index, patch)}
                onRemove={() => onRemoveModel(index)}
                onTest={() => onTestModel(index)}
              />
            ))
          )}

          <div className="flex flex-wrap gap-2 pt-2">
            <button className="secondary-button" onClick={() => onAddModel()}>
              <Plus className="h-4 w-4" />添加模型
            </button>
            {(provider.cachedModels ?? []).slice(0, 6).map((cached) => (
              <button
                key={cached.id}
                className="tag hover:bg-primary hover:text-primary-foreground"
                onClick={() => onAddModel({ upstreamModel: cached.id, alias: cached.id })}
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
  onPatch,
  onRemove,
  onTest,
}: {
  model: ProviderModel;
  cachedModels: CachedModel[];
  onPatch: (patch: Partial<ProviderModel>) => void;
  onRemove: () => void;
  onTest: () => void;
}) {
  const listId = useMemo(() => `cached-models-${newId("dl")}`, []);
  return (
    <div className="grid grid-cols-[minmax(140px,1fr)_minmax(160px,1.2fr)_minmax(140px,0.9fr)_auto_auto] items-center gap-2">
      <input
        className="input font-mono"
        value={model.alias}
        onChange={(event) => onPatch({ alias: event.target.value })}
        placeholder="本地别名，如 my-best-code"
      />
      <div>
        <input
          className="input w-full font-mono"
          list={listId}
          value={model.upstreamModel}
          onChange={(event) => onPatch({ upstreamModel: event.target.value })}
          placeholder="上游真实模型名"
        />
        <datalist id={listId}>
          {cachedModels.map((cached) => (
            <option key={cached.id} value={cached.id}>{cached.displayName || cached.ownedBy || cached.id}</option>
          ))}
        </datalist>
      </div>
      <select className="input" value={model.apiFormat} onChange={(event) => onPatch({ apiFormat: event.target.value as ApiFormat })}>
        <option value="openai_chat">OpenAI Chat</option>
        <option value="openai_responses">OpenAI Responses</option>
        <option value="anthropic">Anthropic Messages</option>
      </select>
      <label className="switch-label"><input type="checkbox" checked={model.enabled} onChange={(event) => onPatch({ enabled: event.target.checked })} />启用</label>
      <div className="flex gap-1">
        <button className="icon-button" onClick={onTest} title="测试此模型"><Send className="h-4 w-4" /></button>
        <button className="icon-button text-destructive" onClick={onRemove} title="删除"><Trash2 className="h-4 w-4" /></button>
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
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-6" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="panel max-h-[90vh] w-full max-w-2xl overflow-y-auto p-6 shadow-2xl">
        <div className="mb-5 flex items-center justify-between">
          <div>
            <h2 className="text-lg font-semibold">{isEditing ? "编辑供应商" : "添加供应商"}</h2>
            <p className="mt-1 text-xs text-muted-foreground">协议不再绑定在供应商上，请在“模型路由”里为每个模型独立选择。</p>
          </div>
          <button className="icon-button" onClick={onClose}><X className="h-4 w-4" /></button>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="名称"><input className="input w-full" value={provider.name} onChange={(event) => onChange({ ...provider, name: event.target.value })} placeholder="例如 OpenRouter" /></Field>
          <Field label="状态"><label className="switch-label h-10"><input type="checkbox" checked={provider.enabled} onChange={(event) => onChange({ ...provider, enabled: event.target.checked })} />启用供应商</label></Field>
          <div className="sm:col-span-2"><Field label="Base URL"><input className="input w-full font-mono" value={provider.baseUrl} onChange={(event) => onChange({ ...provider, baseUrl: event.target.value })} placeholder="https://api.example.com/v1" /></Field></div>
          <div className="sm:col-span-2"><Field label="API Key"><div className="relative"><input className="input w-full pr-10 font-mono" type={showProviderKey ? "text" : "password"} value={provider.apiKey} onChange={(event) => onChange({ ...provider, apiKey: event.target.value })} placeholder="sk-..." /><button className="absolute right-3 top-1/2 -translate-y-1/2 text-muted-foreground" onClick={onToggleProviderKey}>{showProviderKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}</button></div></Field></div>
          <Field label="鉴权方式"><select className="input w-full" value={provider.authStyle} onChange={(event) => onChange({ ...provider, authStyle: event.target.value as GatewayProvider["authStyle"] })}><option value="auto">自动</option><option value="bearer">Authorization: Bearer</option><option value="x-api-key">x-api-key</option></select></Field>
          <Field label="模型列表 URL（可选）"><input className="input w-full font-mono" value={provider.modelsUrl} onChange={(event) => onChange({ ...provider, modelsUrl: event.target.value })} placeholder="留空时自动尝试 /v1/models" /></Field>
          <div className="sm:col-span-2"><Field label="自定义 User-Agent（可选）"><input className="input w-full font-mono" value={provider.customUserAgent} onChange={(event) => onChange({ ...provider, customUserAgent: event.target.value })} placeholder="例如 MyClient/1.0" /></Field><p className="mt-1 text-[11px] leading-5 text-muted-foreground">用于上游公开兼容要求；不会注入官方客户端私有令牌、身份提示词或设备指纹。</p></div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <label className="switch-label"><input type="checkbox" checked={provider.impersonateCodexClient} onChange={(event) => onChange({ ...provider, impersonateCodexClient: event.target.checked })} />以 Codex 客户端身份转发</label>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">部分上游会校验客户端指纹（返回 <code>unauthorized client detected</code>）。开启后转发时发送 <code>codex_cli_rs</code> 的 User-Agent 及成对的 <code>originator</code>/<code>version</code> 兼容标识。</p>
            {provider.impersonateCodexClient && (
              <div className="mt-2"><Field label="Codex 版本（可选）"><input className="input w-full font-mono" value={provider.codexClientVersion} onChange={(event) => onChange({ ...provider, codexClientVersion: event.target.value })} placeholder="留空使用默认 0.144.1" /></Field></div>
            )}
          </div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <div className="text-xs font-medium">推理兼容（高级）</div>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">默认保持自动判断。只有第三方接口不返回思考、错误发送推理参数导致 400，或原生 Claude adaptive thinking 不展示摘要时才需要调整。</p>
            <div className="mt-3 grid gap-3 sm:grid-cols-3">
              <Field label="推理请求映射">
                <select className="input w-full" value={provider.reasoningRequestMode} onChange={(event) => onChange({ ...provider, reasoningRequestMode: event.target.value as GatewayProvider["reasoningRequestMode"] })}>
                  <option value="auto">自动</option>
                  <option value="force">强制映射 effort</option>
                  <option value="disabled">不发送 effort</option>
                </select>
              </Field>
              <Field label="历史推理回传">
                <select className="input w-full" value={provider.reasoningHistoryMode} onChange={(event) => onChange({ ...provider, reasoningHistoryMode: event.target.value as GatewayProvider["reasoningHistoryMode"] })}>
                  <option value="auto">自动</option>
                  <option value="reasoning_content">reasoning_content</option>
                  <option value="disabled">关闭</option>
                </select>
              </Field>
              <Field label="Adaptive 展示">
                <select className="input w-full" value={provider.adaptiveThinkingDisplay} onChange={(event) => onChange({ ...provider, adaptiveThinkingDisplay: event.target.value as GatewayProvider["adaptiveThinkingDisplay"] })}>
                  <option value="auto">自动（不改写）</option>
                  <option value="summarized">显示摘要</option>
                  <option value="omitted">不显示摘要</option>
                </select>
              </Field>
            </div>
          </div>
          <div className="sm:col-span-2"><Field label="自定义请求头（每行一个 Header: Value）"><textarea className="input min-h-24 w-full resize-y font-mono text-xs" value={headersText} onChange={(event) => onHeadersText(event.target.value)} placeholder={"HTTP-Referer: https://example.com\nX-Title: My Gateway"} /></Field></div>
          <div className="sm:col-span-2 rounded-lg border bg-muted/30 p-3">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <div className="text-xs font-medium">上游模型列表（供“模型路由”自动补全）</div>
                <div className="mt-1 text-[11px] text-muted-foreground">{provider.cachedModels.length ? `已缓存 ${provider.cachedModels.length} 个模型` : "尚未获取"}{provider.modelsFetchedAt ? ` · ${new Date(provider.modelsFetchedAt).toLocaleString()}` : ""}</div>
              </div>
              <div className="flex items-center gap-2">
                <select className="input" value={modelsFetchFormat} onChange={(event) => onModelsFetchFormat(event.target.value as ApiFormat)}>
                  <option value="openai_chat">OpenAI Chat</option>
                  <option value="openai_responses">OpenAI Responses</option>
                  <option value="anthropic">Anthropic Messages</option>
                </select>
                <button className="secondary-button" disabled={fetchingModels} onClick={onFetchModels}>
                  <RefreshCw className={cn("h-4 w-4", fetchingModels && "animate-spin")} />
                  {fetchingModels ? "获取中" : "获取模型"}
                </button>
              </div>
            </div>
            {provider.cachedModels.length > 0 && <div className="mt-3 max-h-36 overflow-y-auto rounded border bg-background p-2 font-mono text-[11px] leading-5">{provider.cachedModels.map((model) => <div key={model.id} className="truncate" title={model.displayName || model.id}>{model.id}</div>)}</div>}
          </div>
          <div className="sm:col-span-2"><Field label="备注"><textarea className="input min-h-20 w-full resize-y" value={provider.notes} onChange={(event) => onChange({ ...provider, notes: event.target.value })} /></Field></div>
        </div>
        <div className="mt-6 flex justify-end gap-2"><button className="secondary-button" onClick={onClose}>取消</button><button className="primary-button" onClick={onSubmit}><Check className="h-4 w-4" />确定</button></div>
      </div>
    </div>
  );
}

function SegmentedOption({ name, checked, disabled = false, onChange, children }: {
  name: string;
  checked: boolean;
  disabled?: boolean;
  onChange: () => void;
  children: ReactNode;
}) {
  return (
    <label className={cn(
      "flex min-h-10 cursor-pointer select-none items-center justify-center gap-2 rounded-md px-4 py-2 text-sm font-medium transition-colors",
      checked ? "bg-background text-foreground shadow-sm" : "text-muted-foreground hover:bg-background/60 hover:text-foreground",
      disabled && "pointer-events-none cursor-not-allowed opacity-40",
    )}>
      <input className="sr-only" type="radio" name={name} checked={checked} disabled={disabled} onChange={onChange} />
      {checked && <Check className="h-3.5 w-3.5 shrink-0 text-emerald-500" />}
      <span>{children}</span>
    </label>
  );
}

function ModelTestModal({
  ctx,
  gatewayRunning,
  savedProviders,
  onClose,
}: {
  ctx: ModelTestContext;
  gatewayRunning: boolean;
  savedProviders: GatewayProvider[];
  onClose: () => void;
}) {
  const model = ctx.provider.models[ctx.modelIndex];
  const [prompt, setPrompt] = useState("用一句话介绍你自己。");
  const [viaGateway, setViaGateway] = useState<"direct" | "gateway">("direct");
  const [proxyMode, setProxyMode] = useState<ProxyMode>("bypass");
  const [customProxyUrl, setCustomProxyUrl] = useState("");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<ModelTestResult | null>(null);
  const [showRaw, setShowRaw] = useState(false);
  const initialLoad = useRef(false);

  useEffect(() => {
    if (!initialLoad.current) {
      initialLoad.current = true;
      invoke<string | null>("get_global_proxy_url")
        .then((url) => {
          if (url) setCustomProxyUrl(url);
        })
        .catch(() => {});
    }
  }, []);

  // 判断此模型行是否已在“已保存”配置里（用于判断能否通过网关测试）
  const savedProvider = savedProviders.find((p) => p.id === ctx.provider.id);
  const savedAlias = model.alias.trim();
  const aliasIsSaved = !!(
    savedProvider &&
    savedProvider.models.some(
      (m) =>
        m.alias.trim() === savedAlias &&
        m.upstreamModel.trim() === model.upstreamModel.trim() &&
        m.apiFormat === model.apiFormat &&
        m.enabled,
    ) &&
    savedProvider.enabled
  );
  const gatewayAllowed = gatewayRunning && aliasIsSaved && !!savedAlias;

  const run = async () => {
    if (!model.upstreamModel.trim()) {
      toast.error("请先填写上游模型名");
      return;
    }
    setRunning(true);
    setResult(null);
    try {
      const res = await invoke<ModelTestResult>("test_gateway_model", {
        request: {
          provider: ctx.provider,
          upstreamModel: model.upstreamModel,
          alias: model.alias,
          apiFormat: model.apiFormat,
          prompt,
          viaGateway: viaGateway === "gateway",
          proxyMode,
          customProxyUrl,
        },
      });
      setResult(res);
    } catch (error) {
      setResult({
        ok: false,
        status: 0,
        latencyMs: 0,
        replyText: "",
        rawBodyPreview: "",
        error: String(error),
        pathUsed: viaGateway,
        proxyEffective: null,
      });
    } finally {
      setRunning(false);
    }
  };

  const proxyDisabled = viaGateway === "gateway";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-6" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="panel max-h-[90vh] w-full max-w-2xl overflow-y-auto p-6 shadow-2xl">
        <div className="mb-4 flex items-start justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold">模型测试对话</h2>
            <p className="mt-1 text-xs text-muted-foreground">
              {ctx.providerName} · 上游模型 <code className="font-mono">{model.upstreamModel || "(空)"}</code> · 协议 {formatLabels[model.apiFormat]}
              {savedAlias ? <> · 别名 <code className="font-mono">{savedAlias}</code></> : null}
            </p>
          </div>
          <button className="icon-button" onClick={onClose}><X className="h-4 w-4" /></button>
        </div>

        <div className="space-y-4">
          <div className="rounded-xl border bg-muted/20 p-4">
            <div className="mb-3 text-sm font-medium">请求路径</div>
            <div className="grid grid-cols-2 gap-1 rounded-lg bg-muted p-1">
              <SegmentedOption name="viaGateway" checked={viaGateway === "direct"} onChange={() => setViaGateway("direct")}>直连上游</SegmentedOption>
              <SegmentedOption name="viaGateway" checked={viaGateway === "gateway"} disabled={!gatewayAllowed} onChange={() => setViaGateway("gateway")}>通过本地网关</SegmentedOption>
            </div>
            {!gatewayAllowed && (
              <div className="mt-2.5 text-[11px] leading-5 text-muted-foreground">
                {gatewayRunning ? "此模型条目尚未保存到运行配置中，无法通过网关测试。" : "网关未启动，无法通过网关测试。"}
              </div>
            )}
          </div>

          <div className={cn("rounded-xl border bg-muted/20 p-4", proxyDisabled && "opacity-60")}>
            <div className="mb-3 flex flex-wrap items-center gap-2">
              <Shield className="h-4 w-4 text-muted-foreground" />
              <span className="text-sm font-medium">出口方式</span>
              {proxyDisabled && <span className="text-[11px] text-muted-foreground">通过网关时由网关全局设置决定</span>}
            </div>
            <div className="grid grid-cols-3 gap-1 rounded-lg bg-muted p-1">
              <SegmentedOption name="proxyMode" checked={!proxyDisabled && proxyMode === "bypass"} disabled={proxyDisabled} onChange={() => setProxyMode("bypass")}>直接连接</SegmentedOption>
              <SegmentedOption name="proxyMode" checked={!proxyDisabled && proxyMode === "follow_global"} disabled={proxyDisabled} onChange={() => setProxyMode("follow_global")}>跟随全局</SegmentedOption>
              <SegmentedOption name="proxyMode" checked={!proxyDisabled && proxyMode === "custom"} disabled={proxyDisabled} onChange={() => setProxyMode("custom")}>临时代理</SegmentedOption>
            </div>
            {!proxyDisabled && proxyMode === "bypass" && (
              <p className="mt-2.5 text-[11px] leading-5 text-muted-foreground">本次测试强制直连上游，不使用全局设置或系统环境变量中的代理。</p>
            )}
            {proxyMode === "custom" && !proxyDisabled && (
              <input
                className="input mt-3 w-full font-mono"
                value={customProxyUrl}
                onChange={(event) => setCustomProxyUrl(event.target.value)}
                placeholder="http://127.0.0.1:7890 或 socks5://127.0.0.1:1080"
              />
            )}
          </div>

          <div>
            <div className="mb-1 text-xs font-medium text-muted-foreground">消息</div>
            <textarea
              className="input min-h-24 w-full resize-y"
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
            />
          </div>

          <div className="flex justify-end gap-2">
            <button className="secondary-button" onClick={onClose}>关闭</button>
            <button className="primary-button" disabled={running} onClick={() => void run()}>
              {running ? <RefreshCw className="h-4 w-4 animate-spin" /> : <Send className="h-4 w-4" />}
              发送
            </button>
          </div>

          {result && (
            <div className="rounded-lg border">
              <div className={cn(
                "flex flex-wrap items-center gap-2 border-b px-3 py-2 text-xs",
                result.ok ? "bg-emerald-50 dark:bg-emerald-950/30" : "bg-destructive/5",
              )}>
                <span className={cn("font-semibold", result.ok ? "text-emerald-700 dark:text-emerald-300" : "text-destructive")}>
                  {result.ok ? "成功" : "失败"}
                </span>
                {result.status > 0 && <span>· HTTP {result.status}</span>}
                <span>· {result.latencyMs}ms</span>
                <span>· {result.pathUsed === "gateway" ? "通过网关" : "直连"}</span>
                <span>· {result.proxyEffective ? `代理 ${result.proxyEffective}` : "无代理"}</span>
              </div>
              <div className="p-3">
                {result.error && (
                  <div className="mb-3 rounded border border-destructive/30 bg-destructive/5 p-2 text-xs text-destructive">
                    {result.error}
                  </div>
                )}
                {result.replyText && (
                  <div className="mb-2 text-xs font-medium text-muted-foreground">回复</div>
                )}
                {result.replyText && (
                  <div className="max-h-56 overflow-y-auto whitespace-pre-wrap rounded bg-muted p-3 text-xs leading-5">
                    {result.replyText}
                  </div>
                )}
                {result.rawBodyPreview && (
                  <div className="mt-3">
                    <button className="text-[11px] text-muted-foreground underline" onClick={() => setShowRaw(!showRaw)}>
                      {showRaw ? "隐藏原始响应" : "显示原始响应"}
                    </button>
                    {showRaw && (
                      <pre className="mt-2 max-h-56 overflow-auto rounded bg-muted p-2 font-mono text-[10px] leading-4">{result.rawBodyPreview}</pre>
                    )}
                  </div>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function Dashboard({ status, baseUrl, config, enabledAliases, onStart, onStop, onCopy, onNavigate }: {
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
  const hasModels = config.providers.some((p) => p.enabled && p.models.some((m) => m.enabled));
  return (
    <section className="mx-auto max-w-5xl">
      <div className="mb-6 flex items-start justify-between gap-4">
        <div><h1 className="text-2xl font-semibold tracking-tight">本地统一 API 网关</h1><p className="mt-1 text-sm text-muted-foreground">通过一个本地端口管理多个上游 API、模型别名和故障转移。</p></div>
        {status?.running ? <button className="danger-button" onClick={onStop}><CircleStop className="h-4 w-4" />停止网关</button> : <button className="primary-button" onClick={onStart}><Play className="h-4 w-4" />启动网关</button>}
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard icon={Activity} label="运行状态" value={status?.running ? "运行中" : "已停止"} detail={status?.running ? formatDuration(status.uptime_seconds) : "点击右上角启动"} active={status?.running} />
        <StatCard icon={Zap} label="请求总数" value={String(status?.total_requests ?? 0)} detail={`成功率 ${(status?.success_rate ?? 0).toFixed(1)}%`} />
        <StatCard icon={RefreshCw} label="备用切换" value={`${status?.failover_count ?? 0} 次`} detail={status?.current_provider ? `当前：${status.current_provider}` : "尚未发生请求"} />
        <StatCard icon={Database} label="模型别名" value={String(enabledAliases.length)} detail={`${config.providers.filter((provider) => provider.enabled).length} 个启用供应商`} />
      </div>

      <div className="mt-6 grid gap-4 lg:grid-cols-[1.15fr_0.85fr]">
        <div className="panel p-5">
          <div className="flex items-center justify-between"><div><h2 className="font-semibold">接入信息</h2><p className="mt-1 text-xs text-muted-foreground">适用于 OpenAI SDK、Anthropic SDK、酒馆和其他兼容客户端。</p></div><ShieldCheck className="h-5 w-5 text-emerald-500" /></div>
          <InfoLine label="OpenAI Base URL" value={`${endpoint}`} onCopy={() => onCopy(endpoint, "Base URL")} />
          <InfoLine label="Anthropic Base URL" value={baseUrl} onCopy={() => onCopy(baseUrl, "Base URL")} />
          <InfoLine label="本地 API Key" value={maskKey(config.localApiKey)} onCopy={() => onCopy(config.localApiKey, "API Key")} />
          <div className="mt-4 rounded-lg bg-muted p-3"><pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11px] leading-5">{sample}</pre><button className="secondary-button mt-3" onClick={() => onCopy(sample, "curl 示例")}><Copy className="h-4 w-4" />复制示例</button></div>
        </div>

        <div className="space-y-4">
          <div className="panel p-5"><h2 className="font-semibold">可用入口</h2><div className="mt-3 space-y-2"><Endpoint method="POST" path="/v1/chat/completions" note="OpenAI Chat" /><Endpoint method="POST" path="/v1/responses" note="OpenAI Responses" /><Endpoint method="POST" path="/v1/messages" note="Anthropic Messages" /><Endpoint method="GET" path="/v1/models" note="本地模型列表" /></div></div>
          {(config.providers.length === 0 || !hasModels) && <div className="rounded-xl border border-amber-300 bg-amber-50 p-4 text-amber-900 dark:border-amber-900 dark:bg-amber-950/30 dark:text-amber-200"><h3 className="font-medium">还差一点配置</h3><p className="mt-1 text-xs">至少需要一个供应商和一条启用的模型条目，API 才能转发请求。</p><button className="mt-3 flex items-center gap-1 text-xs font-semibold" onClick={() => onNavigate(config.providers.length ? "routes" : "providers")}>继续配置<ChevronRight className="h-3.5 w-3.5" /></button></div>}
          {status?.last_error && <div className="rounded-xl border border-destructive/30 bg-destructive/5 p-4"><h3 className="text-sm font-medium text-destructive">最近错误</h3><p className="mt-1 break-words text-xs text-muted-foreground">{status.last_error}</p></div>}
        </div>
      </div>
    </section>
  );
}

function NavButton({ icon: Icon, label, active, onClick, badge }: { icon: typeof Activity; label: string; active: boolean; onClick: () => void; badge?: number }) {
  return <button className={cn("flex w-full items-center gap-3 rounded-lg px-3 py-2.5 text-left text-sm transition", active ? "bg-primary text-primary-foreground shadow-sm" : "text-muted-foreground hover:bg-muted hover:text-foreground")} onClick={onClick}><Icon className="h-4 w-4" /><span className="flex-1">{label}</span>{badge !== undefined && <span className={cn("rounded-full px-1.5 text-[10px]", active ? "bg-white/20" : "bg-muted")}>{badge}</span>}</button>;
}

function PageHeader({ title, description, action }: { title: string; description: string; action?: ReactNode }) {
  return <div className="mb-6 flex items-start justify-between gap-4"><div><h1 className="text-2xl font-semibold tracking-tight">{title}</h1><p className="mt-1 text-sm text-muted-foreground">{description}</p></div>{action}</div>;
}

function EmptyState({ icon: Icon, title, description, action, onAction, disabled }: { icon: typeof Server; title: string; description: string; action: string; onAction: () => void; disabled?: boolean }) {
  return <div className="panel flex min-h-80 flex-col items-center justify-center p-8 text-center"><div className="mb-4 flex h-14 w-14 items-center justify-center rounded-2xl bg-muted"><Icon className="h-7 w-7 text-muted-foreground" /></div><h3 className="font-semibold">{title}</h3><p className="mt-1 max-w-sm text-sm text-muted-foreground">{description}</p><button className="primary-button mt-5" disabled={disabled} onClick={onAction}><Plus className="h-4 w-4" />{action}</button></div>;
}

function StatCard({ icon: Icon, label, value, detail, active }: { icon: typeof Activity; label: string; value: string; detail: string; active?: boolean }) {
  return <div className="panel p-4"><div className="flex items-center justify-between"><span className="text-xs text-muted-foreground">{label}</span><Icon className={cn("h-4 w-4", active ? "text-emerald-500" : "text-muted-foreground")} /></div><div className="mt-3 text-2xl font-semibold">{value}</div><div className="mt-1 truncate text-[11px] text-muted-foreground">{detail}</div></div>;
}

function InfoLine({ label, value, onCopy }: { label: string; value: string; onCopy: () => void }) {
  return <div className="mt-4 flex items-center gap-3"><div className="w-32 shrink-0 text-xs text-muted-foreground">{label}</div><code className="min-w-0 flex-1 truncate rounded bg-muted px-2.5 py-1.5 text-xs">{value}</code><button className="icon-button" onClick={onCopy}><Copy className="h-4 w-4" /></button></div>;
}

function Endpoint({ method, path, note }: { method: string; path: string; note: string }) {
  return <div className="flex items-center gap-2 rounded-lg border px-3 py-2"><span className="w-10 text-[10px] font-bold text-primary">{method}</span><code className="min-w-0 flex-1 truncate text-xs">{path}</code><span className="text-[10px] text-muted-foreground">{note}</span></div>;
}

function SettingRow({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return <div className="flex items-center justify-between gap-8 p-5"><div className="max-w-lg"><h3 className="text-sm font-medium">{title}</h3><p className="mt-1 text-xs leading-5 text-muted-foreground">{description}</p></div><div className="shrink-0">{children}</div></div>;
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return <label className="block"><span className="mb-1.5 block text-xs font-medium text-muted-foreground">{label}</span>{children}</label>;
}

export default App;
