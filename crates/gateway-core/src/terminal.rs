/// 协议终态。合法的上限截断与意外断流分开；失败可以覆盖此前的结束标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseTerminalState {
    /// 尚未见到任何协议终态标记（流被中途掐断时停在这里）
    Pending,
    /// 已见到正常结束标记（message_stop / [DONE] / response.completed ...）
    Completed,
    /// 上游明确报告 token 上限等合法截断，不能当成供应商故障。
    Incomplete,
    /// 已见到失败终态（event: error / response.failed），上游已自行报错
    Failed,
}

impl SseTerminalState {
    pub fn is_successful(self) -> bool {
        matches!(self, Self::Completed | Self::Incomplete)
    }

    pub fn observe(&mut self, next: Self) {
        if *self == Self::Failed || next == Self::Pending {
            return;
        }
        if next == Self::Failed || *self == Self::Pending || next == Self::Incomplete {
            *self = next;
        }
    }
}

/// 原始上游观察和下游诊断使用同一套语义，正文中的标记文字不会触发终态。
pub fn classify_terminal_event(event: &serde_json::Value) -> Option<SseTerminalState> {
    use serde_json::Value;
    let kind = event.get("type").and_then(Value::as_str);
    let response = event.get("response").unwrap_or(event);
    if matches!(kind, Some("error" | "response.failed"))
        || event.get("error").is_some_and(|value| !value.is_null())
        || response.get("error").is_some_and(|value| !value.is_null())
        || matches!(
            response.get("status").and_then(Value::as_str),
            Some("failed" | "cancelled")
        )
    {
        return Some(SseTerminalState::Failed);
    }
    match kind {
        Some("response.completed" | "response.incomplete") => {
            return Some(
                if kind == Some("response.incomplete")
                    || response.get("status").and_then(Value::as_str) == Some("incomplete")
                {
                    SseTerminalState::Incomplete
                } else {
                    SseTerminalState::Completed
                },
            );
        }
        Some("message_stop") => return Some(SseTerminalState::Completed),
        Some("message_delta") => {
            return event
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
                .map(|reason| {
                    if reason == "max_tokens" {
                        SseTerminalState::Incomplete
                    } else {
                        SseTerminalState::Completed
                    }
                });
        }
        _ => {}
    }
    for (array, reason_key, incomplete_reason) in [
        ("choices", "finish_reason", "length"),
        ("candidates", "finishReason", "MAX_TOKENS"),
    ] {
        if let Some(items) = event.get(array).and_then(Value::as_array) {
            let mut terminal = SseTerminalState::Pending;
            for reason in items
                .iter()
                .filter_map(|item| item.get(reason_key).and_then(Value::as_str))
            {
                terminal.observe(if reason == incomplete_reason {
                    SseTerminalState::Incomplete
                } else {
                    SseTerminalState::Completed
                });
            }
            if terminal != SseTerminalState::Pending {
                return Some(terminal);
            }
        }
    }
    None
}

/// Preserve the upstream's diagnostic limit reason without reading any content.
pub fn incomplete_reason(event: &serde_json::Value) -> Option<&str> {
    use serde_json::Value;
    let response = event.get("response").unwrap_or(event);
    if response.get("status").and_then(Value::as_str) == Some("incomplete")
        || event.get("type").and_then(Value::as_str) == Some("response.incomplete")
    {
        return Some(
            response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("incomplete"),
        );
    }
    if event.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("max_tokens") {
        return Some("max_tokens");
    }
    for (array, field, expected) in [
        ("choices", "finish_reason", "length"),
        ("candidates", "finishReason", "MAX_TOKENS"),
    ] {
        if event
            .get(array)
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get(field).and_then(Value::as_str) == Some(expected))
            })
        {
            return Some(expected);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_protocols_distinguish_explicit_limits_from_failures() {
        for event in [
            json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}),
            json!({"type":"response.completed","response":{"status":"incomplete"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"}}),
            json!({"choices":[{"finish_reason":"length"}]}),
            json!({"candidates":[{"finishReason":"MAX_TOKENS"}]}),
        ] {
            assert_eq!(
                classify_terminal_event(&event),
                Some(SseTerminalState::Incomplete)
            );
        }
        for event in [
            json!({"type":"response.completed","response":{"status":"failed"}}),
            json!({"type":"response.incomplete","response":{"error":{"message":"boom"}}}),
            json!({"type":"error"}),
            json!({"choices":[{"finish_reason":"stop"}],"error":{"message":"boom"}}),
        ] {
            assert_eq!(
                classify_terminal_event(&event),
                Some(SseTerminalState::Failed)
            );
        }
    }

    #[test]
    fn usage_and_body_text_are_not_completion_signals() {
        for event in [
            json!({"type":"response.output_text.delta","delta":"response.completed and [DONE]"}),
            json!({"choices":[{"delta":{"content":"finish_reason: stop"},"finish_reason":null}]}),
            json!({"choices":[],"usage":{"completion_tokens":42}}),
        ] {
            assert_eq!(classify_terminal_event(&event), None);
        }
    }

    #[test]
    fn failure_is_monotonic_and_explicit_limit_is_not_overwritten_by_stop() {
        let mut state = SseTerminalState::Pending;
        for next in [SseTerminalState::Incomplete, SseTerminalState::Completed] {
            state.observe(next);
        }
        assert_eq!(state, SseTerminalState::Incomplete);
        for next in [
            SseTerminalState::Failed,
            SseTerminalState::Completed,
            SseTerminalState::Incomplete,
        ] {
            state.observe(next);
        }
        assert_eq!(state, SseTerminalState::Failed);
    }
}
