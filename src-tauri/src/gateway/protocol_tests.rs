//! Contract tests over real loopback HTTP: client -> gateway -> strict mock upstream.
//! No desktop, external credentials, or user database is required.
use super::*;
use crate::gateway_runtime::GatewayRuntime;
use axum::{
    body::Body, extract::State, http::StatusCode, response::Response, routing::post, Json, Router,
};
use std::sync::{Arc, Mutex};

const ALIAS: &str = "matrix-alias";
const MODEL: &str = "matrix-upstream";
const TEXT: &str = "matrix reply 中文";
const CALL: &str = "call_matrix";
const ARGS: &str = r#"{"city":"杭州"}"#;

#[derive(Clone, Copy, Debug)]
enum Format {
    Chat,
    Responses,
    Anthropic,
}
impl Format {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
            Self::Anthropic => "/v1/messages",
        }
    }
    fn gateway(self) -> GatewayApiFormat {
        match self {
            Self::Chat => GatewayApiFormat::OpenaiChat,
            Self::Responses => GatewayApiFormat::OpenaiResponses,
            Self::Anthropic => GatewayApiFormat::Anthropic,
        }
    }
}
const FORMATS: [Format; 3] = [Format::Chat, Format::Responses, Format::Anthropic];

fn request(format: Format, stream: bool, scenario: &str) -> Value {
    let schema =
        json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]});
    let mut body = match format {
        Format::Chat => json!({"model":ALIAS,"stream":stream,"max_tokens":128,
            "messages":[{"role":"system","content":"matrix system"},{"role":"user","content":scenario}]}),
        Format::Responses => json!({"model":ALIAS,"stream":stream,"max_output_tokens":128,
            "instructions":"matrix system","input":[{"role":"user","content":scenario}]}),
        Format::Anthropic => json!({"model":ALIAS,"stream":stream,"max_tokens":128,
            "system":"matrix system","messages":[{"role":"user","content":scenario}]}),
    };
    if scenario != "text" {
        body["tools"] = match format {
            Format::Chat => {
                json!([{"type":"function","function":{"name":"weather","description":"weather lookup","parameters":schema}}])
            }
            Format::Responses => {
                json!([{"type":"function","name":"weather","description":"weather lookup","parameters":schema}])
            }
            Format::Anthropic => {
                json!([{"name":"weather","description":"weather lookup","input_schema":schema}])
            }
        };
    }
    if scenario == "result" {
        let history = match format {
            Format::Chat => vec![
                json!({"role":"assistant","content":null,"tool_calls":[{"id":CALL,"type":"function","function":{"name":"weather","arguments":ARGS}}]}),
                json!({"role":"tool","tool_call_id":CALL,"content":"matrix tool result"}),
            ],
            Format::Responses => vec![
                json!({"type":"function_call","call_id":CALL,"name":"weather","arguments":ARGS}),
                json!({"type":"function_call_output","call_id":CALL,"output":"matrix tool result"}),
            ],
            Format::Anthropic => vec![
                json!({"role":"assistant","content":[{"type":"tool_use","id":CALL,"name":"weather","input":{"city":"杭州"}}]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":CALL,"content":"matrix tool result"}]}),
            ],
        };
        let key = if matches!(format, Format::Responses) {
            "input"
        } else {
            "messages"
        };
        body[key].as_array_mut().unwrap().extend(history);
    }
    body
}

fn response_json(format: Format, tool: bool) -> Value {
    match format {
        Format::Chat => {
            json!({"id":"chatcmpl_matrix","object":"chat.completion","created":1,"model":MODEL,
            "choices":[{"index":0,"message":if tool {json!({"role":"assistant","content":null,"tool_calls":[{"id":CALL,"type":"function","function":{"name":"weather","arguments":ARGS}}]})} else {json!({"role":"assistant","content":TEXT})},"finish_reason":if tool {"tool_calls"} else {"stop"}}],
            "usage":{"prompt_tokens":4,"completion_tokens":5,"total_tokens":9}})
        }
        Format::Responses => {
            json!({"id":"resp_matrix","object":"response","created_at":1,"status":"completed","model":MODEL,
            "output":[if tool {json!({"id":"fc_matrix","type":"function_call","status":"completed","call_id":CALL,"name":"weather","arguments":ARGS})} else {json!({"id":"msg_matrix","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":TEXT,"annotations":[]}]})}],
            "usage":{"input_tokens":4,"output_tokens":5,"total_tokens":9}})
        }
        Format::Anthropic => {
            json!({"id":"msg_matrix","type":"message","role":"assistant","model":MODEL,
            "content":[if tool {json!({"type":"tool_use","id":CALL,"name":"weather","input":{"city":"杭州"}})} else {json!({"type":"text","text":TEXT})}],
            "stop_reason":if tool {"tool_use"} else {"end_turn"},"stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":5}})
        }
    }
}

fn response_sse(format: Format, tool: bool) -> String {
    let full = response_json(format, tool);
    let mut events = Vec::<Value>::new();
    match format {
        Format::Chat => {
            let delta = if tool {
                json!({"role":"assistant","tool_calls":[{"index":0,"id":CALL,"type":"function","function":{"name":"weather","arguments":""}}]})
            } else {
                json!({"role":"assistant","content":""})
            };
            let mut chunk = json!({"id":"chatcmpl_matrix","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            events.push(chunk.clone());
            chunk["choices"][0]["delta"] = if tool {
                json!({"tool_calls":[{"index":0,"function":{"arguments":ARGS}}]})
            } else {
                json!({"content":TEXT})
            };
            events.push(chunk.clone());
            chunk["choices"][0]["delta"] = json!({});
            chunk["choices"][0]["finish_reason"] = json!(if tool { "tool_calls" } else { "stop" });
            chunk["usage"] = full["usage"].clone();
            events.push(chunk);
        }
        Format::Responses => {
            let mut created = full.clone();
            created["status"] = json!("in_progress");
            created["output"] = json!([]);
            events.push(json!({"type":"response.created","response":created}));
            let item = full["output"][0].clone();
            let mut added = item.clone();
            added["status"] = json!("in_progress");
            if tool {
                added["arguments"] = json!("");
            } else {
                added["content"] = json!([]);
            }
            events.push(json!({"type":"response.output_item.added","output_index":0,"item":added}));
            if tool {
                events.push(json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_matrix","delta":ARGS}));
                events.push(json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_matrix","arguments":ARGS}));
            } else {
                events.push(json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"msg_matrix","part":{"type":"output_text","text":"","annotations":[]}}));
                events.push(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_matrix","delta":TEXT}));
                events.push(json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg_matrix","text":TEXT}));
                events.push(json!({"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"msg_matrix","part":item["content"][0]}));
            }
            events.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
            events.push(json!({"type":"response.completed","response":full}));
        }
        Format::Anthropic => {
            let mut start = full.clone();
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            start["usage"]["output_tokens"] = json!(0);
            events.push(json!({"type":"message_start","message":start}));
            events.push(json!({"type":"content_block_start","index":0,"content_block":if tool {json!({"type":"tool_use","id":CALL,"name":"weather","input":{}})} else {json!({"type":"text","text":""})}}));
            events.push(json!({"type":"content_block_delta","index":0,"delta":if tool {json!({"type":"input_json_delta","partial_json":ARGS})} else {json!({"type":"text_delta","text":TEXT})}}));
            events.push(json!({"type":"content_block_stop","index":0}));
            events.push(json!({"type":"message_delta","delta":{"stop_reason":full["stop_reason"],"stop_sequence":null},"usage":{"output_tokens":5}}));
            events.push(json!({"type":"message_stop"}));
        }
    }
    let mut wire = String::new();
    for event in events {
        if let Some(kind) = event["type"].as_str() {
            wire.push_str(&format!("event: {kind}\n"));
        }
        wire.push_str(&format!("data: {event}\n\n"));
    }
    if matches!(format, Format::Chat) {
        wire.push_str("data: [DONE]\n\n");
    }
    wire
}

#[derive(Clone)]
struct Mock {
    format: Format,
    tool: bool,
    status: StatusCode,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}
async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.seen.lock().unwrap().push((headers, body.clone()));
    if mock.status != StatusCode::OK {
        return Response::builder()
            .status(mock.status)
            .header("content-type", "application/json")
            .header("retry-after", "60")
            .body(Body::from(
                json!({"error":{"message":"mock overloaded","type":"rate_limit_error"}})
                    .to_string(),
            ))
            .unwrap();
    }
    let (content_type, data) = if body["stream"] == true {
        ("text/event-stream", response_sse(mock.format, mock.tool))
    } else {
        (
            "application/json",
            response_json(mock.format, mock.tool).to_string(),
        )
    };
    // Small chunks split JSON and UTF-8 too; SSE parsing must not depend on TCP boundaries.
    let chunks: Vec<_> = data
        .as_bytes()
        .chunks(7)
        .map(|chunk| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(chunk)))
        .collect();
    Response::builder()
        .header("content-type", content_type)
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}

fn check_upstream(
    format: Format,
    headers: &HeaderMap,
    body: &Value,
    scenario: &str,
    call_id: &str,
) -> Result<(), String> {
    if body["model"] != MODEL {
        return Err(format!("alias not mapped: {body}"));
    }
    let serialized = body.to_string();
    if serialized.contains("matrix-local-key")
        || headers
            .values()
            .any(|v| v == "matrix-local-key" || v == "Bearer matrix-local-key")
    {
        return Err("local key leaked upstream".into());
    }
    if !serialized.contains("matrix system") {
        return Err(format!("system instruction lost: {body}"));
    }
    let (input, tokens, tool_name, parameters) = match format {
        Format::Chat => (
            &body["messages"],
            body.get("max_completion_tokens").or(body.get("max_tokens")),
            &body["tools"][0]["function"]["name"],
            &body["tools"][0]["function"]["parameters"],
        ),
        Format::Responses => (
            &body["input"],
            body.get("max_output_tokens"),
            &body["tools"][0]["name"],
            &body["tools"][0]["parameters"],
        ),
        Format::Anthropic => (
            &body["messages"],
            body.get("max_tokens"),
            &body["tools"][0]["name"],
            &body["tools"][0]["input_schema"],
        ),
    };
    if !input.is_array() {
        return Err(format!("incorrect upstream protocol: {body}"));
    }
    if tokens.and_then(Value::as_u64) != Some(128) {
        return Err(format!("output token budget lost: {body}"));
    }
    if scenario != "text"
        && (tool_name != "weather" || parameters["properties"]["city"]["type"] != "string")
    {
        return Err(format!("tool definition lost: {body}"));
    }
    if scenario == "result" {
        let valid = match format {
            Format::Chat => input.as_array().unwrap().iter().any(|v| {
                v["role"] == "tool"
                    && v["tool_call_id"] == call_id
                    && v["content"] == "matrix tool result"
            }),
            Format::Responses => input.as_array().unwrap().iter().any(|v| {
                v["type"] == "function_call_output"
                    && v["call_id"] == call_id
                    && v["output"] == "matrix tool result"
            }),
            Format::Anthropic => input
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v["content"].as_array())
                .flatten()
                .any(|v| {
                    v["type"] == "tool_result"
                        && v["tool_use_id"] == call_id
                        && v["content"] == "matrix tool result"
                }),
        };
        if !valid {
            return Err(format!("tool result/correlation lost: {body}"));
        }
    }
    Ok(())
}

fn check_response(
    format: Format,
    stream: bool,
    tool: bool,
    wire: &str,
    call_id: &mut String,
) -> Result<(), String> {
    let values: Vec<Value> = if stream {
        wire.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|line| *line != "[DONE]")
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?
    } else {
        vec![serde_json::from_str(wire).map_err(|e| e.to_string())?]
    };
    let mut text = String::new();
    let mut args = String::new();
    let mut name = String::new();
    let mut id = String::new();
    let mut terminal = 0;
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    for value in &values {
        match (format, stream) {
            (Format::Chat, _) => {
                let msg = if stream {
                    &value["choices"][0]["delta"]
                } else {
                    &value["choices"][0]["message"]
                };
                text.push_str(msg["content"].as_str().unwrap_or(""));
                if let Some(calls) = msg["tool_calls"].as_array() {
                    for call in calls {
                        if let Some(v) = call["id"].as_str() {
                            id = v.into();
                        }
                        if let Some(v) = call["function"]["name"].as_str() {
                            name = v.into();
                        }
                        args.push_str(call["function"]["arguments"].as_str().unwrap_or(""));
                    }
                }
                if value["choices"][0]["finish_reason"].is_string() {
                    terminal += 1;
                }
                input_tokens =
                    input_tokens.max(value["usage"]["prompt_tokens"].as_u64().unwrap_or(0));
                output_tokens =
                    output_tokens.max(value["usage"]["completion_tokens"].as_u64().unwrap_or(0));
            }
            (Format::Responses, _) => {
                let response = if stream { &value["response"] } else { value };
                if !stream || value["type"] == "response.completed" {
                    terminal += 1;
                    if let Some(items) = response["output"].as_array() {
                        for item in items {
                            if item["type"] == "function_call" {
                                id = item["call_id"].as_str().unwrap_or("").into();
                                name = item["name"].as_str().unwrap_or("").into();
                                args = item["arguments"].as_str().unwrap_or("").into();
                            }
                            if !stream {
                                if let Some(parts) = item["content"].as_array() {
                                    for part in parts {
                                        text.push_str(part["text"].as_str().unwrap_or(""));
                                    }
                                }
                            }
                        }
                    }
                    input_tokens = response["usage"]["input_tokens"].as_u64().unwrap_or(0);
                    output_tokens = response["usage"]["output_tokens"].as_u64().unwrap_or(0);
                }
                if stream && value["type"] == "response.output_text.delta" {
                    text.push_str(value["delta"].as_str().unwrap_or(""));
                }
            }
            (Format::Anthropic, false) => {
                for part in value["content"].as_array().ok_or("missing content")? {
                    text.push_str(part["text"].as_str().unwrap_or(""));
                    if part["type"] == "tool_use" {
                        id = part["id"].as_str().unwrap_or("").into();
                        name = part["name"].as_str().unwrap_or("").into();
                        args = part["input"].to_string();
                    }
                }
                terminal += usize::from(value["stop_reason"].is_string());
                input_tokens = value["usage"]["input_tokens"].as_u64().unwrap_or(0);
                output_tokens = value["usage"]["output_tokens"].as_u64().unwrap_or(0);
            }
            (Format::Anthropic, true) => {
                if value["type"] == "message_start" {
                    input_tokens = value["message"]["usage"]["input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                }
                if value["type"] == "message_delta" {
                    input_tokens =
                        input_tokens.max(value["usage"]["input_tokens"].as_u64().unwrap_or(0));
                    output_tokens = value["usage"]["output_tokens"].as_u64().unwrap_or(0);
                }
                if value["type"] == "message_stop" {
                    terminal += 1;
                }
                let part = &value["content_block"];
                if part["type"] == "tool_use" {
                    id = part["id"].as_str().unwrap_or("").into();
                    name = part["name"].as_str().unwrap_or("").into();
                }
                text.push_str(value["delta"]["text"].as_str().unwrap_or(""));
                args.push_str(value["delta"]["partial_json"].as_str().unwrap_or(""));
            }
        }
    }
    if terminal != 1 {
        return Err(format!(
            "expected exactly one terminal, got {terminal}: {wire}"
        ));
    }
    if tool {
        if id.is_empty()
            || name != "weather"
            || serde_json::from_str::<Value>(&args).ok() != Some(json!({"city":"杭州"}))
        {
            return Err(format!(
                "tool call corrupted: id={id}, name={name}, args={args}; {wire}"
            ));
        }
        *call_id = id;
    } else if text != TEXT {
        return Err(format!("text missing/duplicated: {text:?}; {wire}"));
    }
    if input_tokens != 4 || output_tokens != 5 {
        return Err(format!(
            "usage lost: {input_tokens}/{output_tokens}; {wire}"
        ));
    }
    if stream
        && matches!(format, Format::Chat)
        && wire.lines().filter(|l| *l == "data: [DONE]").count() != 1
    {
        return Err(format!("missing/duplicate DONE: {wire}"));
    }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn gateway_protocol_matrix_over_real_http() {
    let mut failures = Vec::new();
    for up in FORMATS {
        for down in FORMATS {
            for stream in [false, true] {
                let mut call_id = CALL.to_string();
                for scenario in ["text", "tool", "result"] {
                    let label = format!("{down:?} -> {up:?}, stream={stream}, {scenario}");
                    let mock = Mock {
                        format: up,
                        tool: scenario == "tool",
                        status: StatusCode::OK,
                        seen: Arc::new(Mutex::new(Vec::new())),
                    };
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let addr = listener.local_addr().unwrap();
                    let router = Router::new()
                        .route(up.path(), post(upstream))
                        .with_state(mock.clone());
                    let task = tokio::spawn(async move {
                        axum::serve(listener, router).await.unwrap();
                    });
                    let db = Arc::new(Database::memory().unwrap());
                    let provider: GatewayProvider = serde_json::from_value(json!({"id":"matrix","name":"matrix","baseUrl":format!("http://{addr}"),"apiKey":"matrix-upstream-key","models":[{"alias":ALIAS,"upstreamModel":MODEL,"apiFormat":up.gateway()}]})).unwrap();
                    let config = GatewayConfig {
                        local_api_key: "matrix-local-key".into(),
                        enable_logging: false,
                        providers: vec![provider],
                        ..Default::default()
                    };
                    db.set_setting(CONFIG_KEY, &serde_json::to_string(&config).unwrap())
                        .unwrap();
                    let server = configured_runtime(db, &config).await;
                    let info = server.start().await.unwrap();
                    let client = reqwest::Client::builder()
                        .no_proxy()
                        .timeout(Duration::from_secs(15))
                        .build()
                        .unwrap();
                    let mut payload = request(down, stream, scenario);
                    replace_call_id(&mut payload, &call_id);
                    let outcome = async {
                        let response = client
                            .post(format!("http://127.0.0.1:{}{}", info.port, down.path()))
                            .bearer_auth("matrix-local-key")
                            .header("anthropic-version", "2023-06-01")
                            .json(&payload)
                            .send()
                            .await
                            .map_err(|e| e.to_string())?;
                        let status = response.status();
                        let wire = response.text().await.map_err(|e| e.to_string())?;
                        if status != StatusCode::OK {
                            return Err(format!("HTTP {status}: {wire}"));
                        }
                        let seen = mock.seen.lock().unwrap();
                        if seen.len() != 1 {
                            return Err(format!(
                                "expected one upstream request, got {}",
                                seen.len()
                            ));
                        }
                        check_upstream(up, &seen[0].0, &seen[0].1, scenario, &call_id)?;
                        check_response(down, stream, scenario == "tool", &wire, &mut call_id)
                    }
                    .await;
                    server.stop().await.unwrap();
                    task.abort();
                    let _ = task.await;
                    if let Err(error) = outcome {
                        failures.push(format!("{label}: {error}"));
                    } else {
                        println!("PASS {label}");
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of 54 combinations failed:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn replace_call_id(value: &mut Value, id: &str) {
    match value {
        Value::String(s) if s == CALL => *s = id.to_string(),
        Value::Array(values) => values.iter_mut().for_each(|v| replace_call_id(v, id)),
        Value::Object(values) => values.values_mut().for_each(|v| replace_call_id(v, id)),
        _ => {}
    }
}

#[tokio::test]
#[serial_test::serial]
async fn gateway_http_auth_failover_and_rate_limit_cooldown() {
    for down in FORMATS {
        for stream in [false, true] {
            let mut tasks = Vec::new();
            let mut mocks = Vec::new();
            let mut providers = Vec::new();
            for (id, status) in [
                ("limited", StatusCode::TOO_MANY_REQUESTS),
                ("healthy", StatusCode::OK),
            ] {
                let mock = Mock {
                    format: Format::Chat,
                    tool: false,
                    status,
                    seen: Arc::new(Mutex::new(Vec::new())),
                };
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let router = Router::new()
                    .route(Format::Chat.path(), post(upstream))
                    .with_state(mock.clone());
                tasks.push(tokio::spawn(async move {
                    axum::serve(listener, router).await.unwrap();
                }));
                providers.push(serde_json::from_value::<GatewayProvider>(json!({"id":id,"name":id,"baseUrl":format!("http://{addr}"),"apiKey":"matrix-upstream-key","models":[{"alias":ALIAS,"upstreamModel":MODEL,"apiFormat":"openai_chat"}]})).unwrap());
                mocks.push(mock);
            }
            let db = Arc::new(Database::memory().unwrap());
            let config = GatewayConfig {
                local_api_key: "matrix-local-key".into(),
                providers,
                enable_logging: false,
                ..Default::default()
            };
            db.set_setting(CONFIG_KEY, &serde_json::to_string(&config).unwrap())
                .unwrap();
            let runtime = configured_runtime(db, &config).await;
            let info = runtime.start().await.unwrap();
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap();
            let url = format!("http://127.0.0.1:{}{}", info.port, down.path());
            let payload = request(down, stream, "text");
            let rejected = client
                .post(&url)
                .bearer_auth("wrong-key")
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
            assert!(mocks
                .iter()
                .all(|mock| mock.seen.lock().unwrap().is_empty()));
            for _ in 0..2 {
                let response = client
                    .post(&url)
                    .bearer_auth("matrix-local-key")
                    .json(&payload)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "{down:?}, stream={stream}"
                );
                let wire = response.text().await.unwrap();
                check_response(down, stream, false, &wire, &mut String::new()).unwrap();
            }
            assert_eq!(
                mocks[0].seen.lock().unwrap().len(),
                1,
                "limited upstream must not be retried during cooldown"
            );
            assert_eq!(mocks[1].seen.lock().unwrap().len(), 2);
            let app_type = if matches!(down, Format::Anthropic) {
                "claude"
            } else {
                "codex"
            };
            let id = generated_provider_id("limited", GatewayApiFormat::OpenaiChat);
            assert!(
                runtime
                    .get_provider_cooldown_remaining_seconds(&id, app_type)
                    .await
                    .unwrap_or(0)
                    > 0
            );
            runtime.stop().await.unwrap();
            for task in tasks {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

// Use the same configuration/materialization path as desktop startup, including
// the app-level retry/failover settings consumed by RequestContext.
async fn configured_runtime(db: Arc<Database>, config: &GatewayConfig) -> GatewayRuntime {
    let state = AppState::new(db);
    let mut config = config.clone();
    config.listen_port = 0;
    apply_runtime_config(&state, &config).await.unwrap();
    state.gateway_runtime
}


// Routing contracts use the same public listeners and materialized config as the desktop.
// Each candidate may use a different protocol and reject an otherwise valid request.
async fn routing_contract(
    down: Format,
    payload: Value,
    candidates: Vec<(Format, Value, Option<Value>)>,
) -> (StatusCode, String, Vec<Vec<Value>>) {
    let mut tasks = Vec::new();
    let mut captures = Vec::new();
    let mut providers = Vec::new();
    for (index, (format, options, rejection)) in candidates.into_iter().enumerate() {
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(format.path(), post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            let rejection = rejection.clone();
            async move {
                captured.lock().unwrap().push(body.clone());
                if let Some(error) = rejection {
                    let status = error.get("_http_status").and_then(Value::as_u64).unwrap_or(400) as u16;
                    if let Some(sse) = error.get("_sse").and_then(Value::as_str) {
                        return Response::builder().status(status).header("content-type", "text/event-stream")
                            .body(Body::from(sse.to_string())).unwrap();
                    }
                    return Response::builder().status(status).header("content-type", "application/json")
                        .body(Body::from(error.to_string())).unwrap();
                }
                if body["stream"] == true {
                    Response::builder().header("content-type", "text/event-stream")
                        .body(Body::from(response_sse(format, false))).unwrap()
                } else {
                    let mut response = response_json(format, false);
                    response["vendor_extension"] = json!({"preserve":true});
                    Response::builder().header("content-type", "application/json")
                        .body(Body::from(response.to_string())).unwrap()
                }
            }
        }));
        tasks.push(tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); }));
        let mut provider = json!({"id":format!("route-{index}"),"name":format!("route-{index}"),
            "baseUrl":format!("http://{addr}/v1"),"apiKey":"upstream-key",
            "models":[{"alias":ALIAS,"upstreamModel":"deepseek-v4-flash","apiFormat":format.gateway()}]});
        for (key, value) in options.as_object().unwrap() { provider[key] = value.clone(); }
        providers.push(serde_json::from_value::<GatewayProvider>(provider).unwrap());
        captures.push(seen);
    }
    let db = Arc::new(Database::memory().unwrap());
    let config = GatewayConfig { providers, local_api_key:"matrix-local-key".into(), enable_logging:false, ..Default::default() };
    db.set_setting(CONFIG_KEY, &serde_json::to_string(&config).unwrap()).unwrap();
    let runtime = configured_runtime(db, &config).await;
    let info = runtime.start().await.unwrap();
    let response = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(15)).build().unwrap()
        .post(format!("http://127.0.0.1:{}{}?trace=contract", info.port, down.path()))
        .bearer_auth("matrix-local-key").json(&payload).send().await.unwrap();
    let status = response.status();
    let wire = response.text().await.unwrap();
    runtime.stop().await.unwrap();
    for task in tasks { task.abort(); let _ = task.await; }
    (status, wire, captures.iter().map(|seen| seen.lock().unwrap().clone()).collect())
}

fn agent_chat_request(stream: bool) -> Value {
    json!({"model":ALIAS,"stream":stream,"reasoning_effort":"xhigh",
        "messages":[{"role":"user","content":"continue"},
            {"role":"assistant","content":null,"reasoning_content":"preserve prior reasoning",
             "tool_calls":[{"id":"call_delegate","type":"function","function":{"name":"delegate_task","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_delegate","content":"done"}],
        "tools":[{"type":"function","function":{"name":"delegate_task","strict":false,"parameters":{
            "type":"object","properties":{"tasks":{"type":"array","items":{"type":"object","properties":{"goal":{"type":"string"}},"required":["goal"]}},
            "mode":{"enum":[null,"a","b"]}}}}}],
        "stop":["STOP"],"frequency_penalty":0.4,"presence_penalty":0.2,"seed":12,
        "response_format":{"type":"json_object"},"logprobs":true,"top_logprobs":2,
        "stream_options":{"include_usage":true},"vendor_extension":{"keep":true}})
}

#[tokio::test]
#[serial_test::serial]
async fn native_chat_preserves_agent_fields_schema_and_response() {
    for stream in [false, true] {
        let payload = agent_chat_request(stream);
        let (status, wire, seen) = routing_contract(Format::Chat, payload.clone(), vec![(Format::Chat, json!({}), None)]).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        let mut expected = payload;
        expected["model"] = json!("deepseek-v4-flash");
        assert_eq!(seen[0], vec![expected]);
        if !stream {
            let response: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(response["vendor_extension"]["preserve"], true);
            assert_eq!(response["id"], "chatcmpl_matrix");
        } else {
            check_response(Format::Chat, true, false, &wire, &mut String::new()).unwrap();
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn mixed_protocol_failover_rebuilds_each_candidate_from_original() {
    for (first, second) in [(Format::Responses, Format::Chat), (Format::Chat, Format::Responses)] {
        for stream in [false, true] {
            let payload = agent_chat_request(stream);
            let rejection = json!({"error":{"code":"unsupported_parameter","message":"reasoning is not supported"}});
            let (status, wire, seen) = routing_contract(Format::Chat, payload.clone(), vec![
                (first, json!({"chatReasoningProfile":"deepseek"}), Some(rejection)),
                (second, json!({}), None),
            ]).await;
            assert_eq!(status, StatusCode::OK, "{first:?} -> {second:?}: {wire}");
            assert_eq!(seen[0].len(), 1);
            assert_eq!(seen[1].len(), 1);
            if matches!(second, Format::Chat) {
                let mut expected = payload;
                expected["model"] = json!("deepseek-v4-flash");
                assert_eq!(seen[1][0], expected);
            } else {
                assert!(seen[1][0].get("messages").is_none());
                assert_eq!(seen[1][0]["reasoning"]["effort"], "xhigh");
                assert!(seen[1][0].get("thinking").is_none());
            }
            check_response(Format::Chat, stream, false, &wire, &mut String::new()).unwrap();
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn responses_bridge_uses_explicit_reasoning_wire_profile() {
    for (profile, thinking, effort) in [("auto", false, "xhigh"), ("openai", false, "xhigh"), ("deepseek", true, "max")] {
        let mut payload = request(Format::Responses, false, "text");
        payload["reasoning"] = json!({"effort":"xhigh"});
        let (status, wire, seen) = routing_contract(Format::Responses, payload, vec![(Format::Chat, json!({"chatReasoningProfile":profile}), None)]).await;
        assert_eq!(status, StatusCode::OK, "{wire}");
        assert_eq!(seen[0][0].get("thinking").is_some(), thinking);
        assert_eq!(seen[0][0]["reasoning_effort"], effort);
    }
}

#[tokio::test]
#[serial_test::serial]
async fn schema_compatibility_is_opt_in_and_preserves_nested_constraints() {
    let payload = agent_chat_request(false);
    let (status, wire, seen) = routing_contract(Format::Chat, payload.clone(), vec![(Format::Chat, json!({"chatSchemaRequiredDefaults":true}), None)]).await;
    assert_eq!(status, StatusCode::OK, "{wire}");
    let mut expected = payload;
    expected["model"] = json!("deepseek-v4-flash");
    expected["tools"][0]["function"]["parameters"]["required"] = json!([]);
    assert_eq!(seen[0][0], expected);
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_input_stops_but_relay_schema_rejection_can_fail_over() {
    for (message, succeeds) in [
        ("Invalid schema for function 'delegate_task': null is not of type array", true),
        ("Invalid JSON", false), ("tool_call_id does not match", false),
    ] {
        let (status, wire, seen) = routing_contract(Format::Chat, agent_chat_request(false), vec![
            (Format::Chat, json!({}), Some(json!({"error":{"message":message}}))),
            (Format::Chat, json!({}), None),
        ]).await;
        assert_eq!(status.is_success(), succeeds, "{wire}");
        assert_eq!(seen[1].len(), usize::from(succeeds));
    }
    let mut payload = agent_chat_request(false);
    payload["model"] = json!("unconfigured-alias");
    let (status, _, seen) = routing_contract(Format::Chat, payload, vec![(Format::Chat, json!({}), None)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(seen[0].is_empty());
}


#[tokio::test]
#[serial_test::serial]
async fn semantic_failures_before_output_fail_over_but_committed_streams_do_not() {
    for format in FORMATS {
        for stream in [false, true] {
            let payload = request(format, stream, "text");
            let mut rejection = json!({"_http_status":200,"error":{"message":"overloaded"}});
            if stream {
                rejection["_sse"] = match format {
                    Format::Chat => json!("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"error\":{\"message\":\"overloaded\"}}\n\n"),
                    Format::Responses => json!("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"overloaded\"}}}\n\n"),
                    Format::Anthropic => json!("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"content\":[]}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n"),
                };
            }
            let (status, wire, seen) = routing_contract(format, payload, vec![
                (format, json!({}), Some(rejection)), (format, json!({}), None),
            ]).await;
            assert_eq!(status, StatusCode::OK, "{wire}");
            assert_eq!(seen[1].len(), 1, "{format:?}, stream={stream}: {wire}");
            check_response(format, stream, false, &wire, &mut String::new()).unwrap();
        }
    }
    let rejection = json!({"_http_status":200,"_sse":"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: {\"error\":{\"message\":\"late failure\"}}\n\n"});
    let (_, wire, seen) = routing_contract(Format::Chat, agent_chat_request(true), vec![
        (Format::Chat, json!({}), Some(rejection)), (Format::Chat, json!({}), None),
    ]).await;
    assert!(wire.contains("partial"));
    assert!(seen[1].is_empty(), "never replay a request after output has been committed");
}

#[tokio::test]
#[serial_test::serial]
async fn malformed_http_2xx_protocol_body_fails_over_before_recording_success() {
    for format in FORMATS {
        let malformed = json!({"_http_status":200,"id":"looks-successful-but-has-no-protocol-output"});
        let (status, wire, seen) = routing_contract(
            format,
            request(format, false, "text"),
            vec![
                (format, json!({}), Some(malformed)),
                (format, json!({}), None),
            ],
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{format:?}: {wire}");
        assert_eq!(seen[0].len(), 1);
        assert_eq!(seen[1].len(), 1, "{format:?}: malformed 2xx must fail over");
        check_response(format, false, false, &wire, &mut String::new()).unwrap();
    }
}
