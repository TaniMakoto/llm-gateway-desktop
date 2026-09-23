import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  Clock3,
  RefreshCw,
  X,
  XCircle,
} from "lucide-react";
import { toast } from "sonner";

interface LogProvider {
  name: string;
  enabled: boolean;
  models: Array<{
    alias: string;
    apiFormat: string;
    enabled: boolean;
  }>;
}

interface RequestLog {
  requestId: string;
  providerId: string;
  providerName?: string;
  appType: string;
  model: string;
  requestModel?: string;
  reasoningEffort?: string;
  costMultiplier: string;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  inputCostUsd: string;
  outputCostUsd: string;
  cacheReadCostUsd: string;
  cacheCreationCostUsd: string;
  totalCostUsd: string;
  isStreaming: boolean;
  latencyMs: number;
  firstTokenMs?: number;
  durationMs?: number;
  statusCode: number;
  errorMessage?: string;
  createdAt: number;
  dataSource?: string;
  pricingModel?: string;
}

interface PaginatedLogs {
  data: RequestLog[];
  total: number;
  page: number;
  pageSize: number;
}

type TimeRange = "1h" | "24h" | "7d" | "30d" | "all";
type ResultFilter = "all" | "success" | "failed";

const TIME_RANGES: Array<{
  value: TimeRange;
  label: string;
  seconds?: number;
}> = [
  { value: "1h", label: "最近 1 小时", seconds: 60 * 60 },
  { value: "24h", label: "最近 24 小时", seconds: 24 * 60 * 60 },
  { value: "7d", label: "最近 7 天", seconds: 7 * 24 * 60 * 60 },
  { value: "30d", label: "最近 30 天", seconds: 30 * 24 * 60 * 60 },
  { value: "all", label: "全部时间" },
];

const SOURCE_LABELS: Record<string, string> = {
  codex: "Codex",
  claude: "Claude",
  "claude-desktop": "Claude Desktop",
  gemini: "Gemini",
  hermes: "Hermes",
  opencode: "OpenCode",
};

const numberFormatter = new Intl.NumberFormat("zh-CN");

function formatNumber(value: number): string {
  return numberFormatter.format(value);
}

function formatDuration(value?: number): string {
  if (value == null) return "—";
  return `${numberFormatter.format(value)} ms`;
}

function formatTime(timestamp: number): string {
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(new Date(timestamp * 1000));
}

function isSuccess(log: RequestLog): boolean {
  return log.statusCode >= 200 && log.statusCode < 300;
}

function cacheBase(log: RequestLog): number {
  if (log.appType === "codex" || log.appType === "gemini") {
    return log.inputTokens;
  }
  return log.inputTokens + log.cacheReadTokens + log.cacheCreationTokens;
}

function totalTokens(log: RequestLog): number {
  return cacheBase(log) + log.outputTokens;
}

function cacheRate(log: RequestLog): string {
  const base = cacheBase(log);
  if (base === 0) return "—";
  return `${((log.cacheReadTokens / base) * 100).toFixed(2)}%`;
}

function outputSpeed(log: RequestLog): string {
  if (!log.outputTokens) return "—";
  const total = log.durationMs ?? log.latencyMs;
  const generationMs = Math.max(0, total - (log.firstTokenMs ?? 0));
  if (!generationMs) return "—";
  return `${(log.outputTokens / (generationMs / 1000)).toFixed(1)} t/s`;
}

function protocolProviderOptions(providers: LogProvider[]): string[] {
  const result = new Set<string>();
  for (const provider of providers) {
    if (!provider.enabled) continue;
    for (const model of provider.models) {
      if (model.enabled) result.add(`${provider.name} · ${model.apiFormat}`);
    }
  }
  return Array.from(result).sort((a, b) => a.localeCompare(b, "zh-CN"));
}

export function RequestLogsPage({ providers }: { providers: LogProvider[] }) {
  const [timeRange, setTimeRange] = useState<TimeRange>("24h");
  const [model, setModel] = useState("");
  const [provider, setProvider] = useState("");
  const [source, setSource] = useState("");
  const [result, setResult] = useState<ResultFilter>("all");
  const [page, setPage] = useState(0);
  const [pageSize, setPageSize] = useState(50);
  const [logs, setLogs] = useState<PaginatedLogs>({
    data: [],
    total: 0,
    page: 0,
    pageSize: 50,
  });
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<RequestLog | null>(null);

  const configuredModels = useMemo(() => {
    const values = new Set<string>();
    for (const item of providers) {
      for (const route of item.models) {
        if (route.enabled && route.alias.trim()) values.add(route.alias.trim());
      }
    }
    for (const log of logs.data) values.add(log.requestModel || log.model);
    return Array.from(values).sort((a, b) => a.localeCompare(b));
  }, [logs.data, providers]);

  const providerOptions = useMemo(() => {
    const values = new Set(protocolProviderOptions(providers));
    for (const log of logs.data) {
      if (log.providerName) values.add(log.providerName);
    }
    return Array.from(values).sort((a, b) => a.localeCompare(b, "zh-CN"));
  }, [logs.data, providers]);

  const sourceOptions = useMemo(() => {
    const values = new Set(["codex", "claude", "gemini", "hermes", "opencode"]);
    for (const log of logs.data) values.add(log.appType);
    return Array.from(values);
  }, [logs.data]);

  const loadLogs = useCallback(
    async (quiet = false) => {
      if (!quiet) setLoading(true);
      try {
        const range = TIME_RANGES.find((item) => item.value === timeRange);
        const endDate = Math.floor(Date.now() / 1000);
        const response = await invoke<PaginatedLogs>("get_request_logs", {
          filters: {
            appType: source || null,
            providerName: provider || null,
            requestModel: model || null,
            success: result === "all" ? null : result === "success",
            startDate: range?.seconds ? endDate - range.seconds : null,
            endDate: range?.seconds ? endDate : null,
          },
          page,
          pageSize,
        });
        setLogs(response);
      } catch (error) {
        toast.error(`读取请求日志失败：${String(error)}`);
      } finally {
        if (!quiet) setLoading(false);
      }
    },
    [model, page, pageSize, provider, result, source, timeRange],
  );

  useEffect(() => {
    void loadLogs();
  }, [loadLogs]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("usage-log-recorded", () => {
      if (!disposed) void loadLogs(true);
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loadLogs]);

  const pageCount = Math.max(1, Math.ceil(logs.total / pageSize));
  const first = logs.total === 0 ? 0 : page * pageSize + 1;
  const last = Math.min((page + 1) * pageSize, logs.total);
  const resetPage = () => setPage(0);

  return (
    <section className="mx-auto max-w-[1600px]">
      <div className="mb-6 flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">请求日志</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            查看每次路由的模型、供应商、Token、缓存利用率、延迟和请求结果。
          </p>
        </div>
        <button
          className="secondary-button"
          disabled={loading}
          onClick={() => void loadLogs()}
        >
          <RefreshCw className={loading ? "h-4 w-4 animate-spin" : "h-4 w-4"} />
          刷新
        </button>
      </div>

      <div className="panel mb-4 grid gap-3 p-4 sm:grid-cols-2 xl:grid-cols-5">
        <Filter
          label="时间范围"
          value={timeRange}
          onChange={(value) => {
            setTimeRange(value as TimeRange);
            resetPage();
          }}
        >
          {TIME_RANGES.map((item) => (
            <option key={item.value} value={item.value}>
              {item.label}
            </option>
          ))}
        </Filter>
        <Filter
          label="模型"
          value={model}
          onChange={(value) => {
            setModel(value);
            resetPage();
          }}
        >
          <option value="">全部模型</option>
          {configuredModels.map((item) => (
            <option key={item} value={item}>
              {item}
            </option>
          ))}
        </Filter>
        <Filter
          label="提供商"
          value={provider}
          onChange={(value) => {
            setProvider(value);
            resetPage();
          }}
        >
          <option value="">全部提供商</option>
          {providerOptions.map((item) => (
            <option key={item} value={item}>
              {item}
            </option>
          ))}
        </Filter>
        <Filter
          label="来源"
          value={source}
          onChange={(value) => {
            setSource(value);
            resetPage();
          }}
        >
          <option value="">全部来源</option>
          {sourceOptions.map((item) => (
            <option key={item} value={item}>
              {SOURCE_LABELS[item] ?? item}
            </option>
          ))}
        </Filter>
        <Filter
          label="请求结果"
          value={result}
          onChange={(value) => {
            setResult(value as ResultFilter);
            resetPage();
          }}
        >
          <option value="all">全部结果</option>
          <option value="success">成功</option>
          <option value="failed">失败</option>
        </Filter>
      </div>

      <div className="panel overflow-hidden">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-3">
          <div className="flex items-center gap-3 text-xs text-muted-foreground">
            <span className="rounded-full border bg-muted/40 px-3 py-1 font-semibold text-foreground">
              共 {formatNumber(logs.total)} 条记录
            </span>
            <span>
              显示第 {formatNumber(first)} - {formatNumber(last)} 条
            </span>
          </div>
          <div className="flex items-center gap-2 text-xs">
            <select
              className="input h-9"
              value={pageSize}
              onChange={(event) => {
                setPageSize(Number(event.target.value));
                setPage(0);
              }}
            >
              {[25, 50, 100].map((size) => (
                <option key={size} value={size}>
                  {size} 条/页
                </option>
              ))}
            </select>
            <button
              className="icon-button border"
              disabled={page === 0}
              onClick={() => setPage((value) => value - 1)}
              aria-label="上一页"
            >
              <ChevronLeft className="h-4 w-4" />
            </button>
            <span className="min-w-12 text-center font-semibold">
              {page + 1} / {pageCount}
            </span>
            <button
              className="icon-button border"
              disabled={page + 1 >= pageCount}
              onClick={() => setPage((value) => value + 1)}
              aria-label="下一页"
            >
              <ChevronRight className="h-4 w-4" />
            </button>
          </div>
        </div>

        <div className="overflow-x-auto">
          <table className="w-full min-w-[1180px] border-collapse text-xs">
            <thead className="bg-muted/40 text-muted-foreground">
              <tr>
                {[
                  "时间",
                  "模型",
                  "提供商 / 来源",
                  "输入",
                  "输出",
                  "缓存",
                  "缓存率",
                  "总计",
                  "输出速度",
                  "首字延迟",
                  "总耗时",
                  "结果",
                ].map((label) => (
                  <th
                    key={label}
                    className="whitespace-nowrap border-b px-4 py-3 text-left font-semibold"
                  >
                    {label}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {logs.data.map((log) => (
                <tr
                  key={log.requestId}
                  className="cursor-pointer border-b transition last:border-b-0 hover:bg-muted/35"
                  onClick={() => setSelected(log)}
                >
                  <td className="whitespace-nowrap px-4 py-3 text-muted-foreground">
                    {formatTime(log.createdAt)}
                  </td>
                  <td className="px-4 py-3">
                    <div className="font-semibold">
                      {log.requestModel || log.model}
                    </div>
                    {log.requestModel && log.requestModel !== log.model && (
                      <div
                        className="mt-0.5 max-w-40 truncate text-[10px] text-muted-foreground"
                        title={log.model}
                      >
                        → {log.model}
                      </div>
                    )}
                    {log.reasoningEffort && (
                      <div
                        className="mt-0.5 text-[10px] font-medium text-muted-foreground"
                        title="最终发往成功上游候选的实际思考配置"
                      >
                        上游 · {log.reasoningEffort}
                      </div>
                    )}
                  </td>
                  <td className="px-4 py-3">
                    <div className="max-w-48 truncate" title={log.providerName}>
                      {log.providerName ?? log.providerId}
                    </div>
                    <div className="mt-0.5 text-[10px] text-muted-foreground">
                      {SOURCE_LABELS[log.appType] ?? log.appType}
                      {log.isStreaming ? " · 流式" : ""}
                    </div>
                  </td>
                  <td className="px-4 py-3 text-right tabular-nums">
                    {formatNumber(log.inputTokens)}
                  </td>
                  <td className="px-4 py-3 text-right tabular-nums">
                    {formatNumber(log.outputTokens)}
                  </td>
                  <td className="px-4 py-3 text-right tabular-nums">
                    {formatNumber(log.cacheReadTokens)}
                  </td>
                  <td className="px-4 py-3 text-right tabular-nums">
                    {cacheRate(log)}
                  </td>
                  <td className="px-4 py-3 text-right font-semibold tabular-nums">
                    {formatNumber(totalTokens(log))}
                  </td>
                  <td className="whitespace-nowrap px-4 py-3 text-right tabular-nums">
                    {outputSpeed(log)}
                  </td>
                  <td className="whitespace-nowrap px-4 py-3 text-right tabular-nums">
                    {formatDuration(log.firstTokenMs)}
                  </td>
                  <td className="whitespace-nowrap px-4 py-3 text-right tabular-nums">
                    {formatDuration(log.durationMs ?? log.latencyMs)}
                  </td>
                  <td className="px-4 py-3">
                    <StatusBadge log={log} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {!loading && logs.data.length === 0 && (
            <div className="flex min-h-64 flex-col items-center justify-center text-muted-foreground">
              <Clock3 className="mb-3 h-8 w-8 opacity-50" />
              <p className="font-medium text-foreground">没有匹配的请求日志</p>
              <p className="mt-1 text-xs">尝试调整时间范围或筛选条件。</p>
            </div>
          )}
          {loading && logs.data.length === 0 && (
            <div className="flex min-h-64 items-center justify-center gap-2 text-muted-foreground">
              <RefreshCw className="h-4 w-4 animate-spin" /> 正在读取日志…
            </div>
          )}
        </div>
      </div>

      {selected && (
        <LogDetail log={selected} onClose={() => setSelected(null)} />
      )}
    </section>
  );
}

function Filter({
  label,
  value,
  onChange,
  children,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-xs font-semibold text-muted-foreground">
        {label}
      </span>
      <select
        className="input w-full"
        value={value}
        onChange={(event) => onChange(event.target.value)}
      >
        {children}
      </select>
    </label>
  );
}

function StatusBadge({ log }: { log: RequestLog }) {
  const success = isSuccess(log);
  return (
    <div className="min-w-24">
      <span
        className={
          success
            ? "inline-flex items-center gap-1 rounded-full bg-emerald-500/10 px-2 py-1 font-semibold text-emerald-600 dark:text-emerald-400"
            : "inline-flex items-center gap-1 rounded-full bg-destructive/10 px-2 py-1 font-semibold text-destructive"
        }
      >
        {success ? (
          <CheckCircle2 className="h-3 w-3" />
        ) : (
          <XCircle className="h-3 w-3" />
        )}
        {success ? "成功" : "失败"}
      </span>
      <div className="mt-1 whitespace-nowrap text-[10px] text-muted-foreground">
        HTTP {log.statusCode}
      </div>
      {log.errorMessage && (
        <div
          className="mt-0.5 max-w-36 truncate text-[10px] text-destructive/80"
          title={log.errorMessage}
        >
          {log.errorMessage}
        </div>
      )}
    </div>
  );
}

function LogDetail({ log, onClose }: { log: RequestLog; onClose: () => void }) {
  const items: Array<[string, string]> = [
    ["请求 ID", log.requestId],
    ["请求时间", new Date(log.createdAt * 1000).toLocaleString("zh-CN")],
    ["请求模型", log.requestModel || log.model],
    ["实际模型", log.model],
    ["上游思考", log.reasoningEffort || "—"],
    ["计价模型", log.pricingModel || "—"],
    ["提供商", log.providerName ?? log.providerId],
    ["来源", SOURCE_LABELS[log.appType] ?? log.appType],
    ["响应模式", log.isStreaming ? "流式" : "非流式"],
    ["输入 Token", formatNumber(log.inputTokens)],
    ["输出 Token", formatNumber(log.outputTokens)],
    ["缓存读取", formatNumber(log.cacheReadTokens)],
    ["缓存写入", formatNumber(log.cacheCreationTokens)],
    ["总成本", `$${log.totalCostUsd}`],
    ["首字延迟", formatDuration(log.firstTokenMs)],
    ["总耗时", formatDuration(log.durationMs ?? log.latencyMs)],
    ["状态", `HTTP ${log.statusCode}`],
  ];
  return (
    <div
      className="fixed inset-0 z-50 flex justify-end bg-black/30"
      onMouseDown={onClose}
    >
      <aside
        className="h-full w-full max-w-lg overflow-y-auto border-l bg-background p-6 shadow-xl"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold">请求详情</h2>
            <p className="mt-1 text-xs text-muted-foreground">
              仅显示统计与路由元数据，不记录提示词正文。
            </p>
          </div>
          <button className="icon-button" onClick={onClose} aria-label="关闭">
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="mt-6 divide-y rounded-xl border">
          {items.map(([label, value]) => (
            <div
              key={label}
              className="grid grid-cols-[7rem_1fr] gap-3 px-4 py-3 text-xs"
            >
              <span className="text-muted-foreground">{label}</span>
              <span className="break-all text-right font-medium">{value}</span>
            </div>
          ))}
        </div>
        {log.errorMessage && (
          <div className="mt-4 rounded-xl border border-destructive/25 bg-destructive/5 p-4">
            <div className="text-xs font-semibold text-destructive">
              错误信息
            </div>
            <p className="mt-2 whitespace-pre-wrap break-words text-xs leading-5">
              {log.errorMessage}
            </p>
          </div>
        )}
      </aside>
    </div>
  );
}
