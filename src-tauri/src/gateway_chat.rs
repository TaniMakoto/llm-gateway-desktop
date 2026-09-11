//! OpenAI Chat Completions compatibility facade.
//!
//! Internally the unified gateway uses the Responses endpoint as the canonical
//! path. The inherited Responses handler already supports Responses, Chat and
//! Anthropic upstreams with failover, so this module only translates the local
//! Chat request/response surface.

use crate::proxy::{handlers, server::ProxyState, ProxyError};
use async_stream::stream;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderValue, Uri},
    response::Response,
};
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub async fn handle_chat_completions(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<Response, ProxyError> {
    crate::gateway::validate_local_auth(state.db.as_ref(), request.headers())?;

    let (mut parts, body) = request.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map_err(|error| ProxyError::InvalidRequest(format!("读取 Chat 请求失败: {error}")))?
        .to_bytes();
    let body_bytes = handlers::decode_codex_request_body(&mut parts.headers, body_bytes)?;
    let chat_body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|error| ProxyError::InvalidRequest(format!("Chat 请求 JSON 无效: {error}")))?;
    let requested_model = chat_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("local-model")
        .to_string();
    let is_stream = chat_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let responses_body = chat_request_to_responses(chat_body)?;
    parts.uri = Uri::from_static("/v1/responses");
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let responses_request = axum::extract::Request::from_parts(
        parts,
        Body::from(
            serde_json::to_vec(&responses_body)
                .map_err(|error| ProxyError::Internal(error.to_string()))?,
        ),
    );

    let response = handlers::handle_responses(State(state), responses_request).await?;
    if !response.status().is_success() {
        return Ok(response);
    }

    if is_stream {
        Ok(responses_sse_to_chat_response(response, requested_model))
    } else {
        responses_json_to_chat_response(response, &requested_model).await
    }
}

pub fn chat_request_to_responses(body: Value) -> Result<Value, ProxyError> {
    let object = body
        .as_object()
        .ok_or_else(|| ProxyError::InvalidRequest("Chat 请求体必须是 JSON 对象".to_string()))?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProxyError::InvalidRequest("缺少 model".to_string()))?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProxyError::InvalidRequest("缺少 messages 数组".to_string()))?;

    if object.get("n").and_then(Value::as_u64).unwrap_or(1) > 1 {
        return Err(ProxyError::InvalidRequest(
            "统一网关暂不支持 n > 1 的 Chat 请求".to_string(),
        ));
    }

    let mut input = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");

        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if call_id.is_empty() {
                return Err(ProxyError::InvalidRequest(
                    "tool 消息缺少 tool_call_id".to_string(),
                ));
            }
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": chat_content_as_text(message.get("content"))
            }));
            continue;
        }

        if let Some(content) = message.get("content") {
            if !content.is_null() && !chat_content_is_empty(content) {
                input.push(json!({
                    "type": "message",
                    "role": normalize_chat_role(role),
                    "content": chat_content_to_responses(content)
                }));
            }
        }

        if role == "assistant" {
            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for (tool_position, tool_call) in tool_calls.iter().enumerate() {
                    let Some(function) = tool_call.get("function") else {
                        continue;
                    };
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let call_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_gateway_{tool_position}"));
                    let arguments = function
                        .get("arguments")
                        .map(arguments_as_string)
                        .unwrap_or_else(|| "{}".to_string());
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                        "status": "completed"
                    }));
                }
            }
        }
    }

    let mut result = Map::new();
    result.insert("model".to_string(), Value::String(model.to_string()));
    result.insert("input".to_string(), Value::Array(input));

    copy_field(object, &mut result, "stream");
    copy_field(object, &mut result, "temperature");
    copy_field(object, &mut result, "top_p");
    copy_field(object, &mut result, "parallel_tool_calls");
    copy_field(object, &mut result, "metadata");
    copy_field(object, &mut result, "store");
    copy_field(object, &mut result, "service_tier");
    copy_field(object, &mut result, "user");

    if let Some(max_tokens) = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
    {
        result.insert("max_output_tokens".to_string(), max_tokens.clone());
    }

    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        let converted = tools
            .iter()
            .filter_map(chat_tool_to_responses)
            .collect::<Vec<_>>();
        if !converted.is_empty() {
            result.insert("tools".to_string(), Value::Array(converted));
        }
    }

    if let Some(tool_choice) = object.get("tool_choice") {
        if let Some(converted) = chat_tool_choice_to_responses(tool_choice) {
            result.insert("tool_choice".to_string(), converted);
        }
    }

    if let Some(response_format) = object.get("response_format") {
        result.insert(
            "text".to_string(),
            json!({ "format": response_format.clone() }),
        );
    }

    if let Some(effort) = object.get("reasoning_effort") {
        result.insert("reasoning".to_string(), json!({ "effort": effort.clone() }));
    }

    Ok(Value::Object(result))
}

fn normalize_chat_role(role: &str) -> &str {
    match role {
        "system" | "developer" | "assistant" | "user" => role,
        _ => "user",
    }
}

fn chat_content_is_empty(content: &Value) -> bool {
    match content {
        Value::String(value) => value.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

fn chat_content_to_responses(content: &Value) -> Value {
    match content {
        Value::String(value) => Value::String(value.clone()),
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") => Some(json!({
                        "type": "input_text",
                        "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                    })),
                    Some("image_url") => {
                        let image_url = part
                            .pointer("/image_url/url")
                            .or_else(|| part.get("image_url"))
                            .and_then(Value::as_str)?;
                        let mut converted = json!({
                            "type": "input_image",
                            "image_url": image_url
                        });
                        if let Some(detail) = part.pointer("/image_url/detail") {
                            converted["detail"] = detail.clone();
                        }
                        Some(converted)
                    }
                    Some("input_image") => Some(part.clone()),
                    _ => None,
                })
                .collect(),
        ),
        other => Value::String(other.to_string()),
    }
}

fn chat_content_as_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn arguments_as_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn chat_tool_to_responses(tool: &Value) -> Option<Value> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let function = tool.get("function")?;
    let name = function.get("name")?.as_str()?;
    let mut converted = json!({
        "type": "function",
        "name": name,
        "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
    });
    if let Some(description) = function.get("description") {
        converted["description"] = description.clone();
    }
    if let Some(strict) = function.get("strict") {
        converted["strict"] = strict.clone();
    }
    Some(converted)
}

fn chat_tool_choice_to_responses(value: &Value) -> Option<Value> {
    if value.is_string() {
        return Some(value.clone());
    }
    let function = value.get("function")?;
    let name = function.get("name")?.as_str()?;
    Some(json!({ "type": "function", "name": name }))
}

fn copy_field(source: &Map<String, Value>, target: &mut Map<String, Value>, field: &str) {
    if let Some(value) = source.get(field) {
        target.insert(field.to_string(), value.clone());
    }
}

async fn responses_json_to_chat_response(
    response: Response,
    requested_model: &str,
) -> Result<Response, ProxyError> {
    let (mut parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|error| ProxyError::Internal(format!("读取 Responses 响应失败: {error}")))?
        .to_bytes();
    let responses_value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| ProxyError::TransformError(format!("Responses JSON 无效: {error}")))?;
    let chat_value = responses_response_to_chat(responses_value, requested_model)?;
    let encoded = serde_json::to_vec(&chat_value)
        .map_err(|error| ProxyError::Internal(error.to_string()))?;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(Response::from_parts(parts, Body::from(encoded)))
}

pub fn responses_response_to_chat(
    response: Value,
    requested_model: &str,
) -> Result<Value, ProxyError> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| ProxyError::TransformError("Responses 响应缺少 output".to_string()))?;

    let mut text = String::new();
    let mut refusal = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") | Some("text") => {
                                if let Some(delta) = part.get("text").and_then(Value::as_str) {
                                    text.push_str(delta);
                                }
                            }
                            Some("refusal") => {
                                if let Some(delta) = part
                                    .get("refusal")
                                    .or_else(|| part.get("text"))
                                    .and_then(Value::as_str)
                                {
                                    refusal.push_str(delta);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("reasoning") => {
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    for part in summary {
                        if let Some(delta) = part
                            .get("text")
                            .or_else(|| part.get("summary_text"))
                            .and_then(Value::as_str)
                        {
                            reasoning.push_str(delta);
                        }
                    }
                }
            }
            Some("function_call") => {
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_gateway");
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let arguments = item
                    .get("arguments")
                    .map(arguments_as_string)
                    .unwrap_or_else(|| "{}".to_string());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }));
            }
            Some("custom_tool_call") => {
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_gateway");
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("custom_tool");
                let input = item
                    .get("input")
                    .map(arguments_as_string)
                    .unwrap_or_default();
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(&json!({"input": input})).unwrap_or_else(|_| "{}".to_string())
                    }
                }));
            }
            _ => {}
        }
    }

    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let finish_reason = if !tool_calls.is_empty() {
        "tool_calls"
    } else if status == "incomplete" {
        "length"
    } else {
        "stop"
    };

    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert(
        "content".to_string(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !refusal.is_empty() {
        message.insert("refusal".to_string(), Value::String(refusal));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    let created = response
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_gateway");

    Ok(json!({
        "id": chat_id(id),
        "object": "chat.completion",
        "created": created,
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "logprobs": Value::Null,
            "finish_reason": finish_reason
        }],
        "usage": responses_usage_to_chat(response.get("usage"))
    }))
}

fn responses_usage_to_chat(usage: Option<&Value>) -> Value {
    let input_tokens = usage
        .and_then(|value| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|value| value.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|value| value.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    let cached_tokens = usage
        .and_then(|value| value.pointer("/input_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .and_then(|value| value.pointer("/output_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": total_tokens,
        "prompt_tokens_details": { "cached_tokens": cached_tokens },
        "completion_tokens_details": { "reasoning_tokens": reasoning_tokens }
    })
}

fn responses_sse_to_chat_response(response: Response, requested_model: String) -> Response {
    let (mut parts, body) = response.into_parts();
    let mut upstream = body.into_data_stream();
    let converted = stream! {
        let mut buffer = String::new();
        let mut converter = ResponsesChatSseConverter::new(requested_model);

        while let Some(next) = upstream.next().await {
            match next {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
                    while let Some(position) = buffer.find("\n\n") {
                        let event = buffer[..position].to_string();
                        buffer.drain(..position + 2);
                        for output in converter.process_sse_event(&event) {
                            yield Ok::<Bytes, axum::Error>(Bytes::from(output));
                        }
                    }
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }

        if !buffer.trim().is_empty() {
            for output in converter.process_sse_event(&buffer) {
                yield Ok::<Bytes, axum::Error>(Bytes::from(output));
            }
        }
        if !converter.finished {
            for output in converter.finish(None) {
                yield Ok::<Bytes, axum::Error>(Bytes::from(output));
            }
        }
    };

    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    Response::from_parts(parts, Body::from_stream(converted))
}

struct ResponsesChatSseConverter {
    id: String,
    requested_model: String,
    created: i64,
    sent_role: bool,
    finished: bool,
    next_tool_index: usize,
    tool_indexes: HashMap<String, usize>,
    last_tool_key: Option<String>,
}

impl ResponsesChatSseConverter {
    fn new(requested_model: String) -> Self {
        Self {
            id: "chatcmpl-gateway".to_string(),
            requested_model,
            created: chrono::Utc::now().timestamp(),
            sent_role: false,
            finished: false,
            next_tool_index: 0,
            tool_indexes: HashMap::new(),
            last_tool_key: None,
        }
    }

    fn process_sse_event(&mut self, raw: &str) -> Vec<String> {
        let data = raw
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            return self.finish(None);
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            return Vec::new();
        };
        self.update_metadata(&event);
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut output = Vec::new();

        match event_type {
            "response.created" | "response.in_progress" => {
                output.extend(self.ensure_role());
            }
            "response.output_text.delta" => {
                output.extend(self.ensure_role());
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    output.push(self.chunk(json!({"content": delta}), None, None));
                }
            }
            "response.refusal.delta" => {
                output.extend(self.ensure_role());
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    output.push(self.chunk(json!({"refusal": delta}), None, None));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                output.extend(self.ensure_role());
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    output.push(self.chunk(json!({"reasoning_content": delta}), None, None));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    if matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call") | Some("custom_tool_call")
                    ) {
                        output.extend(self.ensure_role());
                        output.extend(self.start_tool(item));
                    }
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                output.extend(self.ensure_role());
                let key = event
                    .get("item_id")
                    .or_else(|| event.get("call_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.last_tool_key.clone())
                    .unwrap_or_else(|| "gateway_tool".to_string());
                let index = self.tool_index(&key);
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or_default();
                output.push(self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "function": { "arguments": delta }
                        }]
                    }),
                    None,
                    None,
                ));
            }
            "response.completed" | "response.incomplete" => {
                let response = event.get("response");
                output.extend(self.finish(response));
            }
            "response.failed" | "error" => {
                output.push(format!("data: {}\n\n", event));
                output.extend(self.finish(None));
            }
            _ => {}
        }
        output
    }

    fn update_metadata(&mut self, event: &Value) {
        let response = event.get("response").unwrap_or(event);
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.id = chat_id(id);
        }
        if let Some(created) = response.get("created_at").and_then(Value::as_i64) {
            self.created = created;
        }
    }

    fn ensure_role(&mut self) -> Vec<String> {
        if self.sent_role {
            return Vec::new();
        }
        self.sent_role = true;
        vec![self.chunk(json!({"role": "assistant"}), None, None)]
    }

    fn start_tool(&mut self, item: &Value) -> Vec<String> {
        let key = item
            .get("id")
            .or_else(|| item.get("call_id"))
            .and_then(Value::as_str)
            .unwrap_or("gateway_tool");
        self.last_tool_key = Some(key.to_string());
        let index = self.tool_index(key);
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("call_gateway");
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        vec![self.chunk(
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]
            }),
            None,
            None,
        )]
    }

    fn tool_index(&mut self, key: &str) -> usize {
        if let Some(index) = self.tool_indexes.get(key) {
            return *index;
        }
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.tool_indexes.insert(key.to_string(), index);
        index
    }

    fn finish(&mut self, response: Option<&Value>) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let has_tools = !self.tool_indexes.is_empty();
        let status = response
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let finish_reason = if has_tools {
            "tool_calls"
        } else if status == "incomplete" {
            "length"
        } else {
            "stop"
        };
        let usage = response.map(|value| responses_usage_to_chat(value.get("usage")));
        vec![
            self.chunk(json!({}), Some(finish_reason), usage),
            "data: [DONE]\n\n".to_string(),
        ]
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> String {
        let mut value = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.requested_model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "logprobs": Value::Null,
                "finish_reason": finish_reason
            }]
        });
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        format!("data: {}\n\n", value)
    }
}

fn chat_id(id: &str) -> String {
    if id.starts_with("chatcmpl-") {
        id.to_string()
    } else {
        format!("chatcmpl-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_maps_messages_tools_and_limits() {
        let input = json!({
            "model": "best-code",
            "messages": [
                {"role":"system","content":"You are helpful"},
                {"role":"user","content":"hello"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":1}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"ok"}
            ],
            "tools": [{"type":"function","function":{"name":"lookup","description":"Lookup","parameters":{"type":"object"}}}],
            "max_tokens": 100,
            "stream": true
        });
        let output = chat_request_to_responses(input).unwrap();
        assert_eq!(output["model"], "best-code");
        assert_eq!(output["max_output_tokens"], 100);
        assert_eq!(output["tools"][0]["name"], "lookup");
        assert_eq!(output["input"][2]["type"], "function_call");
        assert_eq!(output["input"][3]["type"], "function_call_output");
    }

    #[test]
    fn responses_response_maps_text_and_tool_calls() {
        let input = json!({
            "id":"resp_1",
            "status":"completed",
            "output":[
                {"type":"message","content":[{"type":"output_text","text":"hello"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"}
            ],
            "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13}
        });
        let output = responses_response_to_chat(input, "best-code").unwrap();
        assert_eq!(output["model"], "best-code");
        assert_eq!(output["choices"][0]["message"]["content"], "hello");
        assert_eq!(output["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(output["usage"]["total_tokens"], 13);
    }
}
