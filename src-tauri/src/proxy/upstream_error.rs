//! Distinguish provider capability rejection from globally invalid client input.
//! Like CPA's model-support classification, HTTP status alone is not sufficient.
use super::ProxyError;
use serde_json::Value;

pub(crate) fn is_capability_rejection(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError { status: 400 | 422, body: Some(body) } = error else {
        return false;
    };
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error")).or(parsed.as_ref());
    let code = error.and_then(|v| v.get("code")).and_then(Value::as_str).unwrap_or("");
    let message = error.and_then(|v| v.get("message")).and_then(Value::as_str)
        .unwrap_or(body).to_ascii_lowercase();
    // Invalid history and malformed input must not fan out across providers.
    if ["invalid json", "missing required", "tool_call_id", "tool_use_id", "must be followed", "invalid role"]
        .iter().any(|pattern| message.contains(pattern)) {
        return false;
    }
    if matches!(code, "unsupported_parameter" | "unsupported_value" | "model_not_found" | "model_not_supported") {
        return true;
    }
    let unsupported = ["not supported", "unsupported", "not support", "unrecognized", "unknown parameter", "unknown field"]
        .iter().any(|pattern| message.contains(pattern));
    let capability = ["thinking", "reasoning", "parameter", "field", "model", "tool", "stream", "response_format"]
        .iter().any(|pattern| message.contains(pattern));
    // Some relays serialize absent schema arrays as null. Try another candidate,
    // but never silently weaken enum/items/required constraints in the payload.
    let relay_schema = message.contains("invalid schema for function")
        && (message.contains("null is not of type") || message.contains("not supported"));
    (unsupported && capability) || relay_schema
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinguishes_capability_from_invalid_input() {
        for (message, expected) in [
            ("\"thinking\" is not supported on /v1/chat/completions", true),
            ("Invalid schema for function 'delegate_task': null is not of type \"array\"", true),
            ("unsupported model", true),
            ("Invalid schema for function 'x': required must contain all properties", false),
            ("Invalid JSON", false),
            ("unsupported tool_call_id", false),
            ("invalid schema", false),
        ] {
            let error = ProxyError::UpstreamError { status: 400, body: Some(serde_json::json!({"error":{"message":message}}).to_string()) };
            assert_eq!(is_capability_rejection(&error), expected, "{message}");
        }
    }
}
