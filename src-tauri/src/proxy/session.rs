//! Proxy Session - 请求会话管理
//!
//! 为每个代理请求创建会话上下文，在整个请求生命周期中跟踪状态和元数据。
//!
//! ## Session ID 提取
//!
//! 支持从客户端请求中提取 Session ID，用于关联同一对话的多个请求：
//! - Claude: 从 `metadata.user_id` (格式: `user_xxx_session_yyy`) 或 `metadata.session_id` 提取
//! - Codex: 从 headers 中的 `session_id` / `x-session-id` 或 `metadata.session_id` 提取
//! - 内容哈希: 客户端未提供任何标识时，用「系统提示 + 首条用户消息 + 首条助手消息」
//!   派生稳定键（见 [`SessionIdResult::is_stable`]）
//! - 其他: 生成新的 UUID（一次性，不可用于路由粘性）

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use std::time::Instant;
use uuid::Uuid;

/// 客户端请求格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ClientFormat {
    /// Claude Messages API (/v1/messages)
    Claude,
    /// Codex Response API (/v1/responses)
    Codex,
    /// OpenAI Chat Completions API (/v1/chat/completions)
    OpenAI,
    /// Gemini API (/v1beta/models/*/generateContent)
    Gemini,
    /// Gemini CLI API (/v1internal/models/*/generateContent)
    GeminiCli,
    /// 未知格式
    Unknown,
}

#[allow(dead_code)]
impl ClientFormat {
    /// 从请求路径检测格式
    pub fn from_path(path: &str) -> Self {
        if path.contains("/v1/messages") {
            ClientFormat::Claude
        } else if path.contains("/v1/responses") {
            ClientFormat::Codex
        } else if path.contains("/v1/chat/completions") {
            ClientFormat::OpenAI
        } else if path.contains("/v1internal/") && path.contains("generateContent") {
            // Gemini CLI 使用 /v1internal/ 路径
            ClientFormat::GeminiCli
        } else if (path.contains("/v1beta/") || path.contains("/v1/"))
            && path.contains("generateContent")
        {
            // Gemini API 使用 /v1beta/ 或 /v1/ 路径
            ClientFormat::Gemini
        } else if path.contains("generateContent") {
            // 通用 Gemini 端点
            ClientFormat::Gemini
        } else {
            ClientFormat::Unknown
        }
    }

    /// 从请求体内容检测格式（回退方案）
    pub fn from_body(body: &serde_json::Value) -> Self {
        // Claude 格式特征: messages 数组 + model 字段 + 无 response_format
        if body.get("messages").is_some()
            && body.get("model").is_some()
            && body.get("response_format").is_none()
            && body.get("contents").is_none()
        {
            // 区分 Claude 和 OpenAI
            if body.get("max_tokens").is_some() {
                return ClientFormat::Claude;
            }
            return ClientFormat::OpenAI;
        }

        // Codex 格式特征: input 字段
        if body.get("input").is_some() {
            return ClientFormat::Codex;
        }

        // Gemini 格式特征: contents 数组
        if body.get("contents").is_some() {
            return ClientFormat::Gemini;
        }

        ClientFormat::Unknown
    }

    /// 转换为字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            ClientFormat::Claude => "claude",
            ClientFormat::Codex => "codex",
            ClientFormat::OpenAI => "openai",
            ClientFormat::Gemini => "gemini",
            ClientFormat::GeminiCli => "gemini_cli",
            ClientFormat::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ClientFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 代理会话
///
/// 包含请求全生命周期的上下文数据
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProxySession {
    /// 唯一会话 ID
    pub session_id: String,
    /// 请求开始时间
    pub start_time: Instant,
    /// HTTP 方法
    pub method: String,
    /// 请求 URL
    pub request_url: String,
    /// User-Agent
    pub user_agent: Option<String>,
    /// 客户端请求格式
    pub client_format: ClientFormat,
    /// 选定的供应商 ID
    pub provider_id: Option<String>,
    /// 模型名称
    pub model: Option<String>,
    /// 是否为流式请求
    pub is_streaming: bool,
}

#[allow(dead_code)]
impl ProxySession {
    /// 从请求创建会话
    pub fn from_request(
        method: &str,
        request_url: &str,
        user_agent: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Self {
        // 检测客户端格式
        let mut client_format = ClientFormat::from_path(request_url);
        if client_format == ClientFormat::Unknown {
            if let Some(body) = body {
                client_format = ClientFormat::from_body(body);
            }
        }

        // 检测是否为流式请求
        let is_streaming = body
            .and_then(|b| b.get("stream"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 提取模型名称
        let model = body
            .and_then(|b| b.get("model"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Self {
            session_id: Uuid::new_v4().to_string(),
            start_time: Instant::now(),
            method: method.to_string(),
            request_url: request_url.to_string(),
            user_agent: user_agent.map(|s| s.to_string()),
            client_format,
            provider_id: None,
            model,
            is_streaming,
        }
    }

    /// 设置供应商 ID
    pub fn with_provider(mut self, provider_id: &str) -> Self {
        self.provider_id = Some(provider_id.to_string());
        self
    }

    /// 获取请求延迟（毫秒）
    pub fn latency_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }
}

// ============================================================================
// Session ID 提取器
// ============================================================================

/// Session ID 来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIdSource {
    /// 从 metadata.user_id 提取 (Claude)
    MetadataUserId,
    /// 从 metadata.session_id 提取
    MetadataSessionId,
    /// 从 headers 提取 (Codex)
    Header,
    /// 由请求内容派生（系统提示 + 首条用户消息 + 首条助手消息）
    ContentHash,
    /// 新生成
    Generated,
}

/// Session ID 提取结果
#[derive(Debug, Clone)]
pub struct SessionIdResult {
    /// 提取或生成的 Session ID
    pub session_id: String,
    /// Session ID 来源
    pub source: SessionIdSource,
    /// 是否为客户端提供的 ID（非新生成、非内容派生）
    pub client_provided: bool,
}

impl SessionIdResult {
    /// 该 Session ID 是否为「稳定键」——即同一段对话的后续请求会重现同一个值
    ///
    /// - 客户端显式提供（Claude / Codex 的元数据或头部）→ 稳定
    /// - 由内容哈希派生（[`SessionIdSource::ContentHash`]）→ 稳定：同一段对话的后续轮次
    ///   会得到同一个键。注意它**不是**客户端提供的，只是可复现
    /// - 新生成的随机 UUID（[`SessionIdSource::Generated`]）→ 不稳定，只能用于单次请求的
    ///   日志关联，**不可**用于路由粘性
    pub fn is_stable(&self) -> bool {
        !matches!(self.source, SessionIdSource::Generated)
    }
}

/// 从请求中提取或生成 Session ID
///
/// ## 提取优先级
///
/// ### Claude 请求
/// 1. Headers: `x-claude-code-session-id` / `claude-code-session-id`
/// 2. `metadata.user_id` (格式: `user_xxx_session_yyy`) → 提取 `yyy` 部分
/// 3. `metadata.session_id` → 直接使用
/// 4. 内容哈希（见下）
/// 5. 生成新 UUID
///
/// ### Codex / OpenAI 请求
/// 1. Headers: `session_id` 或 `x-session-id`
/// 2. `metadata.session_id`
/// 3. `metadata.user_id` / `metadata.session_id`（通用提取）
/// 4. 内容哈希（见下）
/// 5. 生成新 UUID
///
/// ## 内容哈希兜底
///
/// 客户端一个标识都不发时（例如直接 curl，或未携带会话头的第三方客户端），
/// 用「系统提示 + 首条用户消息 + 首条助手消息」派生一个稳定键
/// （[`compute_content_session_hash`]）。这样同一段对话的后续轮次会落到同一个键上，
/// 而不是每轮换一个随机 UUID —— 路由粘性与前缀缓存都依赖这一点。
///
/// 内容里连首条用户消息都找不到时（空 body / 只有 system），退回生成随机 UUID，
/// 其结果 [`SessionIdResult::is_stable`] 为 `false`，调用方据此跳过粘性路由。
///
/// ## 示例
///
/// ```ignore
/// let result = extract_session_id(&headers, &body, "claude");
/// if result.is_stable() {
///     // 可以按 result.session_id 做会话粘性
/// }
/// ```
pub fn extract_session_id(
    headers: &HeaderMap,
    body: &serde_json::Value,
    client_format: &str,
) -> SessionIdResult {
    if client_format == "claude" {
        if let Some(result) = extract_claude_session(headers, body) {
            return result;
        }
    }

    // Codex 请求特殊处理
    if client_format == "codex" || client_format == "openai" {
        if let Some(result) = extract_codex_session(headers, body) {
            return result;
        }
    }

    // Claude 请求：从 metadata 提取
    if let Some(result) = extract_from_metadata(body) {
        return result;
    }

    // 兜底 1：由内容派生稳定键（同一段对话的后续轮次可复现）
    if let Some(result) = extract_from_content(body) {
        return result;
    }

    // 兜底 2：生成新 Session ID（一次性，不可用于粘性）
    generate_new_session_id()
}

/// 提取 Claude Session ID
fn extract_claude_session(
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<SessionIdResult> {
    for header_name in &["x-claude-code-session-id", "claude-code-session-id"] {
        if let Some(value) = headers.get(*header_name) {
            if let Ok(session_id) = value.to_str() {
                if !session_id.is_empty() {
                    return Some(SessionIdResult {
                        session_id: session_id.to_string(),
                        source: SessionIdSource::Header,
                        client_provided: true,
                    });
                }
            }
        }
    }

    extract_from_metadata(body)
}

/// 提取 Codex Session ID
fn extract_codex_session(headers: &HeaderMap, body: &serde_json::Value) -> Option<SessionIdResult> {
    // 1. 从 headers 提取
    for header_name in &["session_id", "x-session-id"] {
        if let Some(value) = headers.get(*header_name) {
            if let Ok(session_id) = value.to_str() {
                // Codex Session ID 通常较长（UUID 格式）
                if session_id.len() > 20 {
                    return Some(SessionIdResult {
                        session_id: format!("codex_{session_id}"),
                        source: SessionIdSource::Header,
                        client_provided: true,
                    });
                }
            }
        }
    }

    // 2. 从 body.metadata.session_id 提取
    if let Some(session_id) = body
        .get("metadata")
        .and_then(|m| m.get("session_id"))
        .and_then(|v| v.as_str())
    {
        if session_id.len() > 10 {
            return Some(SessionIdResult {
                session_id: format!("codex_{session_id}"),
                source: SessionIdSource::MetadataSessionId,
                client_provided: true,
            });
        }
    }

    // previous_response_id 是 Responses 协议里的响应游标，不是稳定会话身份。
    // Chat/Responses 桥接时该值通常来自上游每轮返回的随机 response id；
    // 若把它当 prompt_cache_key 或 Codex session header，会导致每轮请求换缓存 key。

    None
}

/// 从 metadata 提取 Session ID (Claude)
fn extract_from_metadata(body: &serde_json::Value) -> Option<SessionIdResult> {
    let metadata = body.get("metadata")?;

    // 1. 从 metadata.user_id 提取（格式: user_xxx_session_yyy）
    if let Some(user_id) = metadata.get("user_id").and_then(|v| v.as_str()) {
        if let Some(session_id) = parse_session_from_user_id(user_id) {
            return Some(SessionIdResult {
                session_id,
                source: SessionIdSource::MetadataUserId,
                client_provided: true,
            });
        }
    }

    // 2. 直接从 metadata.session_id 提取
    if let Some(session_id) = metadata.get("session_id").and_then(|v| v.as_str()) {
        if !session_id.is_empty() {
            return Some(SessionIdResult {
                session_id: session_id.to_string(),
                source: SessionIdSource::MetadataSessionId,
                client_provided: true,
            });
        }
    }

    None
}

/// 从 user_id 解析 session_id
///
/// 格式: `user_identifier_session_actual_session_id`
pub(super) fn parse_session_from_user_id(user_id: &str) -> Option<String> {
    // 查找 "_session_" 分隔符
    if let Some(pos) = user_id.find("_session_") {
        let session_id = &user_id[pos + 9..]; // "_session_" 长度为 9
        if !session_id.is_empty() {
            return Some(session_id.to_string());
        }
    }
    None
}

// ============================================================================
// 内容哈希兜底
// ============================================================================

/// 由请求内容派生 Session ID
fn extract_from_content(body: &serde_json::Value) -> Option<SessionIdResult> {
    compute_content_session_hash(body).map(|session_id| SessionIdResult {
        session_id,
        source: SessionIdSource::ContentHash,
        client_provided: false,
    })
}

/// 由「系统提示 + 首条用户消息 + 首条助手消息」派生稳定会话键
///
/// 客户端不提供任何会话标识时的兜底。只取对话的**开头**三要素，因此：
/// - 同一段对话的后续轮次（历史在增长）会得到同一个键
/// - 不同对话即使前几轮相似，只要首条用户消息不同就会分开
///
/// 返回 `content_` 前缀 + SHA-256 前 32 位十六进制，便于在日志里一眼认出这是派生键。
/// 找不到首条用户消息时返回 `None` —— 没有锚点就不是可识别的对话，调用方应退回随机 UUID。
pub(super) fn compute_content_session_hash(body: &serde_json::Value) -> Option<String> {
    // 首条用户消息是锚点：没有它就无法把「同一段对话」区分出来
    let user_text = first_role_text(body, "user")?;
    let system_text = extract_system_text(body).unwrap_or_default();
    let assistant_text = first_role_text(body, "assistant").unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(system_text.as_bytes());
    // 0x1f = ASCII Unit Separator，避免「a|b」与「a」「|b」拼接歧义
    hasher.update(b"\x1f");
    hasher.update(user_text.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(assistant_text.as_bytes());

    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();

    Some(format!("content_{}", &hex[..32]))
}

/// 取系统提示文本
///
/// 依次尝试：Claude 的 `system` → Responses 的 `instructions` → 消息列表里的 system 角色
fn extract_system_text(body: &serde_json::Value) -> Option<String> {
    if let Some(text) = body.get("system").and_then(value_to_text) {
        return Some(text);
    }

    if let Some(text) = body.get("instructions").and_then(|v| v.as_str()) {
        if let Some(text) = normalized_text(text) {
            return Some(text);
        }
    }

    first_role_text(body, "system")
}

/// 取第一条指定 role 消息的文本
///
/// 同时扫描 `messages`（Claude / Chat Completions）与 `input`（Responses）。
fn first_role_text(body: &serde_json::Value, role: &str) -> Option<String> {
    // Responses 协议的 `input` 允许直接是字符串
    if role == "user" {
        if let Some(text) = body.get("input").and_then(|v| v.as_str()) {
            if let Some(text) = normalized_text(text) {
                return Some(text);
            }
        }
    }

    for container in ["messages", "input"] {
        let items = match body.get(container).and_then(|v| v.as_array()) {
            Some(items) => items,
            None => continue,
        };

        for item in items {
            if let Some(text) = item_role_text(item, role) {
                return Some(text);
            }
        }
    }

    None
}

/// 从单条消息里取出指定 role 的文本；role 不匹配返回 `None`
fn item_role_text(item: &serde_json::Value, role: &str) -> Option<String> {
    match item {
        // `input` 里的裸字符串按用户消息处理
        serde_json::Value::String(text) => {
            if role == "user" {
                return normalized_text(text);
            }
            None
        }
        serde_json::Value::Object(_) => {
            if item.get("role").and_then(|v| v.as_str()) != Some(role) {
                return None;
            }
            item.get("content").and_then(value_to_text)
        }
        _ => None,
    }
}

/// 把 content 字段归一化成纯文本
///
/// 处理两种形态：直接是字符串，或文本块数组。非文本块（图片、tool_use、tool_result）
/// 没有 `text` 字段，会被跳过 —— 它们不该影响会话键的稳定性。
fn value_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => normalized_text(text),
        serde_json::Value::Array(parts) => {
            let mut buffer = String::new();
            for part in parts {
                let piece = match part {
                    serde_json::Value::String(text) => normalized_text(text),
                    serde_json::Value::Object(_) => {
                        part.get("text").and_then(|v| v.as_str()).and_then(normalized_text)
                    }
                    _ => None,
                };

                if let Some(piece) = piece {
                    if !buffer.is_empty() {
                        buffer.push('\n');
                    }
                    buffer.push_str(&piece);
                }
            }

            if buffer.is_empty() {
                None
            } else {
                Some(buffer)
            }
        }
        _ => None,
    }
}

/// 去空白；空串视为「没有内容」
fn normalized_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 生成新的 Session ID
fn generate_new_session_id() -> SessionIdResult {
    SessionIdResult {
        session_id: Uuid::new_v4().to_string(),
        source: SessionIdSource::Generated,
        client_provided: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_client_format_from_path_claude() {
        assert_eq!(
            ClientFormat::from_path("/v1/messages"),
            ClientFormat::Claude
        );
        assert_eq!(
            ClientFormat::from_path("/api/v1/messages"),
            ClientFormat::Claude
        );
    }

    #[test]
    fn test_client_format_from_path_codex() {
        assert_eq!(
            ClientFormat::from_path("/v1/responses"),
            ClientFormat::Codex
        );
    }

    #[test]
    fn test_client_format_from_path_openai() {
        assert_eq!(
            ClientFormat::from_path("/v1/chat/completions"),
            ClientFormat::OpenAI
        );
    }

    #[test]
    fn test_client_format_from_path_gemini() {
        assert_eq!(
            ClientFormat::from_path("/v1beta/models/gemini-pro:generateContent"),
            ClientFormat::Gemini
        );
    }

    #[test]
    fn test_client_format_from_path_gemini_cli() {
        assert_eq!(
            ClientFormat::from_path("/v1internal/models/gemini-pro:generateContent"),
            ClientFormat::GeminiCli
        );
    }

    #[test]
    fn test_client_format_from_body_claude() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 1024
        });
        assert_eq!(ClientFormat::from_body(&body), ClientFormat::Claude);
    }

    #[test]
    fn test_client_format_from_body_codex() {
        let body = json!({
            "input": "Write a function"
        });
        assert_eq!(ClientFormat::from_body(&body), ClientFormat::Codex);
    }

    #[test]
    fn test_client_format_from_body_gemini() {
        let body = json!({
            "contents": [{"parts": [{"text": "Hello"}]}]
        });
        assert_eq!(ClientFormat::from_body(&body), ClientFormat::Gemini);
    }

    #[test]
    fn test_session_id_uniqueness() {
        let session1 = ProxySession::from_request("POST", "/v1/messages", None, None);
        let session2 = ProxySession::from_request("POST", "/v1/messages", None, None);
        assert_ne!(session1.session_id, session2.session_id);
    }

    #[test]
    fn test_session_from_request() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 1024,
            "stream": true
        });

        let session =
            ProxySession::from_request("POST", "/v1/messages", Some("Mozilla/5.0"), Some(&body));

        assert_eq!(session.method, "POST");
        assert_eq!(session.request_url, "/v1/messages");
        assert_eq!(session.user_agent, Some("Mozilla/5.0".to_string()));
        assert_eq!(session.client_format, ClientFormat::Claude);
        assert_eq!(session.model, Some("claude-3-5-sonnet".to_string()));
        assert!(session.is_streaming);
    }

    #[test]
    fn test_session_with_provider() {
        let session = ProxySession::from_request("POST", "/v1/messages", None, None)
            .with_provider("provider-123");

        assert_eq!(session.provider_id, Some("provider-123".to_string()));
    }

    #[test]
    fn test_client_format_as_str() {
        assert_eq!(ClientFormat::Claude.as_str(), "claude");
        assert_eq!(ClientFormat::Codex.as_str(), "codex");
        assert_eq!(ClientFormat::OpenAI.as_str(), "openai");
        assert_eq!(ClientFormat::Gemini.as_str(), "gemini");
        assert_eq!(ClientFormat::GeminiCli.as_str(), "gemini_cli");
        assert_eq!(ClientFormat::Unknown.as_str(), "unknown");
    }

    // ========== Session ID 提取测试 ==========

    #[test]
    fn test_extract_session_from_claude_metadata_user_id() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "user_id": "user_john_doe_session_abc123def456"
            }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "abc123def456");
        assert_eq!(result.source, SessionIdSource::MetadataUserId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_metadata_session_id() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "session_id": "my-session-123"
            }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "my-session-123");
        assert_eq!(result.source, SessionIdSource::MetadataSessionId);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "d937243f-2702-4f20-97b6-c9682235ab81".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "d937243f-2702-4f20-97b6-c9682235ab81");
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_extract_session_from_claude_header_precedes_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "header-session-123".parse().unwrap(),
        );
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{"role": "user", "content": "Hello"}],
            "metadata": {
                "session_id": "my-session-123"
            }
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "header-session-123");
        assert_eq!(result.source, SessionIdSource::Header);
        assert!(result.client_provided);
    }

    #[test]
    fn test_codex_previous_response_id_is_not_stable_session_identity() {
        let headers = HeaderMap::new();
        let body = json!({
            "input": "Write a function",
            "previous_response_id": "resp_abc123def456789"
        });

        let result = extract_session_id(&headers, &body, "codex");

        assert!(!result.session_id.is_empty());
        // previous_response_id 每轮都变，不能当会话身份；这里应该退到内容哈希
        assert_eq!(result.source, SessionIdSource::ContentHash);
        assert!(!result.session_id.contains("resp_abc123def456789"));
        assert!(!result.client_provided);
    }

    #[test]
    fn test_extract_session_generates_new_when_not_found() {
        let headers = HeaderMap::new();
        // 只有 system，没有首条用户消息 —— 没有可识别的对话锚点
        let body = json!({
            "model": "claude-3-5-sonnet",
            "system": "You are helpful."
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert!(!result.session_id.is_empty());
        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.client_provided);
        assert!(!result.is_stable());
    }

    #[test]
    fn test_parse_session_from_user_id() {
        assert_eq!(
            parse_session_from_user_id("user_john_session_abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_session_from_user_id("my_app_session_xyz789"),
            Some("xyz789".to_string())
        );
        // 注意: "_session_" 是分隔符，所以下面的字符串会匹配
        assert_eq!(
            parse_session_from_user_id("no_session_marker"),
            Some("marker".to_string())
        );
        // 没有 "_session_" 分隔符的情况
        assert_eq!(parse_session_from_user_id("user_john_abc123"), None);
        assert_eq!(parse_session_from_user_id("_session_"), None);
    }

    // ========== 内容哈希兜底测试 ==========

    #[test]
    fn test_content_hash_stable_across_turns() {
        let headers = HeaderMap::new();

        let first_turn = json!({
            "model": "claude-3-5-sonnet",
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "Explain Rust ownership."}]
        });

        // 后续轮次：历史变长，但开头三要素没变
        let later_turn = json!({
            "model": "claude-3-5-sonnet",
            "system": "You are a helpful assistant.",
            "messages": [
                {"role": "user", "content": "Explain Rust ownership."},
                {"role": "assistant", "content": "Ownership is ..."},
                {"role": "user", "content": "Give an example."}
            ]
        });

        let first = extract_session_id(&headers, &first_turn, "claude");
        let later = extract_session_id(&headers, &later_turn, "claude");

        assert_eq!(first.source, SessionIdSource::ContentHash);
        assert!(first.is_stable());
        assert!(!first.client_provided);
        assert!(first.session_id.starts_with("content_"));
        assert_eq!(first.session_id, later.session_id);
    }

    #[test]
    fn test_content_hash_differs_for_different_conversations() {
        let headers = HeaderMap::new();
        let system = "You are a helpful assistant.";

        let conversation_a = json!({
            "system": system,
            "messages": [{"role": "user", "content": "Explain Rust ownership."}]
        });
        let conversation_b = json!({
            "system": system,
            "messages": [{"role": "user", "content": "Explain Rust lifetimes."}]
        });

        let a = extract_session_id(&headers, &conversation_a, "claude");
        let b = extract_session_id(&headers, &conversation_b, "claude");

        assert_ne!(a.session_id, b.session_id);
    }

    #[test]
    fn test_content_hash_differs_when_system_prompt_differs() {
        let headers = HeaderMap::new();

        let with_system = json!({
            "system": "Be brief.",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        // 同一 body 必须得到同一个键 —— 哈希是确定性的，不能掺入时间/随机量
        assert_eq!(
            extract_session_id(&headers, &with_system, "claude").session_id,
            extract_session_id(&headers, &with_system, "claude").session_id
        );

        let no_system = json!({
            "messages": [{"role": "user", "content": "Hello"}]
        });
        assert_ne!(
            extract_session_id(&headers, &with_system, "claude").session_id,
            extract_session_id(&headers, &no_system, "claude").session_id
        );
    }

    #[test]
    fn test_content_hash_handles_text_block_arrays() {
        let headers = HeaderMap::new();
        let body = json!({
            "system": [{"type": "text", "text": "You are helpful."}],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "First part."},
                        {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
                        {"type": "text", "text": "Second part."}
                    ]
                }
            ]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.source, SessionIdSource::ContentHash);
        assert!(result.is_stable());
        // 图片块被跳过，文本块按顺序拼接
        let expected = compute_content_session_hash(&json!({
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "First part.\nSecond part."}]
        }))
        .unwrap();
        assert_eq!(result.session_id, expected);
    }

    #[test]
    fn test_content_hash_handles_responses_input_string() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "gpt-5",
            "instructions": "Be concise.",
            "input": "Write a function"
        });

        let result = extract_session_id(&headers, &body, "codex");

        assert_eq!(result.source, SessionIdSource::ContentHash);
        assert!(result.is_stable());
    }

    #[test]
    fn test_content_hash_handles_responses_message_items() {
        let headers = HeaderMap::new();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Write a function"}]
                }
            ]
        });

        let result = extract_session_id(&headers, &body, "codex");

        assert_eq!(result.source, SessionIdSource::ContentHash);
        assert_eq!(
            result.session_id,
            compute_content_session_hash(&json!({
                "messages": [{"role": "user", "content": [{"text": "Write a function"}]}]
            }))
            .unwrap()
        );
    }

    #[test]
    fn test_content_hash_ignores_tool_result_only_history() {
        let headers = HeaderMap::new();
        // 首条是助手消息，没有用户锚点 —— 退回随机 UUID
        let body = json!({
            "messages": [{"role": "assistant", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.source, SessionIdSource::Generated);
        assert!(!result.is_stable());
    }

    #[test]
    fn test_explicit_session_id_beats_content_hash() {
        let headers = HeaderMap::new();
        let body = json!({
            "metadata": {"session_id": "my-session-123"},
            "messages": [{"role": "user", "content": "Hello"}]
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert_eq!(result.session_id, "my-session-123");
        assert_eq!(result.source, SessionIdSource::MetadataSessionId);
    }

    #[test]
    fn test_content_hash_is_insensitive_to_surrounding_whitespace() {
        let headers = HeaderMap::new();
        let padded = json!({
            "messages": [{"role": "user", "content": "  Hello  "}]
        });
        let tight = json!({
            "messages": [{"role": "user", "content": "Hello"}]
        });

        assert_eq!(
            extract_session_id(&headers, &padded, "claude").session_id,
            extract_session_id(&headers, &tight, "claude").session_id
        );
    }

    #[test]
    fn test_client_provided_session_ids_are_stable() {
        let headers = HeaderMap::new();
        let body = json!({
            "metadata": {"session_id": "abc"}
        });

        let result = extract_session_id(&headers, &body, "claude");

        assert!(result.client_provided);
        assert!(result.is_stable());
    }
}
