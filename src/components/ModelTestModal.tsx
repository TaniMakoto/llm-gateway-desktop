import { useEffect, useRef, useState, type ReactNode } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Copy,
  Eraser,
  Pencil,
  RefreshCw,
  Send,
  Settings,
  Shield,
  Trash2,
  X,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { Markdown } from "@/components/markdown";
import type {
  ProxyMode,
  GatewayProvider,
  ModelTestResult,
  ModelTestRole,
  TestThinkingLevel,
  ModelTestStreamEvent,
  ModelTestMessage,
  ModelTestContext,
} from "@/gateway/types";
import { formatLabels, MODEL_TEST_MAX_OUTPUT_TOKENS } from "@/gateway/helpers";

function SegmentedOption({
  name,
  checked,
  disabled = false,
  onChange,
  children,
}: {
  name: string;
  checked: boolean;
  disabled?: boolean;
  onChange: () => void;
  children: ReactNode;
}) {
  return (
    <label
      className={cn(
        "flex min-h-10 cursor-pointer select-none items-center justify-center gap-2 rounded-md px-4 py-2 text-sm font-medium transition-colors",
        checked
          ? "bg-background text-foreground shadow-sm"
          : "text-muted-foreground hover:bg-background/60 hover:text-foreground",
        disabled && "pointer-events-none cursor-not-allowed opacity-40",
      )}
    >
      <input
        className="sr-only"
        type="radio"
        name={name}
        checked={checked}
        disabled={disabled}
        onChange={onChange}
      />
      {checked && <Check className="h-3.5 w-3.5 shrink-0 text-emerald-500" />}
      <span>{children}</span>
    </label>
  );
}

export function ModelTestModal({
  ctx,
  gatewayRunning,
  savedProviders,
  onCopy,
  onClose,
}: {
  ctx: ModelTestContext;
  gatewayRunning: boolean;
  savedProviders: GatewayProvider[];
  onCopy: (text: string, label: string) => void;
  onClose: () => void;
}) {
  const model = ctx.provider.models[ctx.modelIndex];
  const [history, setHistory] = useState<ModelTestMessage[]>([]);
  const [draft, setDraft] = useState("用一句话介绍你自己。");
  const [maxOutputTokensText, setMaxOutputTokensText] = useState("4096");
  const [thinkingLevel, setThinkingLevel] =
    useState<TestThinkingLevel>("disabled");
  const [systemPrompt, setSystemPrompt] = useState("");
  const [viaGateway, setViaGateway] = useState<"direct" | "gateway">("direct");
  const [proxyMode, setProxyMode] = useState<ProxyMode>("bypass");
  const [customProxyUrl, setCustomProxyUrl] = useState("");
  const [streamEnabled, setStreamEnabled] = useState(false);
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<ModelTestResult | null>(null);
  // 流式测试过程中已经上屏的增量。失败时这里保留着断流前收到的内容，
  // 成功时由对话流里的助手气泡接管展示。
  const [streamedText, setStreamedText] = useState("");
  const [streamedReasoning, setStreamedReasoning] = useState("");
  /** 流式请求已收到的 HTTP 状态；`null` 表示还没收到响应头。 */
  const [streamStatus, setStreamStatus] = useState<number | null>(null);
  // 流式思考面板默认展开（推理模型的价值就在于能看着它想），但允许用户收起。
  // 必须受控：`<details open>` 写死为 true 时，每次增量重渲染都会把用户
  // 手动折叠的状态顶回去。
  const [showStreamedReasoning, setShowStreamedReasoning] = useState(true);
  // 上游每秒能推几百个 chunk，逐个 setState 会让 Markdown 每帧重解析几百遍。
  // 增量先落进这个缓冲区，用一帧一次的方式合并上屏。
  const streamBufferRef = useRef({ text: "", reasoning: "" });
  const streamFrameRef = useRef<number | null>(null);
  const [showConfig, setShowConfig] = useState(false);
  const [editingIndex, setEditingIndex] = useState<number | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const initialLoad = useRef(false);

  // 关闭弹窗时可能还有一帧增量在排队，取消掉，避免回调在卸载后才触发。
  useEffect(
    () => () => {
      if (streamFrameRef.current !== null) {
        cancelAnimationFrame(streamFrameRef.current);
        streamFrameRef.current = null;
      }
    },
    [],
  );

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

  // 流式输出时不能无条件滚到底：每来一帧增量就把视口拽回底部，用户想往上翻
  // 看前面说了什么就翻不动。所以只在“用户还停在底部”时跟随，一旦他滚上去就
  // 停止跟随，直到他自己滚回底部或点“回到底部”。
  const pinnedToBottomRef = useRef(true);
  const [showScrollToBottom, setShowScrollToBottom] = useState(false);

  /** 距底部多少像素以内仍算“停在底部”；留一点余量给半行的高度。 */
  const SCROLL_PIN_THRESHOLD = 32;

  const handleStreamScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
    const pinned = distance <= SCROLL_PIN_THRESHOLD;
    pinnedToBottomRef.current = pinned;
    setShowScrollToBottom(!pinned);
  };

  const scrollToBottom = () => {
    pinnedToBottomRef.current = true;
    setShowScrollToBottom(false);
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  };

  // 对话流新增内容后自动滚到底部（仅当用户没有往上翻）。
  useEffect(() => {
    if (!pinnedToBottomRef.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [history, running, result, streamedText, streamedReasoning]);

  const maxOutputTokens = Number(maxOutputTokensText);
  const maxTokensValid =
    Number.isInteger(maxOutputTokens) &&
    maxOutputTokens >= 1 &&
    maxOutputTokens <= MODEL_TEST_MAX_OUTPUT_TOKENS;
  const draftTrimmed = draft.trim();
  const canSend = !running && maxTokensValid && draftTrimmed.length > 0;

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

  /** 把本帧累积的增量一次性写入 state。 */
  const flushStreamedText = () => {
    streamFrameRef.current = null;
    const pending = streamBufferRef.current;
    if (pending.text) {
      const chunk = pending.text;
      pending.text = "";
      setStreamedText((current) => current + chunk);
    }
    if (pending.reasoning) {
      const chunk = pending.reasoning;
      pending.reasoning = "";
      setStreamedReasoning((current) => current + chunk);
    }
  };

  const resetStreamedText = () => {
    if (streamFrameRef.current !== null) {
      cancelAnimationFrame(streamFrameRef.current);
      streamFrameRef.current = null;
    }
    streamBufferRef.current = { text: "", reasoning: "" };
    setStreamedText("");
    setStreamedReasoning("");
    setStreamStatus(null);
    setShowStreamedReasoning(true);
    // 新一轮（或清空会话）默认重新跟随底部。
    pinnedToBottomRef.current = true;
    setShowScrollToBottom(false);
  };

  const updateHistoryMessage = (index: number, content: string) => {
    setHistory((current) =>
      current.map((message, i) =>
        i === index ? { ...message, content } : message,
      ),
    );
  };
  const deleteHistoryMessage = (index: number) => {
    setHistory((current) => current.filter((_, i) => i !== index));
  };
  const clearSession = () => {
    setHistory([]);
    setDraft("用一句话介绍你自己。");
    setResult(null);
    setEditingIndex(null);
    resetStreamedText();
  };

  const run = async () => {
    if (!model.upstreamModel.trim()) {
      toast.error("请先填写上游模型名");
      return;
    }
    if (!maxTokensValid) {
      toast.error(
        `最大输出 token 必须是 1 到 ${MODEL_TEST_MAX_OUTPUT_TOKENS} 之间的整数`,
      );
      return;
    }
    if (!draftTrimmed) {
      toast.error("请输入测试消息");
      return;
    }
    // 只回放带正文的助手轮次：思考耗尽预算、content 为空的轮次若原样回传，
    // 后端会以“测试消息内容不能为空”拒绝。这类轮次仍会展示在对话流里。
    const outgoingHistory = [
      ...history
        .filter(
          (message) => message.role === "user" || message.content.trim() !== "",
        )
        .map((message) => ({ role: message.role, content: message.content })),
      { role: "user" as ModelTestRole, content: draft },
    ];
    setRunning(true);
    setResult(null);
    resetStreamedText();
    // 流式模式下后端会把增量推到这个 channel 上，回复逐字上屏；结束后仍以
    // invoke 的返回值为准（usage、finish_reason、原始报文都在里面）。
    // 后端把该参数声明为必填（Tauri 的 Channel 只能以 CommandArg 注入），
    // 非流式模式下后端不会往里推任何事件。
    const channel = new Channel<ModelTestStreamEvent>();
    channel.onmessage = (event) => {
      if (event.kind === "start") {
        setStreamStatus(event.status);
        return;
      }
      if (event.kind === "delta") {
        const pending = streamBufferRef.current;
        pending.text += event.text;
        pending.reasoning += event.reasoning;
        if (
          streamFrameRef.current === null &&
          (pending.text || pending.reasoning)
        ) {
          streamFrameRef.current = requestAnimationFrame(flushStreamedText);
        }
      }
    };
    try {
      const res = await invoke<ModelTestResult>("test_gateway_model", {
        request: {
          provider: ctx.provider,
          upstreamModel: model.upstreamModel,
          alias: model.alias,
          apiFormat: model.apiFormat,
          messages: outgoingHistory,
          maxOutputTokens: maxOutputTokens,
          thinkingLevel,
          systemPrompt: systemPrompt.trim() || undefined,
          viaGateway: viaGateway === "gateway",
          proxyMode,
          customProxyUrl,
          stream: streamEnabled,
        },
        onEvent: channel,
      });
      setResult(res);
      if (res.ok) {
        const rawRequest = res.rawRequest ?? "";
        setHistory([
          ...history,
          { role: "user", content: draft, rawRequest },
          {
            role: "assistant",
            content: res.replyText,
            reasoningContent: res.reasoningText ?? "",
            rawRequest,
            rawResponse: res.rawBody,
          },
        ]);
        setDraft("");
      }
    } catch (error) {
      setResult({
        ok: false,
        status: 0,
        latencyMs: 0,
        replyText: "",
        rawBody: "",
        error: String(error),
        pathUsed: viaGateway,
        proxyEffective: null,
        lengthTruncated: false,
      });
    } finally {
      setRunning(false);
    }
  };

  const proxyDisabled = viaGateway === "gateway";
  // 有些聚合上游会把 reasoning_tokens 恒报为 0（实测 kimi-k3 那条路由），此时它
  // 给的“推理 0”不是事实——我们确实收到了思考正文。标注一下，别让人以为模型没思考。
  const reasoningTokens = result?.usage?.reasoningTokens;
  const reasoningUncounted =
    (reasoningTokens ?? 0) === 0 && !!result?.reasoningText;
  const usageSummary = result?.usage
    ? [
        result.usage.inputTokens != null
          ? `输入 ${result.usage.inputTokens}`
          : null,
        result.usage.outputTokens != null
          ? `输出 ${result.usage.outputTokens}`
          : null,
        result.usage.totalTokens != null
          ? `合计 ${result.usage.totalTokens}`
          : null,
        result.usage.cacheReadInputTokens != null
          ? `缓存读 ${result.usage.cacheReadInputTokens}`
          : null,
        reasoningTokens != null
          ? reasoningUncounted
            ? "推理 0（上游未单独计数）"
            : `推理 ${reasoningTokens}`
          : reasoningUncounted
            ? "推理 上游未上报"
            : null,
      ].filter(Boolean)
    : [];

  // 思考占满全部输出预算：截断的根因不是“输出 token 不够”，而是模型把预算
  // 全花在推理上、没产出正文。此时提示“调高最大输出 token”会误导用户。
  const resultUsage = result?.usage;
  const reasoningExhausted =
    !!result?.lengthTruncated &&
    ((resultUsage?.reasoningTokens != null &&
      resultUsage.outputTokens != null &&
      resultUsage.reasoningTokens >= resultUsage.outputTokens) ||
      // 计数不可信时的兜底：「截断 + 只收到思考、正文为空」本身就是预算被思考吃光的
      // 形状，比上游给的那个数更可靠。
      (!!result.reasoningText && !result.replyText.trim()));

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-6"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="panel flex max-h-[90vh] w-full max-w-3xl flex-col shadow-2xl">
        {/* 标题栏 */}
        <div className="flex items-start justify-between gap-3 border-b p-4">
          <div className="min-w-0">
            <h2 className="text-lg font-semibold">模型测试对话</h2>
            <p className="mt-1 truncate text-xs text-muted-foreground">
              {ctx.providerName} · 上游模型{" "}
              <code className="font-mono">{model.upstreamModel || "(空)"}</code>{" "}
              · 协议 {formatLabels[model.apiFormat]}
              {savedAlias ? (
                <>
                  {" "}
                  · 别名 <code className="font-mono">{savedAlias}</code>
                </>
              ) : null}
            </p>
          </div>
          <div className="flex items-center gap-1">
            <button
              className={cn("secondary-button", showConfig && "bg-muted")}
              onClick={() => setShowConfig(!showConfig)}
              title="请求路径 / 出口方式 / token"
              aria-label="请求路径 / 出口方式 / token"
            >
              <Settings className="h-4 w-4" />
              配置
              <ChevronDown
                className={cn(
                  "h-3.5 w-3.5 transition",
                  showConfig && "rotate-180",
                )}
              />
            </button>
            <button className="icon-button" onClick={onClose} aria-label="关闭">
              <X className="h-4 w-4" />
            </button>
          </div>
        </div>

        {/* 可折叠配置面板 */}
        {showConfig && (
          <div className="max-h-[45vh] shrink-0 space-y-4 overflow-y-auto border-b bg-muted/20 p-4">
            <div>
              <div className="mb-2 text-xs font-medium text-muted-foreground">
                系统提示词
              </div>
              <textarea
                className="input min-h-16 w-full resize-y text-xs leading-5"
                value={systemPrompt}
                onChange={(event) => setSystemPrompt(event.target.value)}
                disabled={running}
                placeholder="可选。留空则不注入 system 消息。"
              />
            </div>
            <div>
              <div className="mb-2 text-xs font-medium text-muted-foreground">
                请求路径
              </div>
              <div className="grid grid-cols-2 gap-1 rounded-lg bg-muted p-1">
                <SegmentedOption
                  name="viaGateway"
                  checked={viaGateway === "direct"}
                  onChange={() => setViaGateway("direct")}
                >
                  直连上游
                </SegmentedOption>
                <SegmentedOption
                  name="viaGateway"
                  checked={viaGateway === "gateway"}
                  disabled={!gatewayAllowed}
                  onChange={() => setViaGateway("gateway")}
                >
                  通过本地网关
                </SegmentedOption>
              </div>
              {!gatewayAllowed && (
                <div className="mt-2 text-[11px] leading-5 text-muted-foreground">
                  {gatewayRunning
                    ? "此模型条目尚未保存到运行配置中，无法通过网关测试。"
                    : "网关未启动，无法通过网关测试。"}
                </div>
              )}
            </div>

            <div className={cn(proxyDisabled && "opacity-60")}>
              <div className="mb-2 flex flex-wrap items-center gap-2">
                <Shield className="h-4 w-4 text-muted-foreground" />
                <span className="text-xs font-medium">出口方式</span>
                {proxyDisabled && (
                  <span className="text-[11px] text-muted-foreground">
                    通过网关时由网关全局设置决定
                  </span>
                )}
              </div>
              <div className="grid grid-cols-3 gap-1 rounded-lg bg-muted p-1">
                <SegmentedOption
                  name="proxyMode"
                  checked={!proxyDisabled && proxyMode === "bypass"}
                  disabled={proxyDisabled}
                  onChange={() => setProxyMode("bypass")}
                >
                  直接连接
                </SegmentedOption>
                <SegmentedOption
                  name="proxyMode"
                  checked={!proxyDisabled && proxyMode === "follow_global"}
                  disabled={proxyDisabled}
                  onChange={() => setProxyMode("follow_global")}
                >
                  跟随全局
                </SegmentedOption>
                <SegmentedOption
                  name="proxyMode"
                  checked={!proxyDisabled && proxyMode === "custom"}
                  disabled={proxyDisabled}
                  onChange={() => setProxyMode("custom")}
                >
                  临时代理
                </SegmentedOption>
              </div>
              {!proxyDisabled && proxyMode === "bypass" && (
                <p className="mt-2 text-[11px] leading-5 text-muted-foreground">
                  本次测试强制直连上游，不使用全局设置或系统环境变量中的代理。
                </p>
              )}
              {proxyMode === "custom" && !proxyDisabled && (
                <input
                  className="input mt-2 w-full font-mono"
                  value={customProxyUrl}
                  onChange={(event) => setCustomProxyUrl(event.target.value)}
                  placeholder="http://127.0.0.1:7890 或 socks5://127.0.0.1:1080"
                />
              )}
            </div>

            <div className="flex flex-wrap items-center gap-3">
              <label className="flex flex-col gap-1 text-xs text-muted-foreground">
                最大输出 token
                <input
                  className="input w-32 font-mono"
                  type="number"
                  min={1}
                  max={MODEL_TEST_MAX_OUTPUT_TOKENS}
                  step={1}
                  value={maxOutputTokensText}
                  onChange={(event) =>
                    setMaxOutputTokensText(event.target.value)
                  }
                />
              </label>
              <div className="flex flex-col gap-1">
                <span className="text-xs text-muted-foreground">思考等级</span>
                <div className="grid grid-cols-4 gap-1 rounded-lg bg-muted p-1">
                  <SegmentedOption
                    name="thinkingLevel"
                    checked={thinkingLevel === "disabled"}
                    onChange={() => setThinkingLevel("disabled")}
                  >
                    关闭
                  </SegmentedOption>
                  <SegmentedOption
                    name="thinkingLevel"
                    checked={thinkingLevel === "low"}
                    onChange={() => setThinkingLevel("low")}
                  >
                    低
                  </SegmentedOption>
                  <SegmentedOption
                    name="thinkingLevel"
                    checked={thinkingLevel === "medium"}
                    onChange={() => setThinkingLevel("medium")}
                  >
                    中
                  </SegmentedOption>
                  <SegmentedOption
                    name="thinkingLevel"
                    checked={thinkingLevel === "high"}
                    onChange={() => setThinkingLevel("high")}
                  >
                    高
                  </SegmentedOption>
                </div>
              </div>
              <div className="flex flex-col gap-1">
                <span className="text-xs text-muted-foreground">流式传输</span>
                <label
                  className="switch-label min-h-10 rounded-lg border border-border bg-muted/30 px-3"
                  title="向上游请求 SSE，回复逐字上屏；结束后仍会给出 usage 与停止原因"
                >
                  <input
                    type="checkbox"
                    checked={streamEnabled}
                    disabled={running}
                    onChange={(event) => setStreamEnabled(event.target.checked)}
                  />
                  启用
                </label>
              </div>
              {!maxTokensValid && maxOutputTokensText !== "" && (
                <span className="text-[11px] text-destructive">
                  范围 1–{MODEL_TEST_MAX_OUTPUT_TOKENS}
                </span>
              )}
              <button
                className="secondary-button ml-auto"
                disabled={running || history.length === 0}
                onClick={clearSession}
                title="清空当前会话"
              >
                <Eraser className="h-4 w-4" />
                清空会话
              </button>
            </div>
          </div>
        )}

        {/* 对话流 */}
        <div className="relative flex min-h-0 flex-1 flex-col">
          <div
            ref={scrollRef}
            onScroll={handleStreamScroll}
            className="min-h-0 flex-1 space-y-4 overflow-y-auto p-4"
          >
            {history.length === 0 && !running && (
              <div className="py-10 text-center text-xs text-muted-foreground">
                输入消息后按 Cmd/Ctrl + Enter 发送，回复会渲染 Markdown。
              </div>
            )}
            {history.map((message, index) => {
              const isUser = message.role === "user";
              const isEditing = editingIndex === index;
              return (
                <div
                  key={index}
                  className={cn(
                    "group flex",
                    isUser ? "justify-end" : "justify-start",
                  )}
                >
                  <div
                    className={cn(
                      "relative max-w-[85%] rounded-2xl px-4 py-2.5",
                      isUser
                        ? "rounded-br-sm bg-primary/10"
                        : "rounded-bl-sm bg-muted/40",
                    )}
                  >
                    <div className="mb-1 flex items-center justify-between gap-2">
                      <span
                        className={cn(
                          "text-[10px] font-medium",
                          isUser
                            ? "text-primary"
                            : "text-emerald-600 dark:text-emerald-400",
                        )}
                      >
                        {isUser ? "用户" : "助手"}
                      </span>
                      <div className="flex items-center gap-0.5 opacity-0 transition group-hover:opacity-100">
                        {!isUser && (
                          <button
                            className="icon-button h-7 w-7"
                            onClick={() => onCopy(message.content, "助手回复")}
                            title="复制该回复"
                            aria-label="复制该回复"
                            disabled={running}
                          >
                            <Copy className="h-3.5 w-3.5" />
                          </button>
                        )}
                        <button
                          className="icon-button h-7 w-7"
                          onClick={() =>
                            setEditingIndex(isEditing ? null : index)
                          }
                          title={isEditing ? "完成编辑" : "编辑该条"}
                          aria-label={isEditing ? "完成编辑" : "编辑该条"}
                          disabled={running}
                        >
                          {isEditing ? (
                            <Check className="h-3.5 w-3.5" />
                          ) : (
                            <Pencil className="h-3.5 w-3.5" />
                          )}
                        </button>
                        <button
                          className="icon-button h-7 w-7 text-destructive"
                          onClick={() => {
                            deleteHistoryMessage(index);
                            setEditingIndex(null);
                          }}
                          title="删除该条"
                          aria-label="删除该条"
                          disabled={running}
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    </div>
                    {isEditing ? (
                      <textarea
                        className="input min-h-16 w-full resize-y text-xs leading-5"
                        value={message.content}
                        onChange={(event) =>
                          updateHistoryMessage(index, event.target.value)
                        }
                        disabled={running}
                      />
                    ) : isUser ? (
                      <div className="whitespace-pre-wrap break-words text-sm leading-6">
                        {message.content}
                      </div>
                    ) : (
                      <>
                        {message.reasoningContent?.trim() ? (
                          <details className="mb-2 border-b border-border/60 pb-2">
                            <summary className="flex cursor-pointer list-none items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground">
                              <ChevronRight className="h-3 w-3 transition [[open]>&]:rotate-90" />
                              思考过程
                            </summary>
                            <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 font-mono text-[10px] leading-4 text-muted-foreground">
                              {message.reasoningContent}
                            </pre>
                          </details>
                        ) : null}
                        {message.content.trim() ? (
                          <Markdown content={message.content} />
                        ) : (
                          <div className="text-xs italic text-muted-foreground">
                            {message.reasoningContent?.trim()
                              ? "（模型未产出正式回复，仅返回了思考内容）"
                              : "（模型未返回任何文本内容）"}
                          </div>
                        )}
                      </>
                    )}
                    {/* 每条气泡各自的原始请求/响应，默认收起。 */}
                    {isUser && message.rawRequest && (
                      <details className="mt-2 border-t border-border/60 pt-2">
                        <summary className="flex cursor-pointer list-none items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground">
                          <ChevronRight className="h-3 w-3 transition [[open]>&]:rotate-90" />
                          原始请求
                        </summary>
                        <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-all rounded bg-muted p-2 font-mono text-[10px] leading-4">
                          {message.rawRequest}
                        </pre>
                      </details>
                    )}
                    {!isUser && message.rawResponse && (
                      <details className="mt-2 border-t border-border/60 pt-2">
                        <summary className="flex cursor-pointer list-none items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground">
                          <ChevronRight className="h-3 w-3 transition [[open]>&]:rotate-90" />
                          原始响应
                        </summary>
                        <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-all rounded bg-muted p-2 font-mono text-[10px] leading-4">
                          {message.rawResponse}
                        </pre>
                      </details>
                    )}
                  </div>
                </div>
              );
            })}

            {/* 运行中加载气泡；流式模式下有内容后转为实时上屏的助手气泡 */}
            {running && (
              <div className="flex justify-start">
                {streamEnabled && (streamedText || streamedReasoning) ? (
                  <div className="group relative max-w-[85%] rounded-2xl rounded-bl-sm bg-muted/40 px-4 py-2.5">
                    <div className="mb-1 flex items-center gap-2">
                      <span className="text-[10px] font-medium text-emerald-600 dark:text-emerald-400">
                        助手
                      </span>
                      <span className="flex items-center gap-1 text-[10px] text-muted-foreground">
                        <RefreshCw className="h-3 w-3 animate-spin" />
                        {streamStatus
                          ? `已连接 · HTTP ${streamStatus}`
                          : "接收中…"}
                      </span>
                    </div>
                    {streamedReasoning ? (
                      <details
                        className="mb-2 border-b border-border/60 pb-2"
                        open={showStreamedReasoning}
                        onToggle={(event) =>
                          setShowStreamedReasoning(event.currentTarget.open)
                        }
                      >
                        <summary className="flex cursor-pointer list-none items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground">
                          <ChevronRight className="h-3 w-3 transition [[open]>&]:rotate-90" />
                          思考过程
                        </summary>
                        <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 font-mono text-[10px] leading-4 text-muted-foreground">
                          {streamedReasoning}
                        </pre>
                      </details>
                    ) : null}
                    {streamedText ? (
                      <Markdown content={streamedText} />
                    ) : (
                      <div className="text-xs italic text-muted-foreground">
                        （模型尚未产出正文，正在输出思考内容）
                      </div>
                    )}
                  </div>
                ) : (
                  <div className="rounded-2xl rounded-bl-sm bg-muted/40 px-4 py-2.5">
                    <div className="flex items-center gap-2 text-xs text-muted-foreground">
                      <RefreshCw className="h-3.5 w-3.5 animate-spin" />
                      {streamEnabled ? "等待上游首字…" : "等待上游回复…"}
                    </div>
                  </div>
                )}
              </div>
            )}

            {/* 最近一次结果状态条 / 失败错误气泡 */}
            {result && (
              <div
                className={cn(
                  "rounded-lg border px-3 py-2 text-[11px] leading-5",
                  result.ok
                    ? "border-border bg-muted/30 text-muted-foreground"
                    : "border-destructive/30 bg-destructive/5 text-destructive",
                )}
              >
                {result.ok ? (
                  <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
                    <span className="font-medium text-emerald-600 dark:text-emerald-400">
                      成功
                    </span>
                    {result.status > 0 && <span>· HTTP {result.status}</span>}
                    <span>· {result.latencyMs}ms</span>
                    <span>
                      · {result.pathUsed === "gateway" ? "通过网关" : "直连"}
                    </span>
                    <span>
                      ·{" "}
                      {result.proxyEffective
                        ? `代理 ${result.proxyEffective}`
                        : "无代理"}
                    </span>
                    {result.finishReason && (
                      <span>· 停止原因 {result.finishReason}</span>
                    )}
                    {usageSummary.length > 0 && (
                      <span>· {usageSummary.join(" · ")}</span>
                    )}
                    {result.lengthTruncated && (
                      <span className="text-amber-700 dark:text-amber-300">
                        {reasoningExhausted
                          ? `· 输出因长度上限被截断（${result.finishReason ?? "length"}）：思考已占用全部 ${result.usage?.outputTokens} token，模型未产出正文`
                          : `· 输出因长度上限被截断（${result.finishReason ?? "length"}），可调高最大输出 token`}
                      </span>
                    )}
                  </div>
                ) : (
                  <div className="break-words">
                    <span className="font-medium">失败</span>
                    {result.status > 0 && <span> · HTTP {result.status}</span>}
                    {result.error && <span> · {result.error}</span>}
                  </div>
                )}
                {/* 流式请求中途失败：把断流前已经收到的内容留下来，别让它随失败一起消失。 */}
                {!result.ok && (streamedText || streamedReasoning) && (
                  <div className="mt-2 border-t border-border/60 pt-2">
                    <div className="text-[11px] text-muted-foreground">
                      断流前已收到（正文 {streamedText.length} 字 / 思考{" "}
                      {streamedReasoning.length} 字）
                    </div>
                    {streamedReasoning && (
                      <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 font-mono text-[10px] leading-4 text-muted-foreground">
                        {streamedReasoning}
                      </pre>
                    )}
                    {streamedText && (
                      <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 font-mono text-[10px] leading-4">
                        {streamedText}
                      </pre>
                    )}
                  </div>
                )}
                {/* 失败时没有助手气泡，这里补一个原始响应入口，方便排查。 */}
                {!result.ok && result.rawBody && (
                  <details className="mt-2 border-t border-border/60 pt-2">
                    <summary className="flex cursor-pointer list-none items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground">
                      <ChevronRight className="h-3 w-3 transition [[open]>&]:rotate-90" />
                      原始响应
                    </summary>
                    <pre className="mt-2 max-h-60 overflow-auto whitespace-pre-wrap break-all rounded bg-muted p-2 font-mono text-[10px] leading-4">
                      {result.rawBody}
                    </pre>
                  </details>
                )}
              </div>
            )}
          </div>

          {showScrollToBottom && (
            <button
              type="button"
              onClick={scrollToBottom}
              title="跳回最新内容并恢复自动跟随"
              className="secondary-button absolute bottom-3 left-1/2 h-auto -translate-x-1/2 rounded-full px-3 py-1.5 shadow-lg"
            >
              <ChevronDown className="h-3.5 w-3.5" />
              回到底部
            </button>
          )}
        </div>

        {/* 底部输入区 */}
        <div className="border-t p-4">
          <textarea
            className="input min-h-20 w-full resize-y"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            disabled={running}
            placeholder="输入测试消息，Cmd/Ctrl + Enter 发送"
            onKeyDown={(event) => {
              if (
                (event.metaKey || event.ctrlKey) &&
                event.key === "Enter" &&
                canSend
              ) {
                event.preventDefault();
                void run();
              }
            }}
          />
          <div className="mt-2 flex items-center justify-between gap-2">
            <button className="secondary-button" onClick={onClose}>
              关闭
            </button>
            <button
              className="primary-button"
              disabled={!canSend}
              onClick={() => void run()}
            >
              {running ? (
                <RefreshCw className="h-4 w-4 animate-spin" />
              ) : (
                <Send className="h-4 w-4" />
              )}
              发送
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
