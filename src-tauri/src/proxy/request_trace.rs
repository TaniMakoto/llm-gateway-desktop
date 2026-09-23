//! Per-request routing evidence. Only controls and diagnostics are persisted, never bodies.
use super::{
    hyper_client::ProxyResponse,
    sse::{append_utf8_safe, strip_sse_field, take_sse_block},
};
use crate::database::Database;
use crate::error::AppError;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub provider_id: String,
    pub provider_name: String,
    pub outcome: String,
    pub status_code: Option<u16>,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub skip_reason: Option<String>,
    pub sent_reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_note: Option<String>,
    pub returned_reasoning: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub client_reasoning: Option<String>,
    pub sent_reasoning: Option<String>,
    pub returned_reasoning: Option<String>,
    pub returned_reasoning_note: Option<String>,
    pub outcome: String,
    pub fallback: bool,
    pub attempts: Vec<Attempt>,
}

#[derive(Clone)]
pub struct RequestTrace(Arc<Inner>);
struct Inner {
    id: String,
    db: Option<Arc<Database>>,
    app_type: String,
    model: String,
    streaming: std::sync::atomic::AtomicBool,
    started: Instant,
    created_at: i64,
    evidence: Mutex<Evidence>,
    http_status: Mutex<Option<u16>>,
    active_started: Mutex<Instant>,
    runtime_status: Mutex<Option<Arc<tokio::sync::RwLock<super::types::ProxyStatus>>>>,
}

impl Default for RequestTrace {
    fn default() -> Self {
        Self::new(None, "", &serde_json::json!({}))
    }
}

/// The value actually present in this body; no model defaults or inferred effort.
pub fn reasoning(body: &Value) -> Option<String> {
    for path in [
        "/reasoning_effort",
        "/reasoning/effort",
        "/output_config/effort",
        "/generationConfig/thinkingConfig/thinkingLevel",
        "/generation_config/thinking_config/thinking_level",
    ] {
        if let Some(v) = body
            .pointer(path)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
        {
            return Some(v.to_string());
        }
    }
    if let Some(kind) = body.pointer("/thinking/type").and_then(Value::as_str) {
        return Some(
            match body
                .pointer("/thinking/budget_tokens")
                .and_then(Value::as_u64)
            {
                Some(budget) => format!("{kind} · budget={budget}"),
                None => kind.to_string(),
            },
        );
    }
    for path in [
        "/generationConfig/thinkingConfig/thinkingBudget",
        "/generation_config/thinking_config/thinking_budget",
    ] {
        if let Some(v) = body.pointer(path).and_then(Value::as_i64) {
            return Some(format!("budget={v}"));
        }
    }
    body.get("enable_thinking")
        .and_then(Value::as_bool)
        .map(|v| if v { "enabled" } else { "disabled" }.to_string())
}

impl RequestTrace {
    pub fn new(db: Option<Arc<Database>>, app_type: &str, body: &Value) -> Self {
        Self(Arc::new(Inner {
            id: uuid::Uuid::new_v4().to_string(),
            db,
            app_type: app_type.into(),
            model: body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            streaming: std::sync::atomic::AtomicBool::new(
                body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            ),
            started: Instant::now(),
            created_at: chrono::Utc::now().timestamp(),
            http_status: Mutex::new(None),
            active_started: Mutex::new(Instant::now()),
            runtime_status: Mutex::new(None),
            evidence: Mutex::new(Evidence {
                client_reasoning: reasoning(body),
                outcome: "failed".into(),
                ..Default::default()
            }),
        }))
    }
    pub fn set_runtime_status(&self, status: Arc<tokio::sync::RwLock<super::types::ProxyStatus>>) {
        *self.0.runtime_status.lock().unwrap() = Some(status);
    }
    pub fn reset_http_status(&self) {
        *self.0.http_status.lock().unwrap() = None;
    }
    pub fn set_http_status(&self, status: u16) {
        *self.0.http_status.lock().unwrap() = Some(status);
    }
    pub fn http_status(&self) -> Option<u16> {
        *self.0.http_status.lock().unwrap()
    }
    pub fn id(&self) -> &str {
        &self.0.id
    }
    pub fn snapshot(&self) -> Evidence {
        self.0.evidence.lock().unwrap().clone()
    }
    pub fn mark_fallback(&self) -> bool {
        let mut e = self.0.evidence.lock().unwrap();
        let changed = !e.fallback;
        e.fallback = true;
        changed
    }
    pub fn skip(&self, provider: &crate::provider::Provider, reason: &str) {
        self.0.evidence.lock().unwrap().attempts.push(Attempt {
            provider_id: provider.id.clone(),
            provider_name: provider.name.clone(),
            outcome: "skipped".into(),
            skip_reason: Some(reason.into()),
            ..Default::default()
        });
    }
    pub fn attempt(&self, attempt: Attempt) -> usize {
        let mut e = self.0.evidence.lock().unwrap();
        e.sent_reasoning = attempt.sent_reasoning.clone();
        e.returned_reasoning = None;
        e.returned_reasoning_note = None;
        e.outcome = if attempt.outcome == "failed" {
            "failed"
        } else {
            "pending"
        }
        .into();
        e.attempts.push(attempt);
        e.attempts.len() - 1
    }
    pub fn interrupt_stream(&self, error: String) {
        let index = self
            .snapshot()
            .attempts
            .iter()
            .rposition(|a| a.outcome != "skipped");
        if let Some(index) = index {
            let started = *self.0.active_started.lock().unwrap();
            self.finish(index, "stream_interrupted", Some(error), started, true);
        }
    }
    fn finish(
        &self,
        index: usize,
        outcome: &str,
        error: Option<String>,
        started: Instant,
        upstream_failure: bool,
    ) {
        let mut e = self.0.evidence.lock().unwrap();
        if e.outcome == "stream_interrupted" {
            return;
        }
        let newly_failed =
            outcome != "success" && !matches!(e.outcome.as_str(), "failed" | "stream_interrupted");
        if newly_failed {
            if let Some(status) = self.0.runtime_status.lock().unwrap().clone() {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let mut s = status.write().await;
                        s.success_requests = s.success_requests.saturating_sub(1);
                        s.failed_requests += 1;
                        if upstream_failure {
                            s.upstream_failed_attempts += 1;
                        }
                        s.success_rate = if s.total_requests == 0 {
                            0.0
                        } else {
                            s.success_requests as f32 / s.total_requests as f32 * 100.0
                        };
                    });
                }
            }
        }
        e.outcome = if outcome == "success" && e.fallback {
            "fallback_success"
        } else if outcome == "success" {
            "direct_success"
        } else {
            outcome
        }
        .into();
        if let Some(a) = e.attempts.get_mut(index) {
            a.outcome = outcome.into();
            a.error = error;
            a.duration_ms = started.elapsed().as_millis() as u64;
        }
    }
    fn returned(&self, index: usize, value: &Value) {
        let Some(value) = reasoning(value.get("response").unwrap_or(value)) else {
            return;
        };
        let mut e = self.0.evidence.lock().unwrap();
        e.returned_reasoning = Some(value.clone());
        if let Some(a) = e.attempts.get_mut(index) {
            a.returned_reasoning = Some(value);
        }
    }
    /// Observe original upstream bytes before any downstream protocol conversion.
    pub fn observe(
        &self,
        response: ProxyResponse,
        index: usize,
        started: Instant,
    ) -> ProxyResponse {
        *self.0.active_started.lock().unwrap() = started;
        let status = response.status();
        let headers = response.headers().clone();
        let sse = response.is_sse();
        if sse {
            self.0
                .streaming
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let trace = self.clone();
        let guard = ResponseGuard {
            trace: trace.clone(),
            index,
            started,
            sse,
            done: false,
        };
        let stream = async_stream::stream! {
            let mut guard = guard;
            let mut source = Box::pin(response.bytes_stream());
            let mut buffer = String::new();
            let mut remainder = Vec::new();
            let mut completed = false;
            let mut failure = None;
            let mut overflow = false;
            while let Some(chunk) = source.next().await {
                match &chunk {
                    Ok(bytes) => {
                        if !overflow {
                            append_utf8_safe(&mut buffer, &mut remainder, bytes);
                            if sse {
                                while let Some(block) = take_sse_block(&mut buffer) {
                                    let data = block.lines().filter_map(|l| strip_sse_field(l, "data")).collect::<Vec<_>>().join("\n");
                                    if data.trim() == "[DONE]" { completed = true; }
                                    if let Ok(value) = serde_json::from_str::<Value>(&data) {
                                        trace.returned(index, &value);
                                        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                                        if matches!(kind, "response.completed" | "message_stop") || value.get("choices").and_then(Value::as_array).is_some_and(|v| v.iter().any(|c| c.get("finish_reason").is_some_and(|v| !v.is_null()))) || value.get("candidates").and_then(Value::as_array).is_some_and(|v| v.iter().any(|c| c.get("finishReason").is_some())) { completed = true; }
                                        if matches!(kind, "error" | "response.failed" | "response.incomplete") || value.get("error").is_some_and(|v| !v.is_null()) {
                                            let message = value.pointer("/response/error/message").or_else(|| value.pointer("/error/message")).and_then(Value::as_str).unwrap_or(if kind.is_empty() { "error" } else { kind });
                                            failure = Some(format!("上游流失败：{}", message.chars().take(2000).collect::<String>()));
                                        }
                                    }
                                }
                            }
                            // Bound diagnostic memory independently of response size.
                            if buffer.len() > 2 * 1024 * 1024 {
                                buffer.clear(); overflow = !sse;
                                trace.0.evidence.lock().unwrap().returned_reasoning_note = Some("响应超过诊断解析上限，未确认返回配置".into());
                            }
                        }
                    }
                    Err(e) => { failure = Some(e.to_string()); }
                }
                if sse && (completed || failure.is_some()) {
                    trace.finish(index, if failure.is_some() { "stream_interrupted" } else { "success" }, failure.clone(), started, failure.is_some());
                    guard.done = true;
                }
                yield chunk;
            }
            if !sse {
                if let Ok(value) = serde_json::from_str::<Value>(&buffer) {
                    trace.returned(index, &value);
                    if value.get("error").is_some_and(|v| !v.is_null()) || matches!(value.get("status").and_then(Value::as_str), Some("failed" | "incomplete")) { failure = Some("上游返回失败或未完成结果".into()); }
                }
            }
            if sse && !completed && failure.is_none() { failure = Some("上游流结束但未收到正常结束标记".into()); }
            let outcome = if failure.is_some() { if sse { "stream_interrupted" } else { "failed" } } else { "success" };
            trace.finish(index, outcome, failure, started, outcome != "success");
            guard.done = true;
        };
        ProxyResponse::streamed(status, headers, stream)
    }
}

struct ResponseGuard {
    trace: RequestTrace,
    index: usize,
    started: Instant,
    sse: bool,
    done: bool,
}
impl Drop for ResponseGuard {
    fn drop(&mut self) {
        if !self.done {
            self.trace.finish(
                self.index,
                if self.sse {
                    "stream_interrupted"
                } else {
                    "failed"
                },
                Some("响应未读完（超时、下游断开或处理取消）".into()),
                self.started,
                false,
            );
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let Some(db) = &self.db else { return };
        let evidence = self.evidence.lock().unwrap();
        let json = serde_json::to_string(&*evidence).unwrap_or_default();
        let last = evidence
            .attempts
            .iter()
            .rev()
            .find(|a| a.outcome != "skipped");
        let provider = last.map(|a| a.provider_id.as_str()).unwrap_or("");
        let status = last.and_then(|a| a.status_code).unwrap_or(0);
        let error = last.and_then(|a| a.error.as_deref());
        let result = (|| -> Result<(), crate::error::AppError> {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute("INSERT OR REPLACE INTO request_observations (request_id, evidence, outcome) VALUES (?1, ?2, ?3)", rusqlite::params![self.id, json, evidence.outcome])?;
            // Even canceled streams and missing usage must have a visible request row.
            conn.execute("INSERT OR IGNORE INTO proxy_request_logs (request_id, provider_id, app_type, model, request_model, status_code, error_message, latency_ms, is_streaming, created_at, reasoning_effort) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", rusqlite::params![self.id, provider, self.app_type, self.model, status, error, self.started.elapsed().as_millis() as i64, self.streaming.load(std::sync::atomic::Ordering::Relaxed) as i64, self.created_at, evidence.sent_reasoning])?;
            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("保存请求路由明细失败: {e}");
        }
        crate::usage_events::notify_log_recorded();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage_stats::LogFilters;
    use bytes::Bytes;
    use serde_json::json;

    fn attempt(trace: &RequestTrace, provider: &str, status: u16, outcome: &str) -> usize {
        trace.attempt(Attempt {
            provider_id: provider.into(),
            provider_name: provider.into(),
            status_code: Some(status),
            outcome: outcome.into(),
            sent_reasoning: Some("high".into()),
            ..Default::default()
        })
    }

    fn response(body: &str, sse: bool) -> ProxyResponse {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static(if sse {
                "text/event-stream"
            } else {
                "application/json"
            }),
        );
        ProxyResponse::buffered(
            http::StatusCode::OK,
            headers,
            Bytes::copy_from_slice(body.as_bytes()),
        )
    }

    #[tokio::test]
    async fn fallback_preserves_failed_attempt_and_three_distinct_reasoning_values() {
        let db = Arc::new(Database::memory().unwrap());
        let trace = RequestTrace::new(
            Some(db.clone()),
            "codex",
            &json!({"model":"alias", "reasoning":{"effort":"xhigh"}}),
        );
        let id = trace.id().to_string();
        attempt(&trace, "a", 429, "failed");
        assert!(trace.mark_fallback());
        assert!(!trace.mark_fallback());
        let index = attempt(&trace, "b", 200, "pending");
        let body = r#"{"reasoning":{"effort":"medium"},"output":[]}"#;
        assert_eq!(
            trace
                .observe(response(body, false), index, Instant::now())
                .bytes()
                .await
                .unwrap(),
            body.as_bytes()
        );
        let e = trace.snapshot();
        assert_eq!(e.client_reasoning.as_deref(), Some("xhigh"));
        assert_eq!(e.sent_reasoning.as_deref(), Some("high"));
        assert_eq!(e.returned_reasoning.as_deref(), Some("medium"));
        assert_eq!(e.outcome, "fallback_success");
        assert_eq!(e.attempts[0].status_code, Some(429));
        drop(trace);
        let log = db.get_request_detail(&id).unwrap().unwrap();
        assert_eq!(log.status_code, 200);
        assert_eq!(log.evidence.unwrap().attempts.len(), 2);
        let logs = db
            .get_request_logs(
                &LogFilters {
                    outcome: Some("fallback_success".into()),
                    ..Default::default()
                },
                0,
                50,
            )
            .unwrap();
        assert_eq!(logs.total, 1);
    }

    #[tokio::test]
    async fn truncated_200_stream_is_failed_in_filters_and_keeps_http_status() {
        let db = Arc::new(Database::memory().unwrap());
        let trace = RequestTrace::new(
            Some(db.clone()),
            "codex",
            &json!({"model":"alias","stream":true}),
        );
        let index = attempt(&trace, "a", 200, "pending");
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        trace
            .observe(response(body, true), index, Instant::now())
            .bytes()
            .await
            .unwrap();
        assert_eq!(trace.snapshot().outcome, "stream_interrupted");
        assert_eq!(trace.snapshot().returned_reasoning, None);
        drop(trace);
        let failed = db
            .get_request_logs(
                &LogFilters {
                    success: Some(false),
                    ..Default::default()
                },
                0,
                50,
            )
            .unwrap();
        assert_eq!(failed.total, 1);
        assert_eq!(failed.data[0].status_code, 200);
        assert_eq!(
            db.get_request_logs(
                &LogFilters {
                    success: Some(true),
                    ..Default::default()
                },
                0,
                50
            )
            .unwrap()
            .total,
            0
        );
    }

    #[tokio::test]
    async fn terminal_event_before_consumer_drop_is_success_and_failure_wins() {
        for (body, outcome) in [
            ("data: {\"type\":\"response.completed\",\"response\":{\"reasoning\":{\"effort\":\"high\"}}}\n\n", "direct_success"),
            ("data: {\"type\":\"response.failed\"}\n\ndata: [DONE]\n\n", "stream_interrupted"),
        ] {
            let trace = RequestTrace::default();
            let index = attempt(&trace, "a", 200, "pending");
            let mut stream = Box::pin(trace.observe(response(body, true), index, Instant::now()).bytes_stream());
            stream.next().await.unwrap().unwrap();
            drop(stream); // A converter may stop immediately at the terminal marker.
            assert_eq!(trace.snapshot().outcome, outcome);
        }
    }

    #[tokio::test]
    async fn dropped_response_still_records_interruption_without_usage() {
        let db = Arc::new(Database::memory().unwrap());
        let trace = RequestTrace::new(
            Some(db.clone()),
            "codex",
            &json!({"model":"alias","stream":true}),
        );
        let id = trace.id().to_string();
        let index = attempt(&trace, "a", 200, "pending");
        drop(trace.observe(response("", true), index, Instant::now()));
        assert_eq!(trace.snapshot().outcome, "stream_interrupted");
        drop(trace);
        assert_eq!(db.get_request_detail(&id).unwrap().unwrap().input_tokens, 0);
        db.conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM proxy_request_logs WHERE request_id = ?1",
                [&id],
            )
            .unwrap();
        let count: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM request_observations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn chunk_boundaries_preserve_returned_metadata_and_do_not_store_content() {
        let trace = RequestTrace::default();
        let index = attempt(&trace, "a", 200, "pending");
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"reasoning\":{\"effort\":\"high\"},\"output\":\"私密正文\"}}\r\n\r\n";
        let chunks: Vec<_> = body
            .as_bytes()
            .chunks(1)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let headers = response("", true).headers().clone();
        let upstream =
            ProxyResponse::streamed(http::StatusCode::OK, headers, futures::stream::iter(chunks));
        assert_eq!(
            trace
                .observe(upstream, index, Instant::now())
                .bytes()
                .await
                .unwrap(),
            body.as_bytes()
        );
        assert_eq!(trace.snapshot().returned_reasoning.as_deref(), Some("high"));
        assert!(!serde_json::to_string(&trace.snapshot())
            .unwrap()
            .contains("私密正文"));
    }

    #[tokio::test]
    async fn stream_failure_updates_runtime_once_and_skips_do_not_count_as_attempts() {
        let trace = RequestTrace::default();
        let status = Arc::new(tokio::sync::RwLock::new(super::super::types::ProxyStatus {
            total_requests: 1,
            success_requests: 1,
            ..Default::default()
        }));
        trace.set_runtime_status(status.clone());
        trace.skip(
            &crate::provider::Provider::with_id(
                "full".into(),
                "Full provider".into(),
                json!({}),
                None,
            ),
            "并发名额已满",
        );
        trace.mark_fallback();
        let index = attempt(&trace, "a", 200, "pending");
        trace
            .observe(
                response(
                    "data: {\"type\":\"response.failed\"}\n\ndata: [DONE]\n\n",
                    true,
                ),
                index,
                Instant::now(),
            )
            .bytes()
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let status = status.read().await;
        assert_eq!(status.upstream_failed_attempts, 1);
        assert_eq!(status.success_requests, 0);
        assert_eq!(status.failed_requests, 1);
        assert_eq!(trace.snapshot().attempts[0].status_code, None);
        assert_eq!(
            trace.snapshot().attempts[0].skip_reason.as_deref(),
            Some("并发名额已满")
        );
    }
}
