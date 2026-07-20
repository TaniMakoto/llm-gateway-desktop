import { useEffect, useMemo, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { toast } from "sonner";
import {
  Activity,
  ArrowDown,
  ArrowUp,
  Check,
  ChevronRight,
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
  RefreshCw,
  RotateCcw,
  Route,
  Save,
  Server,
  Settings,
  ShieldCheck,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import { cn } from "@/lib/utils";

type ApiFormat = "openai_chat" | "openai_responses" | "anthropic";
type Tab = "dashboard" | "providers" | "routes" | "settings";

interface GatewayProvider {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
  apiFormat: ApiFormat;
  enabled: boolean;
  authStyle: "auto" | "bearer" | "x-api-key";
  customHeaders: Record<string, string>;
  notes: string;
 }

interface RouteTarget {
  providerId: string;
  upstreamModel: string;
  enabled: boolean;
 }

interface GatewayRoute {
  alias: string;
  enabled: boolean;
  targets: RouteTarget[];
 }

interface GatewayConfig {
  listenAddress: string;
  listenPort: number;
  requireAuth: boolean;
  localApiKey: string;
  autoStart: boolean;
  enableLogging: boolean;
  providers: GatewayProvider[];
  routes: GatewayRoute[];
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

const DEFAULT_CONFIG: GatewayConfig = {
  listenAddress: "127.0.0.1",
  listenPort: 10888,
  requireAuth: true,
  localApiKey: "",
  autoStart: false,
  enableLogging: true,
  providers: [],
  routes: [],
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

  const dirty = useMemo(
    () => JSON.stringify(config) !== JSON.stringify(savedConfig),
    [config, savedConfig],
  );
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

  const refreshStatus = async () => {
    try {
      const snapshot = await invoke<GatewaySnapshot>("get_gateway_snapshot");
      setStatus(snapshot.status);
    } catch {
      // Polling errors are intentionally quiet.
    }
  };

  useEffect(() => {
    void loadSnapshot(true);
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
      apiFormat: "openai_chat",
      enabled: true,
      authStyle: "auto",
      customHeaders: {},
      notes: "",
    };
    setEditingProviderId(null);
    setProviderEditor(provider);
    setHeadersText("");
    setShowProviderKey(false);
  };

  const openProvider = (provider: GatewayProvider) => {
    setEditingProviderId(provider.id);
    setProviderEditor(structuredClone(provider));
    setHeadersText(
      Object.entries(provider.customHeaders)
        .map(([key, value]) => `${key}: ${value}`)
        .join("\n"),
    );
    setShowProviderKey(false);
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
      routes: current.routes.map((route) => ({
        ...route,
        targets: route.targets.filter((target) => target.providerId !== id),
      })),
    }));
  };

  const addRoute = () => {
    const first = config.providers.find((provider) => provider.enabled);
    setConfig((current) => ({
      ...current,
      routes: [
        ...current.routes,
        {
          alias: `local-model-${current.routes.length + 1}`,
          enabled: true,
          targets: first
            ? [{ providerId: first.id, upstreamModel: "", enabled: true }]
            : [],
        },
      ],
    }));
  };

  const patchRoute = (index: number, patch: Partial<GatewayRoute>) => {
    setConfig((current) => ({
      ...current,
      routes: current.routes.map((route, routeIndex) =>
        routeIndex === index ? { ...route, ...patch } : route,
      ),
    }));
  };

  const patchTarget = (
    routeIndex: number,
    targetIndex: number,
    patch: Partial<RouteTarget>,
  ) => {
    setConfig((current) => ({
      ...current,
      routes: current.routes.map((route, currentRouteIndex) =>
        currentRouteIndex === routeIndex
          ? {
              ...route,
              targets: route.targets.map((target, currentTargetIndex) =>
                currentTargetIndex === targetIndex ? { ...target, ...patch } : target,
              ),
            }
          : route,
      ),
    }));
  };

  const moveTarget = (routeIndex: number, targetIndex: number, direction: -1 | 1) => {
    setConfig((current) => {
      const routes = structuredClone(current.routes);
      const targets = routes[routeIndex].targets;
      const nextIndex = targetIndex + direction;
      if (nextIndex < 0 || nextIndex >= targets.length) return current;
      [targets[targetIndex], targets[nextIndex]] = [targets[nextIndex], targets[targetIndex]];
      return { ...current, routes };
    });
  };

  const removeTarget = (routeIndex: number, targetIndex: number) => {
    setConfig((current) => ({
      ...current,
      routes: current.routes.map((route, currentRouteIndex) =>
        currentRouteIndex === routeIndex
          ? {
              ...route,
              targets: route.targets.filter((_, index) => index !== targetIndex),
            }
          : route,
      ),
    }));
  };

  const appWindow = getCurrentWindow();

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
            <NavButton icon={Route} label="模型路由" active={tab === "routes"} onClick={() => setTab("routes")} badge={config.routes.length} />
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
                description="集中保存第三方 API。保存后会生成网关内部路由配置，不改写其他 CLI。"
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
                        <span className="tag">{formatLabels[provider.apiFormat]}</span>
                        <span className="tag">{provider.authStyle === "auto" ? "自动鉴权" : provider.authStyle}</span>
                        <span className="tag font-mono">{maskKey(provider.apiKey)}</span>
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
                description="客户端只使用本地模型别名；目标从上到下依次尝试。"
                action={<button className="primary-button" onClick={addRoute} disabled={!config.providers.length}><Plus className="h-4 w-4" />添加模型路由</button>}
              />
              {config.routes.length === 0 ? (
                <EmptyState icon={Route} title="还没有模型路由" description="添加供应商后，为客户端创建一个稳定的本地模型名。" action="添加模型路由" onAction={addRoute} disabled={!config.providers.length} />
              ) : (
                <div className="space-y-4">
                  {config.routes.map((route, routeIndex) => (
                    <div className="panel overflow-hidden" key={`${route.alias}-${routeIndex}`}>
                      <div className="flex items-center gap-3 border-b p-4">
                        <input className="input flex-1 font-mono font-semibold" value={route.alias} onChange={(event) => patchRoute(routeIndex, { alias: event.target.value })} placeholder="例如 best-code" />
                        <label className="switch-label"><input type="checkbox" checked={route.enabled} onChange={(event) => patchRoute(routeIndex, { enabled: event.target.checked })} />启用</label>
                        <button className="icon-button text-destructive" onClick={() => setConfig((current) => ({ ...current, routes: current.routes.filter((_, index) => index !== routeIndex) }))}><Trash2 className="h-4 w-4" /></button>
                      </div>
                      <div className="space-y-2 p-4">
                        {route.targets.map((target, targetIndex) => {
                          return (
                            <div className="grid grid-cols-[32px_minmax(160px,0.8fr)_minmax(180px,1fr)_auto] items-center gap-2" key={`${target.providerId}-${targetIndex}`}>
                              <div className="flex h-8 w-8 items-center justify-center rounded-full bg-muted text-xs font-semibold">{targetIndex + 1}</div>
                              <select className="input" value={target.providerId} onChange={(event) => patchTarget(routeIndex, targetIndex, { providerId: event.target.value })}>
                                {config.providers.map((item) => <option key={item.id} value={item.id}>{item.name} · {formatLabels[item.apiFormat]}</option>)}
                              </select>
                              <input className="input font-mono" value={target.upstreamModel} onChange={(event) => patchTarget(routeIndex, targetIndex, { upstreamModel: event.target.value })} placeholder="上游真实模型名" />
                              <div className="flex gap-1">
                                <button className="icon-button" disabled={targetIndex === 0} onClick={() => moveTarget(routeIndex, targetIndex, -1)}><ArrowUp className="h-4 w-4" /></button>
                                <button className="icon-button" disabled={targetIndex === route.targets.length - 1} onClick={() => moveTarget(routeIndex, targetIndex, 1)}><ArrowDown className="h-4 w-4" /></button>
                                <button className="icon-button text-destructive" onClick={() => removeTarget(routeIndex, targetIndex)}><X className="h-4 w-4" /></button>
                              </div>
                            </div>
                          );
                        })}
                        <button
                          className="secondary-button mt-2"
                          disabled={!config.providers.length}
                          onClick={() => patchRoute(routeIndex, {
                            targets: [...route.targets, { providerId: config.providers[0].id, upstreamModel: "", enabled: true }],
                          })}
                        ><Plus className="h-4 w-4" />添加故障转移目标</button>
                      </div>
                    </div>
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
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-6" onMouseDown={(event) => { if (event.target === event.currentTarget) setProviderEditor(null); }}>
          <div className="panel max-h-[90vh] w-full max-w-2xl overflow-y-auto p-6 shadow-2xl">
            <div className="mb-5 flex items-center justify-between">
              <div><h2 className="text-lg font-semibold">{editingProviderId ? "编辑供应商" : "添加供应商"}</h2><p className="mt-1 text-xs text-muted-foreground">真实 API Key 只保存在本机 SQLite 数据库。</p></div>
              <button className="icon-button" onClick={() => setProviderEditor(null)}><X className="h-4 w-4" /></button>
            </div>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="名称"><input className="input w-full" value={providerEditor.name} onChange={(event) => setProviderEditor({ ...providerEditor, name: event.target.value })} placeholder="例如 OpenRouter" /></Field>
              <Field label="上游格式"><select className="input w-full" value={providerEditor.apiFormat} onChange={(event) => setProviderEditor({ ...providerEditor, apiFormat: event.target.value as ApiFormat })}><option value="openai_chat">OpenAI Chat Completions</option><option value="openai_responses">OpenAI Responses</option><option value="anthropic">Anthropic Messages</option></select></Field>
              <div className="sm:col-span-2"><Field label="Base URL"><input className="input w-full font-mono" value={providerEditor.baseUrl} onChange={(event) => setProviderEditor({ ...providerEditor, baseUrl: event.target.value })} placeholder="https://api.example.com/v1" /></Field></div>
              <div className="sm:col-span-2"><Field label="API Key"><div className="relative"><input className="input w-full pr-10 font-mono" type={showProviderKey ? "text" : "password"} value={providerEditor.apiKey} onChange={(event) => setProviderEditor({ ...providerEditor, apiKey: event.target.value })} placeholder="sk-..." /><button className="absolute right-3 top-1/2 -translate-y-1/2 text-muted-foreground" onClick={() => setShowProviderKey(!showProviderKey)}>{showProviderKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}</button></div></Field></div>
              <Field label="鉴权方式"><select className="input w-full" value={providerEditor.authStyle} onChange={(event) => setProviderEditor({ ...providerEditor, authStyle: event.target.value as GatewayProvider["authStyle"] })}><option value="auto">自动</option><option value="bearer">Authorization: Bearer</option><option value="x-api-key">x-api-key</option></select></Field>
              <Field label="状态"><label className="switch-label h-10"><input type="checkbox" checked={providerEditor.enabled} onChange={(event) => setProviderEditor({ ...providerEditor, enabled: event.target.checked })} />启用供应商</label></Field>
              <div className="sm:col-span-2"><Field label="自定义请求头（每行一个 Header: Value）"><textarea className="input min-h-24 w-full resize-y font-mono text-xs" value={headersText} onChange={(event) => setHeadersText(event.target.value)} placeholder={"HTTP-Referer: https://example.com\nX-Title: My Gateway"} /></Field></div>
              <div className="sm:col-span-2"><Field label="备注"><textarea className="input min-h-20 w-full resize-y" value={providerEditor.notes} onChange={(event) => setProviderEditor({ ...providerEditor, notes: event.target.value })} /></Field></div>
            </div>
            <div className="mt-6 flex justify-end gap-2"><button className="secondary-button" onClick={() => setProviderEditor(null)}>取消</button><button className="primary-button" onClick={commitProvider}><Check className="h-4 w-4" />确定</button></div>
          </div>
        </div>
      )}
    </div>
  );
 }

function Dashboard({ status, baseUrl, config, onStart, onStop, onCopy, onNavigate }: {
  status: ProxyStatus | null;
  baseUrl: string;
  config: GatewayConfig;
  onStart: () => void;
  onStop: () => void;
  onCopy: (text: string, label: string) => void;
  onNavigate: (tab: Tab) => void;
 }) {
  const endpoint = `${baseUrl}/v1`;
  const sample = `curl ${baseUrl}/v1/chat/completions \\\n  -H "Authorization: Bearer ${config.localApiKey || "LOCAL_API_KEY"}" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model":"${config.routes[0]?.alias || "local-model"}","messages":[{"role":"user","content":"Hello"}]}'`;
  return (
    <section className="mx-auto max-w-5xl">
      <div className="mb-6 flex items-start justify-between gap-4">
        <div><h1 className="text-2xl font-semibold tracking-tight">本地统一 API 网关</h1><p className="mt-1 text-sm text-muted-foreground">通过一个本地端口管理多个上游 API、模型别名和故障转移。</p></div>
        {status?.running ? <button className="danger-button" onClick={onStop}><CircleStop className="h-4 w-4" />停止网关</button> : <button className="primary-button" onClick={onStart}><Play className="h-4 w-4" />启动网关</button>}
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard icon={Activity} label="运行状态" value={status?.running ? "运行中" : "已停止"} detail={status?.running ? formatDuration(status.uptime_seconds) : "点击右上角启动"} active={status?.running} />
        <StatCard icon={Zap} label="请求总数" value={String(status?.total_requests ?? 0)} detail={`成功率 ${(status?.success_rate ?? 0).toFixed(1)}%`} />
        <StatCard icon={RefreshCw} label="故障转移" value={String(status?.failover_count ?? 0)} detail={status?.current_provider || "暂无活跃供应商"} />
        <StatCard icon={Database} label="模型路由" value={String(config.routes.filter((route) => route.enabled).length)} detail={`${config.providers.filter((provider) => provider.enabled).length} 个启用供应商`} />
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
          {(config.providers.length === 0 || config.routes.length === 0) && <div className="rounded-xl border border-amber-300 bg-amber-50 p-4 text-amber-900 dark:border-amber-900 dark:bg-amber-950/30 dark:text-amber-200"><h3 className="font-medium">还差一点配置</h3><p className="mt-1 text-xs">至少需要一个供应商和一个模型路由，API 才能转发请求。</p><button className="mt-3 flex items-center gap-1 text-xs font-semibold" onClick={() => onNavigate(config.providers.length ? "routes" : "providers")}>继续配置<ChevronRight className="h-3.5 w-3.5" /></button></div>}
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
