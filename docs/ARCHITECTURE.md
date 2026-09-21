# 架构说明

核对日期：2026-09-20。依据当前源码；验证范围见 [开发状态](DEVELOPMENT_STATUS.md)。

## 产品边界

单机桌面 LLM 网关：统一本地入口、管理上游 API、模型别名、协议转换、稳定转发与诊断。不以多用户、销售计费、订阅套餐或团队权限为目标。

```text
React 配置 / 测试台 / 托盘 → Tauri commands → GatewayConfig / SQLite
客户端 → axum → 本地鉴权 → 别名候选集 → 路由策略 / 会话亲和
       → 容量控制 / 冷却 / 熔断 → 协议转换 → 上游
       ← JSON / SSE 转换、usage 与诊断
```

## 配置与路由

`src-tauri/src/gateway.rs` 是产品配置入口。当前结构是 `GatewayConfig.providers[].models[]`：

- `GatewayProvider` 保存 URL、凭证、请求头、诊断开关、并发和排队参数。
- `GatewayProviderModel` 保存 `alias`、`upstream_model`、`api_format`、启用状态和能力元数据。协议属于模型条目，同一供应商可以配置不同协议。
- 相同 alias 的启用模型条目构成候选集，默认按供应商配置顺序选择。
- `routing_policies` 支持 `priority`、`round_robin`、`weighted_round_robin`、`least_outstanding`；`routing_weights` 保存别名下各供应商的权重。
- `model_registry_overrides` 与数据目录下的 `model-registry.overrides.json` 管理模型能力修正；内置能力来源为 `resources/model_capabilities.json`。

配置仍存储于 SQLite `settings` 表的 `unified_gateway_config_v1` 键。键名没有随结构变化而升级；`parse_config_with_migration` / `migrate_legacy_config` 负责读取旧 `routes/targets` 结构。旧文档中的 `GatewayRoute`、`GatewayRouteTarget` 已不是当前配置类型。

为了复用原代理，每个供应商与协议组合物化为 `unified_gateway` 分类下的内部 provider，并分别写入 `claude`、`codex` 应用域。它们不是本机 CLI 配置。

`proxy/handler_context.rs` 先解析统一网关别名，再应用路由策略；有效会话绑定优先于重新分配。网关配置存在时，未知别名返回明确的请求错误，不再进入继承 provider router；仅未配置统一网关的旧应用路径保留旧行为。

## 请求与响应

`proxy/server.rs` 暴露：

```text
GET  /health
GET  /v1/gateway/status
GET  /v1/models
POST /v1/chat/completions
POST /v1/responses
POST /v1/responses/compact
POST /v1/messages
```

网关公开三类客户端协议。继承目录含 Gemini、OAuth 等实现，不代表这些能力已通过当前 UI、配置和 HTTP 路由成为产品功能。

Chat 和 Responses 入口共享 OpenAI handler，保留原始请求直到选定候选。`proxy/request_plan.rs` 在每次尝试中按候选协议决定转换：Chat → Chat 直接转发，只进行模型映射、显式兼容配置及已有私有字段过滤；跨协议才调用 `gateway_chat.rs`。成功响应按实际成功候选的协议处理，原生 Chat 的 JSON/SSE 不绕 Responses。响应协议和应用配置域分开，Chat SSE 使用自己的终态检查与 usage parser。

推理接口由已知服务端域名或供应商显式 `chatReasoningProfile` 决定，模型名和展示名称不能触发厂商字段注入。原生 Chat 在 auto 下保留客户端推理参数；跨协议未知服务商采用 OpenAI effort 字段。工具 schema 默认保留；`chatSchemaRequiredDefaults` 仅为要求此字段的服务商补齐非 strict 工具缺省的 required 数组，不删除 null 或改写 enum/items。

`proxy/providers/` 负责三类协议转换、工具调用、reasoning 与 SSE。`forwarder.rs` 组织发送、错误处理与故障转移；`provider_router.rs` 提供路由、供应商容量控制、排队和冷却；`session_affinity.rs` 保存有界的会话绑定。并发容量按源供应商共享，避免物化为多个协议后重复获得并发额度。

已有 429 / Retry-After 冷却、熔断、会话重新绑定及取消排队计数。供应商明确拒绝参数/模型能力的 400/422 可换源，释放熔断器探测名额但不累计供应商健康失败；无效 JSON、工具历史错误和未识别的 400 仍终止。每家仅从原始请求构建自己的出站参数。HTTP 200 错误正文和 OpenAI SSE 在实际输出前的错误也在重试循环内识别；输出已提交后不重放请求。详见 [路由可靠性改造](ROUTING_RELIABILITY.md)。

## 桌面、数据与继承代码

`lib.rs` 负责专用启动、Tauri 命令、托盘、监听恢复；这些路径统一使用 `AppState.gateway_runtime`。`gateway_runtime.rs` 独立管理监听、停止、配置热更新和状态，使用同一个锁串行化启动/停止/重新绑定；新端口绑定失败时保留原监听及代理配置。它不调用 CLI 配置接管服务。`forwarder.rs` 也禁止统一网关 provider 的成功路由触发继承的 live 配置切换。

`lightweight.rs`、`auto_launch.rs`、`portable.rs` 负责桌面生命周期。默认数据目录为 `~/.llm-gateway-desktop/`；可执行文件旁存在 `portable.flag` 时使用旁边的 `data/`。

入口不执行原应用的 CLI 配置接管、MCP、Skills 和提示词初始化。首轮已删除未注册的认证聚合、深链、导入导出、模型发现包装、OMO、插件、S3/WebDAV 同步、workspace 和 session manager 命令，以及整个 CLI 会话历史浏览模块，共约 6,700 行；模型发现本身仍由 gateway 命令提供。

`lib.rs`、`services/mod.rs`、`commands/mod.rs` 仍声明部分继承模块，旧 `ProxyService` 暂时留在 `AppState` 供继承服务引用。数据库及 provider 类型仍与其有依赖。运行入口已分离不等于全部继承依赖已经移除，后续应按编译依赖继续裁剪。

后续精简应先分离网关运行服务与外部应用配置服务，随后按编译依赖移除模块；旧数据库结构应通过迁移处理。不能仅凭目录名批量删除。

## 安全与诊断默认值

- 默认监听 `127.0.0.1:10888`，启用本地访问密钥。
- 凭证与配置保存在本地，备份应作为敏感数据处理。
- 供应商或模型可显式开启请求/响应正文录制，默认关闭；录制文件位于日志目录的 `request-bodies/` 下。
- 本地 usage、延迟、失败原因与并发状态用于诊断，不需要扩展为销售计费系统。

选型结论与后续顺序见 [内核选型评估](KERNEL_COMPARISON.md)。

## 回归入口

`gateway/protocol_tests.rs` 使用内存 SQLite 和临时端口，沿真实 `GatewayRuntime → ProxyServer → handler → forwarder → mock upstream` 路径发 HTTP 请求。54 个组合覆盖三种下游 × 三种上游 × JSON/SSE × 文本/工具调用/工具结果；工具结果使用实际返回的工具 ID。另有三种下游 JSON/SSE 的鉴权、故障转移与 429 冷却测试。

Chat SSE 桥共用增量 UTF-8 解码和 SSE 分块，避免中文跨网络分片时损坏；提前 EOF 或明确失败输出错误，不再伪造成功的 `finish_reason: stop`。针对逐字节 UTF-8、CRLF、断流和重复终态有额外测试。
