# 架构说明

## 目标

LLM Gateway Desktop 将多个第三方 LLM API 统一暴露为一个本地端口，并将“客户端协议”和“上游协议”解耦。

```text
客户端
  ├─ OpenAI Chat
  ├─ OpenAI Responses
  └─ Anthropic Messages
        │
        ▼
127.0.0.1:10888
  ├─ 本地鉴权
  ├─ 模型别名解析
  ├─ 有序路由链
  ├─ 精确模型映射
  ├─ 协议转换
  ├─ SSE 转换
  └─ 熔断与故障转移
        │
        ▼
第三方 API A / B / C
```

## 核心模块

### `gateway.rs`

保存一份与原 LLM Gateway Desktop 应用切换逻辑隔离的统一网关配置：

- `GatewayProvider`
- `GatewayRoute`
- `GatewayRouteTarget`
- 本地监听和鉴权配置

配置存储在 SQLite `settings` 表的 `unified_gateway_config_v1` 键中。

为了复用原代理内核，每个通用供应商会被物化为内部 `claude` 和 `codex` provider。它们只属于 `unified_gateway` 分类，不会写入 Claude Code、Codex CLI 或 Gemini 的 live 配置。

### `gateway_chat.rs`

OpenAI Chat 请求先转换成 Responses 请求，再进入成熟的 Responses 路由链。这样 Chat 客户端也能路由到：

- Chat 上游
- Responses 上游
- Anthropic 上游

非流式 Responses 响应转换回 Chat Completion；流式 Responses SSE 事件转换为 Chat Completion chunks。

### `proxy/`

保留的关键代理基础设施：

- HTTP 转发
- SSE 解码和重建
- Anthropic ↔ OpenAI Chat
- Anthropic ↔ OpenAI Responses
- Responses ↔ OpenAI Chat
- 工具调用转换
- 模型映射
- 请求日志
- 超时、熔断与重试

### 数据隔离

默认使用 `~/.llm-gateway-desktop/llm-gateway.db`。专用启动流程不会扫描或接管本机 Claude、Codex、Gemini、OpenCode、Hermes、MCP、Skills 或提示词配置。

## 路由选择

客户端请求中的 `model` 是本地模型别名。`RequestContext` 在正常 provider router 之前检查统一网关路由：

1. 查找启用的同名 route。
2. 按 route targets 顺序取得启用的 provider。
3. 交给既有 forwarder 执行故障转移。
4. 每个 provider 的 `gateway_model_map` 将本地别名替换为对应真实模型名。

## 安全默认值

- 默认仅绑定 `127.0.0.1`
- 默认启用本地 API Key
- API Key 不返回给下游客户端
- 默认不保存额外提示词正文；日志行为沿用代理内核配置
- 允许手动绑定 `0.0.0.0`，但应配合防火墙并保持本地鉴权开启

## 暂未独立实现的功能

- 单个 provider 内的多 Key 轮换；目前可通过添加多个 provider 条目实现
- 按价格或实时延迟自动选择
- 配额感知路由
- 完全无损转换厂商专属字段
- 独立于 LLM Gateway Desktop 内核的最小 Rust crate 拆分
