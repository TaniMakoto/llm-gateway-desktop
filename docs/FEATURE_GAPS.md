# 本地网关功能差距与优先级

核对日期：2026-09-20。基于本项目实施后的源码，以及 `_refs/CPA` 的 `ac02da6c`、`_refs/sub2api` 的 `bdb42e22` 本地快照。未假定这些是上游最新版本，也未对真实平台做兼容性对测。

## 客户端伪装：确有欠缺，但不是完全没有

需要把“自定义 UA”“应用层兼容档案”“设备/会话标识”“TLS 指纹”分开。它们不是一个开关可以等价替代的能力。

| 层次 | 当前项目 | 参考实现 | 判断 |
|---|---|---|---|
| 自定义请求头/UA | UI、GatewayProvider、实际转发已接通 | 两者均有 | 已有 |
| Codex 兼容头 | `client_fingerprint()` 生成 UA、originator、version；支持版本覆盖 | CPA 有按认证方式区分的默认头；sub2api 另有设备/会话标识策略 | 基础能力已接通，范围较窄 |
| Claude Code 应用层兼容 | `ProviderMeta.impersonate_claude_code` 与 forwarder 中的分支存在，但 GatewayProvider/UI 不暴露，物化也不赋值 | CPA 有 cloak、fingerprint-profile、客户端检测及配套请求规范化 | **内核局部实现未产品化** |
| 请求体兼容 | 现有旧 Claude 分支只在 Responses → Anthropic 条件下启用；包含身份 system 块等处理 | CPA 还处理 system 组织、工具别名、beta 集合、metadata 等 | 不能只加一个 UI checkbox 就宣称追平 |
| 稳定设备/会话身份 | 已有路由会话亲和；没有面向用户的完整客户端身份配置 | sub2api 有 Codex off/device/session/full 模式与每账号 seed；CPA 有用户标识和设备档案处理 | 身份管理与路由亲和是两回事 |
| TLS 指纹 | 使用现有 Rust TLS/HTTP 栈；已有原始头大小写/顺序处理，未提供可配置 TLS 指纹模板 | sub2api 有专门的 TLS fingerprint dialer/profile | 确实缺少，普通 API Key 网关不应因此默认引入复杂性 |

当前源码证据：

- `src-tauri/src/gateway.rs`：`client_fingerprint()`、`provider_meta_with_registry()`。后者只传递目前公开字段，未设置 `impersonate_claude_code`。
- `src-tauri/src/provider.rs`：继承的 `impersonate_claude_code`、`LocalProxyRequestOverrides`。
- `src-tauri/src/proxy/forwarder.rs`：`codex_impersonate_claude_code` 分支、旧固定 UA、system 前缀和跨协议指纹头过滤。
- `src-tauri/src/proxy/server.rs`、`hyper_client.rs`：已有原始 HTTP 头大小写处理，不能算作完全没有传输兼容。

参考源码：CPA `internal/runtime/executor/claude_executor_cloaking.go` 和 `config.example.yaml` 的 cloak/fingerprint-profile/claude-header-defaults；sub2api `backend/internal/service/openai_codex_fingerprint.go`、`account.go` 和 `backend/internal/pkg/tlsfingerprint/dialer.go`。

建议先做显式、可测试的 provider 级兼容档案：保持调用方、Codex 兼容、Claude Messages 兼容。将头部覆盖、system 变更、metadata 处理各自定义清楚，提供最终出站请求预览和回归；不同下游应得到一致的目标上游行为。TLS 与账号身份策略留作单独能力，按实际接入需要决定。不能靠静态伪造一个 UA 就承诺所有上游兼容，也不应照搬参考项目中会改变用户提示词的所有规则。

## 其他值得补的差距

| 优先级 | 能力 | 当前情况与参考差异 | 建议范围 |
|---|---|---|---|
| P1 | 同协议保真/透传 | Chat 总是先转 Responses，工具/reasoning/扩展字段可能受往返转换影响；sub2api 有专门 passthrough 路径 | 同协议优先透传，跨协议才转换；先用字段级回归约束行为 |
| P1 | 按供应商的超时和重试策略 | 内核已有错误分类、重试、首字节检查与 429 冷却；gateway 启动配置固定 `max_retries=10`，没有完整 provider 级设置 | 连接/首字节/空闲超时、尝试上限和可重试状态；明确开始输出后不能透明重播 |
| P1 | 声明式请求参数规则 | 有 reasoning 专项选项；继承的 body override 类型存在，但 gateway 物化固定 `body: None` | 按供应商/模型设置默认值、强制覆盖、删除不兼容字段；避免频繁为单个中转站改代码 |
| P1 | Token 计数入口 | 当前路由表没有 `/v1/messages/count_tokens`；参考程序有对应处理 | 优先对支持的上游透传；跨协议估算须明确标记，不能冒充精确计数 |
| P1/P2 | 错误与流稳定性完整矩阵 | 已补 54 组合及额外 SSE/鉴权/冷却测试；还没覆盖全部 reasoning、图像、structured output、compact 和早期错误组合 | 扩展现有矩阵，尤其首输出前失败、工具多轮、取消与内容完整性 |
| P2 | 按供应商出站代理 | 产品常规转发配置为全局代理；测试台可单次选代理，但不等于 provider 独立出口 | 每供应商可继承全局、直连或指定代理；连接池按出口正确隔离 |
| P2 | 更细的健康/配额状态 | 已有 provider 级冷却、熔断、排队及四策略；CPA 有 auth/model 级冷却，sub2api 调度记录错误率和 TTFT EWMA | 可按模型冷却，区分整个凭证失效与单模型限流；必要时再加延迟/错误率调度 |
| 条件性 P1，否则 P2 | OAuth 生命周期和账号池 | 当前产品是每 provider 一个 API key，没有完整登录/刷新/过期/账号池入口；CPA/sub2api 是核心能力 | 需求明确后优先评估 CPA 内核复用，不要把文件中存在 OAuth 代码当成已支持 |
| P2 | 模型目录更新与准确能力暴露 | 已有本地集中注册表、模型发现、手动修正；CPA 有远程目录更新器 | 保留本地覆盖，增加可审阅的版本化目录更新；不要自动覆盖用户修正 |
| 按需 | Gemini 原生、Responses WS、Realtime | 当前公开入口没有；参考程序有相关实现 | 由真实客户端需求决定，普通 HTTP/SSE 不必为“功能齐全”承担全部复杂度 |
| 按需 | Embeddings、图片/音频等入口 | 当前路由表没有；sub2api 路由含 embeddings、images、realtime 等 | 如果网关服务 RAG/多模态客户端再加，不混入当前文本协议重构 |

P1 表示对当前本地网关较直接的收益，不是本次已获批准逐项实现的承诺；本次新增需求是分析差距。

## 不能重复算作缺失的能力

- priority / round-robin / weighted-round-robin / least-outstanding 已有。
- 会话亲和、429 的 Retry-After 秒数/日期解析、熔断、供应商并发和有界排队已有。
- 首块预读以及部分 Responses 语义失败的首输出前处理已有。差距是不同路径的一致性、配置粒度和覆盖度，不是“完全不会安全重试”。
- 模型别名、每模型上游协议、模型能力覆盖、请求录制、测试台、便携包已有。
- 路由粘性不等于客户端身份伪装；有前者不能证明后者已经完成。

补充源码依据：CPA `config.example.yaml` 中的 request-retry、routing、streaming、proxy-url、payload 规则，以及 `cmd/server/main.go` 的模型更新器；sub2api `backend/internal/service/openai_account_scheduler.go`、`gateway_anthropic_passthrough.go`、`openai_gateway_passthrough.go` 和 `backend/internal/server/routes/gateway.go`。当前对应入口见 `gateway.rs`、`proxy/forwarder.rs`、`proxy/server.rs` 和 `model_capabilities.rs`。

## 不纳入追平目标

用户注册、租户/团队权限、余额扣费、套餐/订阅销售、支付、返佣、邀请、商业审计和分布式集群管理。保留本地 token usage、上游配额、失败率与耗时，因为它们用于诊断和路由，而非商业计费。

## 建议下一批顺序

先做客户端兼容档案和声明式参数规则，再做同协议保真与每供应商重试/超时，随后补 count_tokens、独立出站代理和更广的兼容矩阵。若确定 OAuth 账号池是近期核心需求，提前执行 CPA 原型，重新衡量继续自建这些能力的成本。无需为上述差距迁移到 sub2api。
