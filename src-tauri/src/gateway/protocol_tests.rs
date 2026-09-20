//! Contract tests over real loopback HTTP: client -> gateway -> strict mock upstream.
//! No desktop, external credentials, or user database is required.
use super::*;
use axum::{body::Body, extract::State, http::StatusCode, response::Response, routing::post, Json, Router};
use std::sync::{Arc, Mutex};
use crate::{gateway_runtime::GatewayRuntime, proxy::ProxyConfig};

const ALIAS: &str = "matrix-alias";
const MODEL: &str = "matrix-upstream";
const TEXT: &str = "matrix reply 中文";
const CALL: &str = "call_matrix";
const ARGS: &str = r#"{"city":"杭州"}"#;

#[derive(Clone, Copy, Debug)]
enum Format { Chat, Responses, Anthropic }
impl Format {
    fn path(self) -> &'static str {
        match self { Self::Chat => "/v1/chat/completions", Self::Responses => "/v1/responses", Self::Anthropic => "/v1/messages" }
    }
    fn gateway(self) -> GatewayApiFormat {
        match self { Self::Chat => GatewayApiFormat::OpenaiChat, Self::Responses => GatewayApiFormat::OpenaiResponses, Self::Anthropic => GatewayApiFormat::Anthropic }
    }
}
const FORMATS: [Format; 3] = [Format::Chat, Format::Responses, Format::Anthropic];

fn request(format: Format, stream: bool, scenario: &str) -> Value {
    let schema = json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]});
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
            Format::Chat => json!([{"type":"function","function":{"name":"weather","description":"weather lookup","parameters":schema}}]),
            Format::Responses => json!([{"type":"function","name":"weather","description":"weather lookup","parameters":schema}]),
            Format::Anthropic => json!([{"name":"weather","description":"weather lookup","input_schema":schema}]),
        };
    }
    if scenario == "result" {
        let history = match format {
            Format::Chat => vec![json!({"role":"assistant","content":null,"tool_calls":[{"id":CALL,"type":"function","function":{"name":"weather","arguments":ARGS}}]}),json!({"role":"tool","tool_call_id":CALL,"content":"matrix tool result"})],
            Format::Responses => vec![json!({"type":"function_call","call_id":CALL,"name":"weather","arguments":ARGS}),json!({"type":"function_call_output","call_id":CALL,"output":"matrix tool result"})],
            Format::Anthropic => vec![json!({"role":"assistant","content":[{"type":"tool_use","id":CALL,"name":"weather","input":{"city":"杭州"}}]}),json!({"role":"user","content":[{"type":"tool_result","tool_use_id":CALL,"content":"matrix tool result"}]})],
        };
        let key = if matches!(format, Format::Responses) { "input" } else { "messages" };
        body[key].as_array_mut().unwrap().extend(history);
    }
    body
}

fn response_json(format: Format, tool: bool) -> Value {
    match format {
        Format::Chat => json!({"id":"chatcmpl_matrix","object":"chat.completion","created":1,"model":MODEL,
            "choices":[{"index":0,"message":if tool {json!({"role":"assistant","content":null,"tool_calls":[{"id":CALL,"type":"function","function":{"name":"weather","arguments":ARGS}}]})} else {json!({"role":"assistant","content":TEXT})},"finish_reason":if tool {"tool_calls"} else {"stop"}}],
            "usage":{"prompt_tokens":4,"completion_tokens":5,"total_tokens":9}}),
        Format::Responses => json!({"id":"resp_matrix","object":"response","created_at":1,"status":"completed","model":MODEL,
            "output":[if tool {json!({"id":"fc_matrix","type":"function_call","status":"completed","call_id":CALL,"name":"weather","arguments":ARGS})} else {json!({"id":"msg_matrix","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":TEXT,"annotations":[]}]})}],
            "usage":{"input_tokens":4,"output_tokens":5,"total_tokens":9}}),
        Format::Anthropic => json!({"id":"msg_matrix","type":"message","role":"assistant","model":MODEL,
            "content":[if tool {json!({"type":"tool_use","id":CALL,"name":"weather","input":{"city":"杭州"}})} else {json!({"type":"text","text":TEXT})}],
            "stop_reason":if tool {"tool_use"} else {"end_turn"},"stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":5}}),
    }
}

fn response_sse(format: Format, tool: bool) -> String {
    let full = response_json(format, tool);
    let mut events = Vec::<Value>::new();
    match format {
        Format::Chat => {
            let delta = if tool { json!({"role":"assistant","tool_calls":[{"index":0,"id":CALL,"type":"function","function":{"name":"weather","arguments":""}}]}) } else { json!({"role":"assistant","content":""}) };
            let mut chunk = json!({"id":"chatcmpl_matrix","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            events.push(chunk.clone());
            chunk["choices"][0]["delta"] = if tool {json!({"tool_calls":[{"index":0,"function":{"arguments":ARGS}}]})} else {json!({"content":TEXT})};
            events.push(chunk.clone());
            chunk["choices"][0]["delta"] = json!({});
            chunk["choices"][0]["finish_reason"] = json!(if tool {"tool_calls"} else {"stop"});
            chunk["usage"] = full["usage"].clone();
            events.push(chunk);
        }
        Format::Responses => {
            let mut created = full.clone(); created["status"] = json!("in_progress"); created["output"] = json!([]);
            events.push(json!({"type":"response.created","response":created}));
            let item = full["output"][0].clone();
            let mut added = item.clone(); added["status"] = json!("in_progress");
            if tool { added["arguments"] = json!(""); } else { added["content"] = json!([]); }
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
            let mut start = full.clone(); start["content"] = json!([]); start["stop_reason"] = Value::Null; start["usage"]["output_tokens"] = json!(0);
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
        if let Some(kind) = event["type"].as_str() { wire.push_str(&format!("event: {kind}\n")); }
        wire.push_str(&format!("data: {event}\n\n"));
    }
    if matches!(format, Format::Chat) { wire.push_str("data: [DONE]\n\n"); }
    wire
}

#[derive(Clone)]
struct Mock { format: Format, tool: bool, seen: Arc<Mutex<Vec<(HeaderMap, Value)>>> }
async fn upstream(State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    mock.seen.lock().unwrap().push((headers, body.clone()));
    let (content_type, data) = if body["stream"] == true { ("text/event-stream", response_sse(mock.format, mock.tool)) } else { ("application/json", response_json(mock.format, mock.tool).to_string()) };
    // Small chunks split JSON and UTF-8 too; SSE parsing must not depend on TCP boundaries.
    let chunks: Vec<_> = data.as_bytes().chunks(7).map(|chunk| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(chunk))).collect();
    Response::builder().header("content-type", content_type).body(Body::from_stream(futures::stream::iter(chunks))).unwrap()
}

fn check_upstream(format: Format, headers: &HeaderMap, body: &Value, scenario: &str, call_id: &str) -> Result<(), String> {
    if body["model"] != MODEL { return Err(format!("alias not mapped: {body}")); }
    let serialized = body.to_string();
    if serialized.contains("matrix-local-key") || headers.values().any(|v| v == "matrix-local-key" || v == "Bearer matrix-local-key") { return Err("local key leaked upstream".into()); }
    if !serialized.contains("matrix system") { return Err(format!("system instruction lost: {body}")); }
    let (input, tokens, tool_name, parameters) = match format {
        Format::Chat => (&body["messages"], body.get("max_completion_tokens").or(body.get("max_tokens")), &body["tools"][0]["function"]["name"], &body["tools"][0]["function"]["parameters"]),
        Format::Responses => (&body["input"], body.get("max_output_tokens"), &body["tools"][0]["name"], &body["tools"][0]["parameters"]),
        Format::Anthropic => (&body["messages"], body.get("max_tokens"), &body["tools"][0]["name"], &body["tools"][0]["input_schema"]),
    };
    if !input.is_array() { return Err(format!("incorrect upstream protocol: {body}")); }
    if tokens.and_then(Value::as_u64) != Some(128) { return Err(format!("output token budget lost: {body}")); }
    if scenario != "text" && (tool_name != "weather" || parameters["properties"]["city"]["type"] != "string") { return Err(format!("tool definition lost: {body}")); }
    if scenario == "result" {
        let valid = match format {
            Format::Chat => input.as_array().unwrap().iter().any(|v| v["role"] == "tool" && v["tool_call_id"] == call_id && v["content"] == "matrix tool result"),
            Format::Responses => input.as_array().unwrap().iter().any(|v| v["type"] == "function_call_output" && v["call_id"] == call_id && v["output"] == "matrix tool result"),
            Format::Anthropic => input.as_array().unwrap().iter().filter_map(|v| v["content"].as_array()).flatten().any(|v| v["type"] == "tool_result" && v["tool_use_id"] == call_id && v["content"] == "matrix tool result"),
        };
        if !valid { return Err(format!("tool result/correlation lost: {body}")); }
    }
    Ok(())
}

fn check_response(format: Format, stream: bool, tool: bool, wire: &str, call_id: &mut String) -> Result<(), String> {
    let values: Vec<Value> = if stream {
        wire.lines().filter_map(|line| line.strip_prefix("data:")).map(str::trim).filter(|line| *line != "[DONE]").map(serde_json::from_str).collect::<Result<_, _>>().map_err(|e| e.to_string())?
    } else { vec![serde_json::from_str(wire).map_err(|e| e.to_string())?] };
    let mut text = String::new(); let mut args = String::new(); let mut name = String::new(); let mut id = String::new(); let mut terminal = 0;
    let mut input_tokens = 0; let mut output_tokens = 0;
    for value in &values {
        match (format, stream) {
            (Format::Chat, _) => {
                let msg = if stream { &value["choices"][0]["delta"] } else { &value["choices"][0]["message"] };
                text.push_str(msg["content"].as_str().unwrap_or(""));
                if let Some(calls) = msg["tool_calls"].as_array() { for call in calls {
                    if let Some(v) = call["id"].as_str() { id = v.into(); }
                    if let Some(v) = call["function"]["name"].as_str() { name = v.into(); }
                    args.push_str(call["function"]["arguments"].as_str().unwrap_or(""));
                }}
                if value["choices"][0]["finish_reason"].is_string() { terminal += 1; }
                input_tokens = input_tokens.max(value["usage"]["prompt_tokens"].as_u64().unwrap_or(0));
                output_tokens = output_tokens.max(value["usage"]["completion_tokens"].as_u64().unwrap_or(0));
            }
            (Format::Responses, _) => {
                let response = if stream { &value["response"] } else { value };
                if !stream || value["type"] == "response.completed" {
                    terminal += 1;
                    if let Some(items) = response["output"].as_array() { for item in items {
                        if item["type"] == "function_call" { id = item["call_id"].as_str().unwrap_or("").into(); name = item["name"].as_str().unwrap_or("").into(); args = item["arguments"].as_str().unwrap_or("").into(); }
                        if !stream { if let Some(parts) = item["content"].as_array() { for part in parts { text.push_str(part["text"].as_str().unwrap_or("")); } } }
                    }}
                    input_tokens = response["usage"]["input_tokens"].as_u64().unwrap_or(0); output_tokens = response["usage"]["output_tokens"].as_u64().unwrap_or(0);
                }
                if stream && value["type"] == "response.output_text.delta" { text.push_str(value["delta"].as_str().unwrap_or("")); }
            }
            (Format::Anthropic, false) => {
                for part in value["content"].as_array().ok_or("missing content")? {
                    text.push_str(part["text"].as_str().unwrap_or(""));
                    if part["type"] == "tool_use" { id = part["id"].as_str().unwrap_or("").into(); name = part["name"].as_str().unwrap_or("").into(); args = part["input"].to_string(); }
                }
                terminal += usize::from(value["stop_reason"].is_string());
                input_tokens = value["usage"]["input_tokens"].as_u64().unwrap_or(0); output_tokens = value["usage"]["output_tokens"].as_u64().unwrap_or(0);
            }
            (Format::Anthropic, true) => {
                if value["type"] == "message_start" { input_tokens = value["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0); }
                if value["type"] == "message_delta" { input_tokens = input_tokens.max(value["usage"]["input_tokens"].as_u64().unwrap_or(0)); output_tokens = value["usage"]["output_tokens"].as_u64().unwrap_or(0); }
                if value["type"] == "message_stop" { terminal += 1; }
                let part = &value["content_block"];
                if part["type"] == "tool_use" { id = part["id"].as_str().unwrap_or("").into(); name = part["name"].as_str().unwrap_or("").into(); }
                text.push_str(value["delta"]["text"].as_str().unwrap_or(""));
                args.push_str(value["delta"]["partial_json"].as_str().unwrap_or(""));
            }
        }
    }
    if terminal != 1 { return Err(format!("expected exactly one terminal, got {terminal}: {wire}")); }
    if tool {
        if id.is_empty() || name != "weather" || serde_json::from_str::<Value>(&args).ok() != Some(json!({"city":"杭州"})) { return Err(format!("tool call corrupted: id={id}, name={name}, args={args}; {wire}")); }
        *call_id = id;
    } else if text != TEXT { return Err(format!("text missing/duplicated: {text:?}; {wire}")); }
    if input_tokens != 4 || output_tokens != 5 { return Err(format!("usage lost: {input_tokens}/{output_tokens}; {wire}")); }
    if stream && matches!(format, Format::Chat) && wire.lines().filter(|l| *l == "data: [DONE]").count() != 1 { return Err(format!("missing/duplicate DONE: {wire}")); }
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn gateway_protocol_matrix_over_real_http() {
    let mut failures = Vec::new();
    for up in FORMATS { for down in FORMATS { for stream in [false, true] { let mut call_id = CALL.to_string(); for scenario in ["text", "tool", "result"] {
        let label = format!("{down:?} -> {up:?}, stream={stream}, {scenario}");
        let mock = Mock { format: up, tool: scenario == "tool", seen: Arc::new(Mutex::new(Vec::new())) };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(up.path(), post(upstream)).with_state(mock.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let db = Arc::new(Database::memory().unwrap());
        let provider: GatewayProvider = serde_json::from_value(json!({"id":"matrix","name":"matrix","baseUrl":format!("http://{addr}"),"apiKey":"matrix-upstream-key","models":[{"alias":ALIAS,"upstreamModel":MODEL,"apiFormat":up.gateway()}]})).unwrap();
        let config = GatewayConfig { local_api_key: "matrix-local-key".into(), enable_logging: false, providers: vec![provider], ..Default::default() };
        db.set_setting(CONFIG_KEY, &serde_json::to_string(&config).unwrap()).unwrap();
        sync_generated_providers(&db, &config).unwrap();
        let server = GatewayRuntime::new(db);
        server.update_config(&ProxyConfig { listen_port: 0, enable_logging: false, ..Default::default() }).await.unwrap();
        let info = server.start().await.unwrap();
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(15)).build().unwrap();
        let mut payload = request(down, stream, scenario);
        replace_call_id(&mut payload, &call_id);
        let outcome = async {
            let response = client.post(format!("http://127.0.0.1:{}{}", info.port, down.path())).bearer_auth("matrix-local-key").header("anthropic-version", "2023-06-01").json(&payload).send().await.map_err(|e| e.to_string())?;
            let status = response.status();
            let wire = response.text().await.map_err(|e| e.to_string())?;
            if status != StatusCode::OK { return Err(format!("HTTP {status}: {wire}")); }
            let seen = mock.seen.lock().unwrap();
            if seen.len() != 1 { return Err(format!("expected one upstream request, got {}", seen.len())); }
            check_upstream(up, &seen[0].0, &seen[0].1, scenario, &call_id)?;
            check_response(down, stream, scenario == "tool", &wire, &mut call_id)
        }.await;
        server.stop().await.unwrap(); task.abort(); let _ = task.await;
        if let Err(error) = outcome { failures.push(format!("{label}: {error}")); } else { println!("PASS {label}"); }
    }}}}
    assert!(failures.is_empty(), "{} of 54 combinations failed:\n{}", failures.len(), failures.join("\n\n"));
}

fn replace_call_id(value: &mut Value, id: &str) {
    match value {
        Value::String(s) if s == CALL => *s = id.to_string(),
        Value::Array(values) => values.iter_mut().for_each(|v| replace_call_id(v, id)),
        Value::Object(values) => values.values_mut().for_each(|v| replace_call_id(v, id)),
        _ => {}
    }
}
