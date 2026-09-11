#[inline]
pub(crate) fn strip_sse_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    line.strip_prefix(&format!("{field}: "))
        .or_else(|| line.strip_prefix(&format!("{field}:")))
}

#[inline]
pub(crate) fn take_sse_block(buffer: &mut String) -> Option<String> {
    let mut best: Option<(usize, usize)> = None;

    for (delimiter, len) in [("\r\n\r\n", 4usize), ("\n\n", 2usize)] {
        if let Some(pos) = buffer.find(delimiter) {
            if best.is_none_or(|(best_pos, _)| pos < best_pos) {
                best = Some((pos, len));
            }
        }
    }

    let (pos, len) = best?;
    let block = buffer[..pos].to_string();
    buffer.drain(..pos + len);
    Some(block)
}

/// Append raw bytes to a UTF-8 `String` buffer, correctly handling multi-byte
/// characters that are split across chunk boundaries.
///
/// `remainder` accumulates trailing bytes from the previous chunk that form an
/// incomplete UTF-8 sequence (at most 3 bytes under normal operation). On each
/// call the remainder is prepended to `new_bytes`, the longest valid UTF-8
/// prefix is appended to `buffer`, and any trailing incomplete bytes are saved
/// back into `remainder` for the next call.
///
/// A defensive guard discards `remainder` via lossy conversion if it ever
/// exceeds 3 bytes, which cannot happen with well-formed UTF-8 streams.
pub(crate) fn append_utf8_safe(buffer: &mut String, remainder: &mut Vec<u8>, new_bytes: &[u8]) {
    // Build the byte slice to decode: prepend any leftover bytes from previous chunk.
    let (owned, bytes): (Option<Vec<u8>>, &[u8]) = if remainder.is_empty() {
        (None, new_bytes)
    } else {
        // Defensive guard: remainder should never exceed 3 bytes (max incomplete
        // UTF-8 sequence is 3 bytes: a 4-byte char missing its last byte). If it
        // does, the stream is producing genuinely invalid bytes; flush them lossy
        // and start fresh.
        if remainder.len() > 3 {
            buffer.push_str(&String::from_utf8_lossy(remainder));
            remainder.clear();
            (None, new_bytes)
        } else {
            let mut combined = std::mem::take(remainder);
            combined.extend_from_slice(new_bytes);
            (Some(combined), &[])
        }
    };
    let input = owned.as_deref().unwrap_or(bytes);

    // Decode loop: consume all valid UTF-8 and any genuinely invalid bytes,
    // only leaving a trailing incomplete sequence in remainder.
    let mut pos = 0;
    loop {
        match std::str::from_utf8(&input[pos..]) {
            Ok(s) => {
                buffer.push_str(s);
                // Everything consumed – remainder stays empty.
                return;
            }
            Err(e) => {
                let valid_up_to = pos + e.valid_up_to();
                let valid_slice = &input[pos..valid_up_to];
                match std::str::from_utf8(valid_slice) {
                    Ok(valid) => buffer.push_str(valid),
                    Err(_) => buffer.push_str(&String::from_utf8_lossy(valid_slice)),
                }
                if let Some(invalid_len) = e.error_len() {
                    // Genuinely invalid byte(s) – emit U+FFFD and continue.
                    buffer.push('\u{FFFD}');
                    pos = valid_up_to + invalid_len;
                } else {
                    // Incomplete trailing sequence – stash for next chunk.
                    *remainder = input[valid_up_to..].to_vec();
                    return;
                }
            }
        }
    }
}

/// 客户端侧 SSE 协议。**仅用于诊断日志**：识别该协议"正常结束"与"失败"标记的形态。
///
/// 透传层只做字节级转发、不解析各协议的完整状态机，因此这里只保留判定终态所需的
/// 最小信息，不参与任何转发决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientSseProtocol {
    /// Anthropic Messages SSE（Claude / Claude Desktop 客户端）
    Anthropic,
    /// OpenAI Responses SSE（Codex 客户端）
    Responses,
    /// OpenAI Chat Completions / Gemini 的 data-only SSE
    Chat,
}

impl ClientSseProtocol {
    /// 协议名，用于断流日志。
    pub(crate) fn name(self) -> &'static str {
        match self {
            ClientSseProtocol::Anthropic => "Anthropic",
            ClientSseProtocol::Responses => "Responses",
            ClientSseProtocol::Chat => "Chat",
        }
    }

    /// 便宜的预筛：该事件块是否**可能**携带终态标记。
    ///
    /// 只是为了免去对每个事件都做一次 JSON 解析。真正的判定走 [`Self::classify`]，
    /// 所以工具参数/正文里恰好出现 `stop_reason` 这类字样不会被误判成已收尾。
    fn may_carry_terminal(self, block: &str) -> bool {
        match self {
            ClientSseProtocol::Anthropic => {
                block.contains("message_stop") || block.contains("stop_reason")
            }
            ClientSseProtocol::Responses => {
                block.contains("response.completed")
                    || block.contains("response.incomplete")
                    || block.contains("response.failed")
            }
            ClientSseProtocol::Chat => {
                block.contains("[DONE]")
                    || block.contains("finish_reason")
                    || block.contains("error")
            }
        }
    }

    /// 按事件 JSON 的 `type` / 字段判定终态；`None` 表示该事件不是终态标记。
    fn classify(self, event: &serde_json::Value) -> Option<SseTerminalState> {
        let event_type = event.get("type").and_then(serde_json::Value::as_str);
        match self {
            ClientSseProtocol::Anthropic => match event_type {
                Some("message_stop") => Some(SseTerminalState::Completed),
                // message_delta 的 stop_reason 非 null 才算收尾（null 表示流还在继续）
                Some("message_delta") => event
                    .pointer("/delta/stop_reason")
                    .filter(|value| !value.is_null())
                    .map(|_| SseTerminalState::Completed),
                Some("error") => Some(SseTerminalState::Failed),
                _ => None,
            },
            ClientSseProtocol::Responses => match event_type {
                // response.incomplete 是 token 上限截断，仍属正常收尾
                Some("response.completed" | "response.incomplete") => {
                    Some(SseTerminalState::Completed)
                }
                Some("response.failed" | "error") => Some(SseTerminalState::Failed),
                _ => None,
            },
            ClientSseProtocol::Chat => {
                if event_type == Some("error")
                    || event.get("error").is_some_and(|value| !value.is_null())
                {
                    return Some(SseTerminalState::Failed);
                }
                let finished = event
                    .get("choices")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice.get("finish_reason").is_some_and(|value| !value.is_null())
                        })
                    });
                finished.then_some(SseTerminalState::Completed)
            }
        }
    }
}

/// 客户端侧 SSE 流已观察到的终态。仅用于日志，不影响转发行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseTerminalState {
    /// 尚未见到任何协议终态标记（流被中途掐断时停在这里）
    Pending,
    /// 已见到正常结束标记（message_stop / [DONE] / response.completed ...）
    Completed,
    /// 已见到失败终态（event: error / response.failed），上游已自行报错
    Failed,
}

/// 统计客户端侧 SSE 流是否已经出现过本协议的终态标记。
///
/// 调用方必须喂入**完整的 SSE 事件块**（以空行分隔，见 [`take_sse_block`]），
/// 这样标记不会被 TCP 分片切断，无需额外的跨块缓冲。
#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalMarkerScanner {
    protocol: ClientSseProtocol,
    state: SseTerminalState,
}

impl TerminalMarkerScanner {
    pub(crate) fn new(protocol: ClientSseProtocol) -> Self {
        Self {
            protocol,
            state: SseTerminalState::Pending,
        }
    }

    /// 已观察到的终态。`Pending` 表示流结束时仍未见到任何协议终态标记。
    pub(crate) fn state(&self) -> SseTerminalState {
        self.state
    }

    /// 喂入一个完整的 SSE 事件块，返回当前已观察到的终态（首个命中的标记生效）。
    pub(crate) fn push(&mut self, block: &str) -> SseTerminalState {
        if self.state != SseTerminalState::Pending {
            return self.state;
        }
        if !self.protocol.may_carry_terminal(block) {
            return self.state;
        }
        for payload in block.lines().filter_map(|line| strip_sse_field(line, "data")) {
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            // Chat 的结束标记是字面量 [DONE]，不是 JSON。
            if self.protocol == ClientSseProtocol::Chat && payload == "[DONE]" {
                self.state = SseTerminalState::Completed;
                return self.state;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if let Some(state) = self.protocol.classify(&event) {
                self.state = state;
                return self.state;
            }
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_utf8_safe, strip_sse_field, take_sse_block, ClientSseProtocol, SseTerminalState,
        TerminalMarkerScanner,
    };

    // ------------------------------------------------------------------
    // 终态判定（只用于诊断日志）
    // ------------------------------------------------------------------

    #[test]
    fn terminal_scanner_accepts_anthropic_message_stop() {
        let mut scanner = TerminalMarkerScanner::new(ClientSseProtocol::Anthropic);
        assert_eq!(
            scanner.push("event: message_start\ndata: {\"type\":\"message_start\"}"),
            SseTerminalState::Pending
        );
        assert_eq!(
            scanner.push("event: message_stop\ndata: {\"type\":\"message_stop\"}"),
            SseTerminalState::Completed
        );
    }

    #[test]
    fn terminal_scanner_accepts_anthropic_non_null_stop_reason() {
        let mut scanner = TerminalMarkerScanner::new(ClientSseProtocol::Anthropic);
        assert_eq!(
            scanner.push(
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}"
            ),
            SseTerminalState::Completed
        );
    }

    #[test]
    fn terminal_scanner_rejects_null_stop_reason() {
        let mut scanner = TerminalMarkerScanner::new(ClientSseProtocol::Anthropic);
        assert_eq!(
            scanner.push(
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null}}"
            ),
            SseTerminalState::Pending
        );
    }

    /// 关键回归：终态标记出现在**工具参数/正文文本里**时不能被误判成已收尾。
    /// 这正是裸字节子串匹配会出错、而按事件 JSON 判定不会出错的地方。
    #[test]
    fn terminal_scanner_ignores_marker_text_inside_tool_arguments() {
        let mut scanner = TerminalMarkerScanner::new(ClientSseProtocol::Anthropic);
        assert_eq!(
            scanner.push(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pattern\\\":\\\"stop_reason\\\"}\"}}"
            ),
            SseTerminalState::Pending
        );

        let mut chat = TerminalMarkerScanner::new(ClientSseProtocol::Chat);
        assert_eq!(
            chat.push(
                "data: {\"choices\":[{\"delta\":{\"content\":\"finish_reason appears in prose\"},\"finish_reason\":null}]}"
            ),
            SseTerminalState::Pending
        );
    }

    #[test]
    fn terminal_scanner_chat_requires_done_or_non_null_finish_reason() {
        let mut chat = TerminalMarkerScanner::new(ClientSseProtocol::Chat);
        assert_eq!(
            chat.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":null}]}"),
            SseTerminalState::Pending
        );
        // 上游终止流但省略 [DONE] 时，非 null 的 finish_reason 同样算正常收尾。
        assert_eq!(
            chat.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}"),
            SseTerminalState::Completed
        );

        let mut done = TerminalMarkerScanner::new(ClientSseProtocol::Chat);
        assert_eq!(done.push("data: [DONE]"), SseTerminalState::Completed);
    }

    #[test]
    fn terminal_scanner_responses_failed_is_terminal_but_not_completed() {
        let mut scanner = TerminalMarkerScanner::new(ClientSseProtocol::Responses);
        assert_eq!(
            scanner.push("event: response.created\ndata: {\"type\":\"response.created\"}"),
            SseTerminalState::Pending
        );
        assert_eq!(
            scanner.push(
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}"
            ),
            SseTerminalState::Failed
        );
    }

    #[test]
    fn terminal_scanner_treats_error_event_as_failed_terminal() {
        for protocol in [
            ClientSseProtocol::Anthropic,
            ClientSseProtocol::Responses,
            ClientSseProtocol::Chat,
        ] {
            let mut scanner = TerminalMarkerScanner::new(protocol);
            assert_eq!(
                scanner.push("event: error\ndata: {\"type\":\"error\"}"),
                SseTerminalState::Failed
            );
        }
        // Chat 形状的错误体（error.rs 的 {"error":{...}}）
        let mut chat = TerminalMarkerScanner::new(ClientSseProtocol::Chat);
        assert_eq!(
            chat.push("data: {\"error\":{\"message\":\"boom\",\"type\":\"stream_error\"}}"),
            SseTerminalState::Failed
        );
    }

    #[test]
    fn terminal_scanner_keeps_first_terminal_marker() {
        let mut scanner = TerminalMarkerScanner::new(ClientSseProtocol::Responses);
        assert_eq!(
            scanner.push("event: response.completed\ndata: {\"type\":\"response.completed\"}"),
            SseTerminalState::Completed
        );
        assert_eq!(
            scanner.push("event: error\ndata: {\"type\":\"error\"}"),
            SseTerminalState::Completed
        );
    }

    #[test]
    fn strip_sse_field_accepts_optional_space() {
        assert_eq!(
            strip_sse_field("data: {\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("event: message_start", "event"),
            Some("message_start")
        );
        assert_eq!(
            strip_sse_field("event:message_start", "event"),
            Some("message_start")
        );
        assert_eq!(strip_sse_field("id:1", "data"), None);
    }

    #[test]
    fn take_sse_block_supports_lf_delimiters() {
        let mut buffer = "data: {\"ok\":true}\n\nrest".to_string();

        assert_eq!(
            take_sse_block(&mut buffer),
            Some("data: {\"ok\":true}".to_string())
        );
        assert_eq!(buffer, "rest");
    }

    #[test]
    fn take_sse_block_supports_crlf_delimiters() {
        let mut buffer = "data: {\"ok\":true}\r\n\r\nrest".to_string();

        assert_eq!(
            take_sse_block(&mut buffer),
            Some("data: {\"ok\":true}".to_string())
        );
        assert_eq!(buffer, "rest");
    }

    // ------------------------------------------------------------------
    // append_utf8_safe tests
    // ------------------------------------------------------------------

    #[test]
    fn ascii_passthrough() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"hello world");
        assert_eq!(buf, "hello world");
        assert!(rem.is_empty());
    }

    #[test]
    fn complete_multibyte_in_single_chunk() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, "你好世界".as_bytes());
        assert_eq!(buf, "你好世界");
        assert!(rem.is_empty());
    }

    #[test]
    fn split_multibyte_across_two_chunks() {
        // "你" = E4 BD A0 (3 bytes)
        let bytes = "你".as_bytes();
        assert_eq!(bytes.len(), 3);

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Chunk 1: first 2 bytes (incomplete)
        append_utf8_safe(&mut buf, &mut rem, &bytes[..2]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 2);

        // Chunk 2: last byte completes the character
        append_utf8_safe(&mut buf, &mut rem, &bytes[2..]);
        assert_eq!(buf, "你");
        assert!(rem.is_empty());
    }

    #[test]
    fn split_four_byte_char_across_chunks() {
        // 😀 = F0 9F 98 80 (4 bytes)
        let bytes = "😀".as_bytes();
        assert_eq!(bytes.len(), 4);

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Send 1 byte at a time
        append_utf8_safe(&mut buf, &mut rem, &bytes[..1]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 1);

        append_utf8_safe(&mut buf, &mut rem, &bytes[1..2]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 2);

        append_utf8_safe(&mut buf, &mut rem, &bytes[2..3]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 3);

        append_utf8_safe(&mut buf, &mut rem, &bytes[3..]);
        assert_eq!(buf, "😀");
        assert!(rem.is_empty());
    }

    #[test]
    fn mixed_ascii_and_split_multibyte() {
        // "hi你" = 68 69 E4 BD A0
        let all = "hi你".as_bytes();
        assert_eq!(all.len(), 5);

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Chunk 1: "hi" + first byte of "你"
        append_utf8_safe(&mut buf, &mut rem, &all[..3]);
        assert_eq!(buf, "hi");
        assert_eq!(rem.len(), 1);

        // Chunk 2: remaining 2 bytes of "你"
        append_utf8_safe(&mut buf, &mut rem, &all[3..]);
        assert_eq!(buf, "hi你");
        assert!(rem.is_empty());
    }

    #[test]
    fn multiple_split_characters_in_sequence() {
        let text = "你好";
        let bytes = text.as_bytes(); // E4 BD A0 E5 A5 BD

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Split in the middle: first char complete + 1 byte of second
        append_utf8_safe(&mut buf, &mut rem, &bytes[..4]);
        assert_eq!(buf, "你");
        assert_eq!(rem.len(), 1);

        // Remaining 2 bytes complete second char
        append_utf8_safe(&mut buf, &mut rem, &bytes[4..]);
        assert_eq!(buf, "你好");
        assert!(rem.is_empty());
    }

    #[test]
    fn empty_chunks_are_harmless() {
        let mut buf = String::new();
        let mut rem = Vec::new();

        append_utf8_safe(&mut buf, &mut rem, b"");
        assert_eq!(buf, "");
        assert!(rem.is_empty());

        append_utf8_safe(&mut buf, &mut rem, b"ok");
        assert_eq!(buf, "ok");

        append_utf8_safe(&mut buf, &mut rem, b"");
        assert_eq!(buf, "ok");
    }

    #[test]
    fn sse_json_with_chinese_split_at_boundary() {
        // Simulates an SSE data line with Chinese content split across chunks
        let json_line = "data: {\"text\":\"你好\"}\n\n";
        let bytes = json_line.as_bytes();

        // Find where "你" starts in the byte stream and split there
        let ni_start = bytes.windows(3).position(|w| w == "你".as_bytes()).unwrap();
        let split_point = ni_start + 1; // split inside "你"

        let mut buf = String::new();
        let mut rem = Vec::new();

        append_utf8_safe(&mut buf, &mut rem, &bytes[..split_point]);
        append_utf8_safe(&mut buf, &mut rem, &bytes[split_point..]);

        assert_eq!(buf, json_line);
        assert!(rem.is_empty());

        // Verify the buffer can be parsed as SSE with valid JSON
        let data = strip_sse_field(buf.lines().next().unwrap(), "data").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["text"], "你好");
    }

    #[test]
    fn invalid_bytes_flushed_immediately_not_accumulated() {
        // 0xFF is never valid in UTF-8 – it should be replaced immediately,
        // not stashed in remainder.
        let mut buf = String::new();
        let mut rem = Vec::new();

        // "hi" + invalid byte + "ok"
        append_utf8_safe(&mut buf, &mut rem, b"hi\xFFok");
        assert!(
            rem.is_empty(),
            "remainder should be empty after invalid byte"
        );
        assert!(buf.contains("hi"), "valid prefix must be present");
        assert!(buf.contains("ok"), "valid suffix must be present");
        assert!(buf.contains('\u{FFFD}'), "invalid byte must produce U+FFFD");
    }

    #[test]
    fn invalid_byte_in_slow_path_flushed_immediately() {
        let mut buf = String::new();
        let mut rem = Vec::new();

        // Prime remainder with an incomplete sequence (first byte of "你")
        append_utf8_safe(&mut buf, &mut rem, &"你".as_bytes()[..1]);
        assert_eq!(rem.len(), 1);

        // Next chunk starts with an invalid byte – the stale remainder and the
        // invalid byte should both be flushed, not accumulated.
        append_utf8_safe(&mut buf, &mut rem, b"\xFFworld");
        assert!(rem.is_empty(), "remainder should be empty");
        assert!(
            buf.contains("world"),
            "valid data after invalid byte must appear"
        );
    }

    #[test]
    fn defensive_guard_flushes_oversized_remainder() {
        let mut buf = String::new();
        let mut rem = Vec::new();

        // Manually inject 4 invalid bytes into remainder to trigger the >3 guard.
        // This can't happen with well-formed UTF-8, but tests the safety net.
        rem.extend_from_slice(b"\x80\x80\x80\x80");
        assert_eq!(rem.len(), 4);

        append_utf8_safe(&mut buf, &mut rem, b"hello");
        // The 4 invalid bytes should have been flushed lossy, then "hello" decoded.
        assert!(rem.is_empty(), "remainder must be empty after guard flush");
        assert!(
            buf.contains("hello"),
            "valid data after guard flush must appear"
        );
        // The 4 invalid bytes each produce a U+FFFD
        let replacement_count = buf.chars().filter(|&c| c == '\u{FFFD}').count();
        assert_eq!(
            replacement_count, 4,
            "each invalid byte should produce one U+FFFD"
        );
    }
}
