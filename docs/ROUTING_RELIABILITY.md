# 路由可靠性改造

## 改造依据

参考本地 CPA 快照的 `sdk/translator/registry.go`、`internal/runtime/executor/openai_compat_executor.go`、`sdk/cliproxy/auth/selector.go`、`sdk/cliproxy/auth/conductor_cooldown.go` 和 `internal/clienterror/client_error.go`：区分源/目标协议、在执行候选时适配、先过滤不可用候选、结合错误语义判断模型支持和请求错误。没有引入 Go 依赖或复制整个 CPA 调度器；桌面端继续使用现有配置和数据模型。

## 请求契约

1. 保存客户端原始 JSON，候选切换不能继承上一家的转换结果。
2. Chat 同协议直接转发，保留工具 schema、历史 reasoning、采样参数、响应扩展和多 choice 请求；跨协议仍有表示能力限制，例如 n > 1 会跳过无法表示它的候选。
3. 模型映射和协议转换独立。响应处理依据最终成功候选，不能根据首选候选猜测响应格式。
4. Chat 和 Responses 使用各自的 SSE 终态与 usage 解析器；不通过改写配置域假装是另一个应用。
5. 已配置网关拒绝未知别名，避免漏到继承的默认供应商。

## 推理与工具兼容

`chatReasoningProfile` 支持 auto、openai、deepseek、openrouter、siliconflow、disabled。auto 对原生 Chat 保留客户端字段；对 Responses → Chat 只按确切的已知接口域名推断厂商格式。未知中转不因模型名称含 deepseek/kimi/glm 等注入厂商字段。显式 profile 可用于采用厂商格式的私有代理。

`chatSchemaRequiredDefaults` 默认关闭。开启时只遍历 schema 位置，为对象 schema 的缺省 required 补 []；保留已有 required、显式 null、enum 中的 null、examples 和 strict:true 工具。这是服务商兼容选项，不意味着 Hermes 缺省 required 的请求违反 JSON Schema。

不能仅凭“null is not of type array”证明 null 由网关生成。真实渠道仍需比较客户端输入和最终出站录制；本次回归断言整份原生 Chat 出站请求仅改变模型名。

## 故障转移

- 网络、服务端故障、限流：使用有界候选循环；同一请求不会重复尝试同一候选。
- 明确参数或模型不支持的 400/404/422：换候选，不把整家供应商标坏；模型不支持和普通 404 按 CPA 规则对该供应商的当前模型冷却 12 小时。
- 已识别的 relay 工具 schema null-array 拒绝：允许换候选，不自动放宽工具约束。
- 400、409、413、422，以及结构化 `invalid_request`、`invalid_prompt`、`context_length_exceeded` 等请求错误：终止，避免无意义扇出；即使中转把它包装成 5xx，也不轮换供应商。
- HTTP 200 中的错误正文，以及 SSE 实际输出前失败：仍可换源。
- HTTP 200 必须通过实际候选协议的响应结构校验后才记为成功；Chat、Responses、Anthropic 的错误结构，以及已完成 Responses 工具调用中无法转换的参数，不会留到下游转换阶段才暴露。
- 可轮换候选的 401/402/403 按 CPA 的 credential scope 冷却 30 分钟；429 按 `Retry-After` 仅冷却当前模型，同一供应商的其他模型仍可调度。UI 上的供应商冷却时间显示其所有模型冷却的最大剩余值。
- 候选能力拒绝不生成自定义请求指纹缓存。CPA 没有这一层；每次请求只依赖显式模型/凭据冷却、熔断和当前候选排除。
- 单候选同样使用熔断器，持续故障期间不会无限请求上游；Round Robin 按上次候选 ID 推进，配置热更新删改候选后不会因旧数组下标跳号。
- Round Robin、Weighted Round Robin 和 Least Outstanding 在应用策略前过滤熔断/冷却候选。Weighted Round Robin 保留临时排除候选的累计 credit，只在权重变化时重置，并按 CPA 使用 1024 项状态上限。
- SSE 内容已经提交：不重放请求，保留失败状态。

## 验证

`gateway::protocol_tests` 通过真实 loopback HTTP 运行完整入口、路由、转发和 mock 上游。保留三协议、JSON/SSE、工具调用/结果矩阵，新增整份 Chat 请求保真、混合协议换源、推理 profile、schema 兼容、未知别名、语义失败、Anthropic 早期流错误和畸形 2xx 响应回归。

这些测试验证网关契约，不代表已经向黑与白或 AgentRouter 发出真实付费请求。真实部署需使用本次构建，旧进程不会因源码修改自动升级。
