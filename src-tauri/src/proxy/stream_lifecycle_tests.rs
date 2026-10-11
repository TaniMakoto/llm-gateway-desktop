//! Regression fixtures for the same lifecycle rules across conversion directions.
//! No credentials, database files, or real provider traffic are used.
use super::providers::{
    streaming, streaming_codex_anthropic, streaming_codex_chat, streaming_responses,
    transform_codex_chat,
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

fn source(
    input: &str,
    chunk_size: usize,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let chunks: Vec<_> = input
        .as_bytes()
        .chunks(chunk_size)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    futures::stream::iter(chunks)
}

async fn wire(stream: impl Stream<Item = Result<Bytes, std::io::Error>>) -> String {
    let chunks = stream.map(|chunk| chunk.unwrap()).collect::<Vec<_>>().await;
    String::from_utf8(chunks.concat()).unwrap()
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn chat_tool(arguments: &str, finish: Option<&str>, done: bool) -> String {
    let mut input = event(
        json!({"id":"chatcmpl_fixture","model":"fixture","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"lookup","arguments":arguments}}]},"finish_reason":null}]}),
    );
    if let Some(finish) = finish {
        input.push_str(&event(
            json!({"choices":[{"delta":{},"finish_reason":finish}]}),
        ));
    }
    if done {
        input.push_str("data: [DONE]\n\n");
    }
    input
}

#[tokio::test]
async fn incomplete_function_calls_never_become_completed_in_either_chat_bridge() {
    for arguments in ["", "{", "null", "[]", "{} trailing"] {
        for finish in [None, Some("tool_calls"), Some("length")] {
            for done in [false, true] {
                let input = chat_tool(arguments, finish, done);
                let responses = wire(streaming_codex_chat::create_responses_sse_stream_from_chat(
                    source(&input, 1),
                ))
                .await;
                assert!(
                    responses.contains("event: response.failed"),
                    "{arguments:?} {finish:?} {done}: {responses}"
                );
                assert!(!responses.contains("event: response.completed"));
                assert!(!responses.contains("event: response.function_call_arguments.done"));
                let messages =
                    wire(streaming::create_anthropic_sse_stream(source(&input, 7))).await;
                assert!(messages.contains("event: error"));
                assert!(!messages.contains("event: message_stop"));
                assert!(!messages.contains("event: content_block_stop"));
            }
        }
    }
}

#[tokio::test]
async fn complete_chat_tools_keep_utf8_usage_tail_and_a_single_completion() {
    let mut input = chat_tool("{\"城市\":\"北京\"}", Some("tool_calls"), false);
    input.push_str(&event(
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}),
    ));
    input.push_str("data: [DONE]\n\ndata: [DONE]\n\n");
    for chunk_size in [1, 7, input.len()] {
        let responses = wire(streaming_codex_chat::create_responses_sse_stream_from_chat(
            source(&input, chunk_size),
        ))
        .await;
        assert_eq!(responses.matches("event: response.completed").count(), 1);
        assert_eq!(
            responses
                .matches("event: response.function_call_arguments.done")
                .count(),
            1
        );
        assert!(responses.contains("北京") && responses.contains("\"output_tokens\":7"));
        assert!(!responses.contains("response.failed") && !responses.contains('\u{fffd}'));
        let messages = wire(streaming::create_anthropic_sse_stream(source(
            &input, chunk_size,
        )))
        .await;
        assert_eq!(messages.matches("event: message_stop").count(), 1);
        assert!(messages.contains("北京") && messages.contains("\"output_tokens\":7"));
    }
}

#[tokio::test]
async fn custom_tools_keep_text_instead_of_becoming_json_objects() {
    let context = transform_codex_chat::build_codex_tool_context_from_request(
        &json!({"tools":[{"type":"custom","name":"lookup","format":{"type":"text"}}]}),
    );
    let input = chat_tool("ls -la && echo 文本", Some("tool_calls"), true);
    let output = wire(
        streaming_codex_chat::create_responses_sse_stream_from_chat_with_context(
            source(&input, 1),
            context,
        ),
    )
    .await;
    assert!(output.contains("response.custom_tool_call_input.done"));
    assert!(output.contains("ls -la && echo 文本"));
    assert!(!output.contains("event: response.failed"));
}

#[tokio::test]
async fn explicit_length_is_incomplete_but_unexpected_text_eof_is_failed() {
    let text = event(json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}));
    let unexpected = wire(streaming_codex_chat::create_responses_sse_stream_from_chat(
        source(&text, 1),
    ))
    .await;
    assert!(unexpected.contains("partial") && unexpected.contains("event: response.failed"));
    assert!(!unexpected.contains("max_output_tokens"));
    let limited = format!(
        "{}{}data: [DONE]\n\n",
        text,
        event(json!({"choices":[{"delta":{},"finish_reason":"length"}]}))
    );
    let complete = wire(streaming_codex_chat::create_responses_sse_stream_from_chat(
        source(&limited, 1),
    ))
    .await;
    assert!(
        complete.contains("\"status\":\"incomplete\"") && complete.contains("max_output_tokens")
    );
    assert!(!complete.contains("event: response.failed"));
}

#[tokio::test]
async fn responses_function_done_and_terminal_both_validate_streamed_arguments() {
    for close in [
        "response.function_call_arguments.done",
        "response.output_item.done",
        "response.completed",
        "response.incomplete",
    ] {
        for arguments in ["", "{", "[]", "null"] {
            let mut input = event(
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"lookup","arguments":""}}),
            );
            if !arguments.is_empty() {
                input.push_str(&event(json!({"type":"response.function_call_arguments.delta","item_id":"fc_fixture","delta":arguments})));
            }
            let closing = match close {
                "response.function_call_arguments.done" => {
                    json!({"type":close,"item_id":"fc_fixture","arguments":arguments})
                }
                "response.output_item.done" => {
                    json!({"type":close,"item":{"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"lookup","arguments":arguments}})
                }
                _ => {
                    json!({"type":close,"response":{"status":if close == "response.incomplete" {"incomplete"} else {"completed"},"output":[]}})
                }
            };
            input.push_str(&event(closing));
            input.push_str(&event(
                json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
            ));
            let output = wire(
                streaming_responses::create_anthropic_sse_stream_from_responses(source(&input, 1)),
            )
            .await;
            assert!(
                output.contains("event: error") && output.contains("incomplete_tool_call"),
                "{close} {arguments:?}: {output}"
            );
            assert!(!output.contains("event: message_stop"));
            assert!(!output.contains("event: content_block_stop"));
        }
    }
}

#[tokio::test]
async fn anthropic_function_blocks_validate_before_output_item_done() {
    for close in ["content_block_stop", "message_stop"] {
        let input = [
            event(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_fixture","name":"lookup","input":{}}})),
            event(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}})),
            event(json!({"type":close,"index":0})),
        ].concat();
        let output = wire(
            streaming_codex_anthropic::create_responses_sse_stream_from_anthropic(source(
                &input, 1,
            )),
        )
        .await;
        assert!(output.contains("event: response.failed"));
        assert!(!output.contains("event: response.completed"));
        assert!(!output.contains("event: response.function_call_arguments.done"));
    }
}

#[tokio::test]
async fn late_errors_survive_completed_events_across_bridges() {
    let chat = format!(
        "{}{}",
        chat_tool("{}", Some("tool_calls"), true),
        event(json!({"error":{"message":"late failure"}}))
    );
    let chat_responses = wire(streaming_codex_chat::create_responses_sse_stream_from_chat(
        source(&chat, 1),
    ))
    .await;
    assert!(
        chat_responses.contains("event: response.failed")
            && chat_responses.contains("late failure")
    );
    let chat_messages = wire(streaming::create_anthropic_sse_stream(source(&chat, 1))).await;
    assert!(chat_messages.contains("event: error") && chat_messages.contains("late failure"));
    let responses = [
        event(json!({"type":"response.completed","response":{"status":"completed"}})),
        event(json!({"type":"error","error":{"message":"late failure"}})),
    ]
    .concat();
    let responses_messages = wire(
        streaming_responses::create_anthropic_sse_stream_from_responses(source(&responses, 1)),
    )
    .await;
    assert!(
        responses_messages.contains("event: error") && responses_messages.contains("late failure")
    );
    let messages = [
        event(json!({"type":"message_stop"})),
        event(json!({"type":"error","error":{"message":"late failure"}})),
    ]
    .concat();
    let messages_responses = wire(
        streaming_codex_anthropic::create_responses_sse_stream_from_anthropic(source(&messages, 1)),
    )
    .await;
    assert!(
        messages_responses.contains("event: response.failed")
            && messages_responses.contains("late failure")
    );
}
