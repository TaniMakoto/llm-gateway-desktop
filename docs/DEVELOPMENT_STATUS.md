# 开发与验证状态

核对日期：2026-09-20；初始审阅基线：`3cb7a7a`；后续按用户批准实施回归、修复、运行服务分离与首轮裁剪。清单版本仍为 `0.1.0`，按 alpha 阶段看待。

## 当前源码已实现

- Tauri 桌面、托盘、开机启动、轻量模式、Windows portable 数据隔离。
- 三协议入口与转换：Chat Completions、Responses、Anthropic Messages；另有 Responses compact、模型列表及运行状态入口。
- 供应商内模型条目、每模型协议、别名聚合、旧路由配置迁移。
- 优先顺序、轮询、加权轮询、最少未完成请求四种路由策略。
- 会话亲和、故障转移、熔断、429 冷却、供应商并发上限与有界排队。
- 模型发现与缓存、集中模型能力注册表、手动能力覆盖、reasoning 兼容选项。
- 直连/经网关测试台、流式正文与思考显示、usage、请求/响应正文录制。
- 独立 `GatewayRuntime`，桌面启动/停止/状态不再使用 CLI 接管服务；统一网关路由不触发外部 CLI 配置切换。
- 首轮裁剪约 6,700 行无入口命令和 CLI 会话历史浏览代码；共享服务与数据库迁移仍保留。
- Mock 上游、真实 HTTP 三协议回归、内联 Rust 测试，以及多平台 CI 打包工作流。

上述是源码能力清单，不是所有组合已在真实供应商上通过的保证。

## 本次实际验证

- `python tools/static_check.py`：通过，包括所选 Rust 文件结构、公开路由、清单/Python 解析及 TS 语法检查。
- `node node_modules/typescript/bin/tsc --noEmit`：通过完整 TypeScript 类型检查。
- `pnpm typecheck`：包装器触发依赖安装检查后因非交互式目录清理确认失败，未完成该命令；随后直接调用同一项目 TypeScript 编译器验证通过。没有为此清理或重装依赖。
- 本机未安装 Cargo；Rust 编译和全量测试通过推送 GitHub Actions 执行。使用官方发行包临时提取的 rustfmt 格式化新增/修改的回归和运行服务文件，没有安装系统 Rust 工具链。
- GitHub Actions 已通过运行服务分离和首轮裁剪后的全量 Rust 测试及 54 组合真实 HTTP 回归：[裁剪后验证](https://github.com/TaniMakoto/llm-gateway-desktop/actions/runs/35498053687)。
- 未使用真实上游凭证；mock 端到端通过不代表所有真实供应商已验收。

## 回归与修复内容

- 54 个组合：三下游 × 三上游 × JSON/SSE × 文本/工具调用/工具结果。检查模型映射、系统提示、输出预算、工具定义/参数/关联、usage 和流终止；上游按小块发送字节以覆盖跨分片处理。
- 工具结果回放使用网关实际返回的 ID；Anthropic usage 同时读取 message_start 和 message_delta，避免把合法协议差异当作丢失。
- 增加三下游 JSON/SSE 的错误密钥、429 后故障转移和冷却期间不重复访问故障上游的真实 HTTP 测试。
- 修复 Chat SSE 桥的中文跨分片乱码，以及提前 EOF/失败事件被补成正常 stop 的问题。
- 运行服务回归覆盖并发启动、重复停止、端口占用时保留原服务与配置，并检查不会开启 CLI 接管。

## 后续边界

- reasoning、多模态、供应商专属字段、Responses compact 尚未全部纳入这次 54 组合矩阵。
- 统一网关配置存在时，未知别名已明确拒绝；只有未配置统一网关的旧应用路径保留继承 router。
- 真实客户端/上游验收、更多排队取消压力场景仍需补充。
- 首轮仅删除依赖关系明确的模块；剩余继承服务和 schema 不应未经迁移直接删除。

## 历史结果的解释

旧报告记载的 mock 8/8 是历史工具自测，复现命令直接访问 mock 端口；不能据此宣称当前 Rust 网关端到端通过。历史记录保留在 [VALIDATION_REPORT.md](../VALIDATION_REPORT.md)，本页记录本次审阅的验证范围。
