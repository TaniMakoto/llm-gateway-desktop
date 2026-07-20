# 开发与验证状态

版本：`0.1.0-alpha`

## 已实现

- 精简桌面 UI：仪表盘、上游供应商、模型路由、网关设置
- 独立数据目录和 SQLite 配置
- 本地访问密钥
- 供应商 URL、鉴权方式和自定义请求头
- 模型别名、上游模型映射和有序故障转移
- OpenAI Chat、OpenAI Responses、Anthropic Messages 下游入口
- 普通响应、SSE 和标准工具调用转换路径
- `/v1/models` 与 `/health`
- 专用启动、停止、托盘菜单和自动启动
- Mock 上游与冒烟测试工具
- Windows、Linux 和 macOS GitHub Actions 打包流程

## 当前环境已完成

- JSON、TOML 和 Python 源码解析
- 修改过的 Rust 文件分隔符静态检查
- 路由存在性检查
- Mock 冒烟测试 8/8：健康检查、模型列表、三种 JSON 响应和三种 SSE 流
- 品牌字符串与公开发布文件检查

## 尚未完成

当前打包环境未安装 Rust 工具链和 pnpm 项目依赖，因此尚未完成：

- `cargo check` / `cargo test` / `cargo fmt`
- 完整 `pnpm typecheck`
- 四个平台的真实 Tauri 打包
- 真实付费 API 端到端兼容性测试
- 独立安全审计

首次公开发布应标记为 alpha，并在 GitHub Actions 通过后再上传 Release。

## 建议验收顺序

1. GitHub Actions 四个平台构建通过。
2. 使用 `tools/mock_upstream.py` 验证普通响应与 SSE。
3. 验证标准 function tool call。
4. 各接入一个真实 Chat、Responses 和 Anthropic 上游。
5. 验证第一目标失败后切换到第二目标。
6. 接入实际客户端进行回归测试。
