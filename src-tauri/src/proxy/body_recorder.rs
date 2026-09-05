//! 请求体录制（统一网关诊断功能）
//!
//! 按供应商/模型粒度开启后，把实际发往上游的最终请求体与上游最终响应体
//! 以 JSONL 行的形式追加写入 `<日志目录>/request-bodies/`，用于排查
//! 「中转站统计的输入 token 与客户端统计不一致」这类问题——通过对比
//! 实际传输内容定位差异来源（历史注入、thinking 块、system 改写等）。
//!
//! 设计要点：
//! - 每个供应商每天一个文件：`<供应商名>-<YYYYMMDD>.jsonl`，请求与响应
//!   交错写入，靠同一 `trace_id` 关联一次请求的两条记录。
//! - 写入通过 `tokio::spawn` 异步执行，绝不阻塞转发热路径；任何 IO 失败
//!   只记 warn 日志，不影响请求本身。
//! - 响应体录制采用「边透传边收集」的 tee 模式：截断阈值（16 MiB）防止
//!   异常大响应撑爆磁盘；SSE 流收集原始字节，按需解压后落盘。

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 单个 body 的截断阈值（16 MiB）。超出部分截断并打 `truncated` 标记。
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// 进程内单调递增的 trace 序号，用于关联同一次请求的 request/response 记录。
static TRACE_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn next_trace_id() -> String {
    let n = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    // 时间戳 + 序号：重启后序号重复也无妨，时间戳保证唯一性足够诊断使用
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("t{ts:x}-{n}")
}

/// 流式路径专用：tee 流在 tokio::spawn 的异步上下文里收尾，此时响应已透传
/// 完毕，直接复用 `record_response` 的 JSONL 写入逻辑（只绕开再拷贝一次）。
pub fn record_response_from_async(
    provider_name: &str,
    provider_id: &str,
    trace_id: &str,
    outbound_model: &str,
    status: u16,
    content_type: Option<&str>,
    body_bytes: Vec<u8>,
) {
    let (bytes, truncated) = truncate_bytes(&body_bytes);
    let body_str = String::from_utf8(bytes).unwrap_or_default();
    let body_value = serde_json::from_str::<Value>(&body_str).unwrap_or(Value::Null);
    write_record(
        provider_name,
        json!({
            "trace_id": trace_id,
            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "direction": "response",
            "provider_id": provider_id,
            "provider_name": provider_name,
            "outbound_model": outbound_model,
            "status": status,
            "content_type": content_type,
            "truncated": truncated,
            "body": if body_value.is_null() { Value::String(body_str) } else { body_value },
        }),
    );
}

/// 判断该次转发是否命中录制范围；命中时生成 trace id 用于配对，未命中返回 None。
///
/// 录制开关由统一网关物化到 `ProviderMeta.body_recording_models`：
/// - `None`：未开启
/// - `Some(空列表)`：全量录制（该协议下所有模型）
/// - `Some(模型列表)`：仅录制出站模型名命中列表的请求
pub fn request_trace_id(
    provider: &crate::provider::Provider,
    outbound_model: Option<&str>,
) -> Option<String> {
    let list = provider
        .meta
        .as_ref()?
        .body_recording_models
        .as_ref()?;
    match outbound_model {
        Some(model) if !list.is_empty() && !list.iter().any(|m| m == model) => None,
        _ => Some(next_trace_id()),
    }
}

/// 供应商名 → 安全文件名片段：保留字母数字与常见分隔符，其余替换为 `_`。
fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim().trim_matches('.');
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 解析录制目录：`<日志目录>/request-bodies`。日志目录取
/// `panic_hook::get_log_dir()`（已处理便携模式），失败时返回 None。
fn recording_dir() -> Option<PathBuf> {
    Some(crate::panic_hook::get_log_dir().join("request-bodies"))
}

/// 截断字节体到阈值内；返回 (字节, 是否截断)。
fn truncate_bytes(raw: &[u8]) -> (Vec<u8>, bool) {
    if raw.len() <= MAX_BODY_BYTES {
        (raw.to_vec(), false)
    } else {
        (raw[..MAX_BODY_BYTES].to_vec(), true)
    }
}


/// 组装一条 JSONL 记录并异步落盘。
///
/// `body_json` 为已解析的 JSON（解析失败则退化为纯文本字段）。
fn write_record(provider: &str, record: Value) {
    let dir = match recording_dir() {
        Some(dir) => dir,
        None => return,
    };
    let provider_slug = sanitize_component(provider);
    let date = chrono::Local::now().format("%Y%m%d");
    let path = dir.join(format!("{provider_slug}-{date}.jsonl"));
    let mut line = record.to_string();
    line.push('\n');

    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(line.as_bytes())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("[BodyRecorder] 写入请求体录制失败: {e}"),
            Err(e) => log::warn!("[BodyRecorder] 写入任务异常: {e}"),
        }
    });
}

/// 记录发往上游的最终请求体。
///
/// `filtered_body` 是 `prepare_upstream_request_body` 之后、即将发出的真值
/// （已完成协议转换、模型映射、私有字段过滤）。
pub fn record_request(
    provider: &crate::provider::Provider,
    trace_id: &str,
    endpoint: &str,
    outbound_model: &str,
    request_model: &str,
    body: &Value,
    is_streaming: bool,
) {
    write_record(
        &provider.name,
        json!({
            "trace_id": trace_id,
            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "direction": "request",
            "provider_id": provider.id,
            "provider_name": provider.name,
            "endpoint": endpoint,
            "outbound_model": outbound_model,
            "request_model": request_model,
            "is_streaming": is_streaming,
            "body": body,
        }),
    );
}

/// 记录上游最终响应体（已解压的完整字节或 SSE 原始流聚合）。
pub fn record_response(
    provider: &crate::provider::Provider,
    trace_id: &str,
    outbound_model: &str,
    status: u16,
    content_type: Option<&str>,
    body_bytes: Vec<u8>,
) {
    let (bytes, truncated) = truncate_bytes(&body_bytes);
    let body_str = String::from_utf8(bytes).unwrap_or_default();
    // 响应体绝大多数是 JSON / SSE 文本；解析成功就存结构化 JSON，便于逐字段比对
    let body_value = serde_json::from_str::<Value>(&body_str).unwrap_or(Value::Null);
    write_record(
        &provider.name,
        json!({
            "trace_id": trace_id,
            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "direction": "response",
            "provider_id": provider.id,
            "provider_name": provider.name,
            "outbound_model": outbound_model,
            "status": status,
            "content_type": content_type,
            "truncated": truncated,
            "body": if body_value.is_null() { Value::String(body_str) } else { body_value },
        }),
    );
}

/// 上游请求失败（非 2xx）时记录错误体，方便与录制到的请求配对分析。
pub fn record_upstream_error(
    provider: &crate::provider::Provider,
    trace_id: &str,
    outbound_model: &str,
    status: u16,
    body_text: Option<&str>,
) {
    write_record(
        &provider.name,
        json!({
            "trace_id": trace_id,
            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "direction": "error",
            "provider_id": provider.id,
            "provider_name": provider.name,
            "outbound_model": outbound_model,
            "status": status,
            "body": body_text,
        }),
    );
}

/// 把上游响应流变成「tee 流」：字节原样透传给客户端的同时，收集一份副本
/// （最多 MAX_BODY_BYTES）供响应结束时落盘。
///
/// 透传语义与 `create_logged_passthrough_stream` 的无超时模式一致：不解析
/// SSE、不改写任何字节，仅旁路收集。
pub fn tee_response_stream(
    stream: impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    on_finish: impl FnOnce(Vec<u8>) + Send + 'static,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut collected: Vec<u8> = Vec::new();
        let mut overflow = false;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    if collected.len() < MAX_BODY_BYTES {
                        let remaining = MAX_BODY_BYTES - collected.len();
                        let take = bytes.len().min(remaining);
                        collected.extend_from_slice(&bytes[..take]);
                        if take < bytes.len() {
                            overflow = true;
                        }
                    } else {
                        overflow = true;
                    }
                    yield Ok(bytes);
                }
                Err(e) => {
                    yield Err(e);
                    // 流出错：仍然落盘已收集部分（错误现场同样有诊断价值）
                    on_finish(std::mem::take(&mut collected));
                    return;
                }
            }
        }
        let _ = overflow;
        on_finish(collected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderMeta};

    fn provider_with_recording(list: Option<Vec<String>>) -> Provider {
        let mut meta = ProviderMeta::default();
        meta.body_recording_models = list;
        Provider {
            id: "p1".into(),
            name: "Test Relay".into(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(meta),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    fn sanitize_component_replaces_unsafe_chars() {
        assert_eq!(sanitize_component("My Relay 站"), "My Relay _");
        assert_eq!(sanitize_component("a/b\\c:d*e"), "a_b_c_d_e");
        assert_eq!(sanitize_component("..hidden.."), "hidden");
        assert_eq!(sanitize_component("   "), "_");
    }

    #[test]
    fn truncate_bytes_caps_at_limit() {
        let small = vec![b'a'; 100];
        let (out, truncated) = truncate_bytes(&small);
        assert_eq!(out.len(), 100);
        assert!(!truncated);

        let huge = vec![b'b'; MAX_BODY_BYTES + 1];
        let (out, truncated) = truncate_bytes(&huge);
        assert_eq!(out.len(), MAX_BODY_BYTES);
        assert!(truncated);
    }

    #[test]
    fn next_trace_id_is_unique() {
        let a = next_trace_id();
        let b = next_trace_id();
        assert_ne!(a, b);
    }

    #[test]
    fn request_trace_id_respects_scope() {
        // 未开启录制：meta 为 None 或 body_recording_models 为 None
        let off = provider_with_recording(None);
        assert!(request_trace_id(&off, Some("gpt-5")).is_none());

        let no_meta = Provider {
            id: "p2".into(),
            name: "No Meta".into(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: None,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };
        assert!(request_trace_id(&no_meta, Some("gpt-5")).is_none());

        // 全量录制：空列表命中所有模型，包括出站模型未知
        let all = provider_with_recording(Some(vec![]));
        assert!(request_trace_id(&all, Some("gpt-5")).is_some());
        assert!(request_trace_id(&all, None).is_some());

        // 按模型录制：命中出站模型才录制
        let scoped = provider_with_recording(Some(vec!["gpt-5".to_string()]));
        assert!(request_trace_id(&scoped, Some("gpt-5")).is_some());
        assert!(request_trace_id(&scoped, Some("claude-x")).is_none());
        assert!(request_trace_id(&scoped, None).is_none());
    }
}
