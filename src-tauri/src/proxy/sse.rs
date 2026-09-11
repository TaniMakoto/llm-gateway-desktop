use bytes::Bytes;
use serde_json::json;

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

/// 客户端侧 SSE 协议：决定"正常结束标记"以及断流时回发给客户端的 error 事件形状。
///
/// 透传层（`create_logged_passthrough_stream`）只做字节级转发，不解析各协议的
/// 完整状态机，因此这里只按客户端协议区分最小必要的两种信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientSseProtocol {
    /// Anthropic Messages SSE（Claude / Claude Desktop 客户端）
    Anthropic,
    /// OpenAI Responses SSE（Codex 客户端）
    Responses,
    /// OpenAI Chat Completions / Gemini 的 data-only SSE
    Chat,
}

/// Anthropic 流的正常终态：`message_stop` 事件，或 message_delta 里非 null 的
/// `stop_reason`（部分上游/转换层在流末尾省略 message_stop，见 streaming.rs:684）。
const COMPLETED_ANTHROPIC: &[&str] = &["event: message_stop", r#""stop_reason":""#];
/// Anthropic 流的失败终态：上游/转换层已经报过错（streaming.rs:653），透传层不再重复补发。
const FAILED_ANTHROPIC: &[&str] = &["event: error"];

/// Responses 流的正常终态事件（`response.incomplete` 是 token 上限截断，仍属正常收尾）。
const COMPLETED_RESPONSES: &[&str] = &["event: response.completed", "event: response.incomplete"];
/// Responses 流的失败终态：转换层发现上游截断时发的也是 response.failed。
const FAILED_RESPONSES: &[&str] = &["event: response.failed", "event: error"];

/// Chat Completions 流的正常终态：`[DONE]` 或非 null 的 `finish_reason`。
const COMPLETED_CHAT: &[&str] = &["data: [DONE]", r#""finish_reason":""#];
/// Chat 流的失败终态。
const FAILED_CHAT: &[&str] = &["event: error"];

impl ClientSseProtocol {
    /// 协议名，用于断流日志。
    pub(crate) fn name(self) -> &'static str {
        match self {
            ClientSseProtocol::Anthropic => "Anthropic",
            ClientSseProtocol::Responses => "Responses",
            ClientSseProtocol::Chat => "Chat",
        }
    }

    fn completed_markers(self) -> &'static [&'static str] {
        match self {
            ClientSseProtocol::Anthropic => COMPLETED_ANTHROPIC,
            ClientSseProtocol::Responses => COMPLETED_RESPONSES,
            ClientSseProtocol::Chat => COMPLETED_CHAT,
        }
    }

    fn failed_markers(self) -> &'static [&'static str] {
        match self {
            ClientSseProtocol::Anthropic => FAILED_ANTHROPIC,
            ClientSseProtocol::Responses => FAILED_RESPONSES,
            ClientSseProtocol::Chat => FAILED_CHAT,
        }
    }

    /// 断流时补发给客户端的 SSE error 事件字节。
    ///
    /// 形状沿用仓库既有写法，不发明新 schema：
    /// - Anthropic / Responses：`event: error` + `{"type":"error","error":{...}}`
    ///   （同 streaming.rs:653 与 streaming_responses.rs:53 的 anthropic_error_sse）。
    /// - Chat（含 Gemini 的 data-only SSE）：`data: {"error":{...}}`，即 error.rs
    ///   里网关错误体的形状（部分 OpenAI 兼容网关也把错误当普通 data chunk 下发，
    ///   见 handlers.rs 对 `{"error":{...}}` data chunk 的识别）。
    pub(crate) fn error_event(self, message: &str) -> Bytes {
        match self {
            ClientSseProtocol::Anthropic | ClientSseProtocol::Responses => {
                let payload = json!({
                    "type": "error",
                    "error": {"type": "stream_error", "message": message}
                });
                Bytes::from(format!(
                    "event: error\ndata: {}\n\n",
                    serde_json::to_string(&payload).unwrap_or_default()
                ))
            }
            ClientSseProtocol::Chat => {
                let payload = json!({
                    "error": {"message": message, "type": "stream_error"}
                });
                Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&payload).unwrap_or_default()
                ))
            }
        }
    }
}

/// 客户端侧 SSE 流已观察到的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseTerminalState {
    /// 尚未见到任何协议终态标记（流被中途掐断时停在这里）
    Pending,
    /// 已见到正常结束标记（message_stop / [DONE] / response.completed ...）
    Completed,
    /// 已见到失败终态（event: error / response.failed），上游或转换层已自行报错
    Failed,
}

/// 检测客户端侧 SSE 流是否已经出现过本协议的终态标记。
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
        if self
            .protocol
            .completed_markers()
            .iter()
            .any(|m| block.contains(m))
        {
            self.state = SseTerminalState::Completed;
        } else if self
            .protocol
            .failed_markers()
            .iter()
            .any(|m| block.contains(m))
        {
            self.state = SseTerminalState::Failed;
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
    // 断流判定 / error 事件
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

    #[test]
    fn terminal_scanner_chat_requires_done_or_non_null_finish_reason() {
        let mut chat = TerminalMarkerScanner::new(ClientSseProtocol::Chat);
        assert_eq!(
            chat.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":null}]}"),
            SseTerminalState::Pending
        );
        assert_eq!(chat.push("data: [DONE]"), SseTerminalState::Completed);
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
    fn terminal_scanner_treats_upstream_error_event_as_failed_terminal() {
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
    }

    #[test]
    fn terminal_scanner_completed_wins_over_later_failed_marker() {
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
    fn error_event_shapes_match_existing_precedent() {
        // Anthropic：event: error + {"type":"error","error":{...}}（streaming.rs:653 同款）
        let anthropic =
            String::from_utf8(ClientSseProtocol::Anthropic.error_event("boom").to_vec()).unwrap();
        let data = anthropic
            .strip_prefix("event: error\ndata: ")
            .expect("Anthropic error event must use event: error framing");
        let parsed: serde_json::Value = serde_json::from_str(data.trim_end()).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["error"]["type"], "stream_error");
        assert_eq!(parsed["error"]["message"], "boom");
        assert!(anthropic.ends_with("\n\n"));

        // Chat：data-only 的 {"error":{...}}（error.rs 的错误体形状）
        let chat = String::from_utf8(ClientSseProtocol::Chat.error_event("boom").to_vec()).unwrap();
        assert!(!chat.contains("event:"));
        let parsed: serde_json::Value =
            serde_json::from_str(chat.trim_start_matches("data: ").trim_end()).unwrap();
        assert_eq!(parsed["error"]["type"], "stream_error");
        assert_eq!(parsed["error"]["message"], "boom");

        // Responses 与 Anthropic 同形状（同一个 error 事件封装）
        assert_eq!(
            ClientSseProtocol::Responses.error_event("boom"),
            ClientSseProtocol::Anthropic.error_event("boom")
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
