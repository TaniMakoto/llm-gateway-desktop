//! Distinguish provider capability rejection from globally invalid client input.
//! Like CPA's model-support classification, HTTP status alone is not sufficient.
use super::ProxyError;
use serde_json::Value;

pub(crate) fn is_model_support_rejection(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError {
        status: 400 | 404 | 422,
        body: Some(body),
    } = error
    else {
        return false;
    };
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .or(parsed.as_ref());
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        code.as_str(),
        "model_not_found"
            | "model_not_found_error"
            | "unknown_model"
            | "model_does_not_exist"
            | "model_not_exist"
            | "model_not_supported"
    ) {
        return true;
    }
    let message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body)
        .to_ascii_lowercase();
    [
        "model_not_supported",
        "requested model is not supported",
        "requested model is unsupported",
        "requested model is unavailable",
        "model is not supported",
        "model not supported",
        "unsupported model",
        "model unavailable",
        "not available for your plan",
        "not available for your account",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

pub(crate) fn is_capability_rejection(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError {
        status: 400 | 404 | 422,
        body: Some(body),
    } = error
    else {
        return false;
    };
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error_body = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .or(parsed.as_ref());
    let code = error_body
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error_body
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body)
        .to_ascii_lowercase();
    // Invalid history and malformed input must not fan out across providers.
    if ["invalid json", "missing required", "tool_call_id", "tool_use_id", "must be followed", "invalid role"]
        .iter().any(|pattern| message.contains(pattern)) {
        return false;
    }
    if is_model_support_rejection(error) || code == "unsupported_parameter" {
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

/// Port of CPA's request-fault boundary: these failures are caused by the
/// request and must neither rotate nor penalize providers.
pub(crate) fn is_request_fault(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError {
        status,
        body: Some(body),
    } = error
    else {
        return false;
    };
    if matches!(*status, 402 | 429) {
        return false;
    }

    let parsed = serde_json::from_str::<Value>(body).ok();
    let envelopes = parsed
        .as_ref()
        .into_iter()
        .flat_map(|root| {
            [
                Some(root),
                root.get("error"),
                root.pointer("/response/error"),
                root.pointer("/body/error"),
            ]
        })
        .flatten()
        .collect::<Vec<_>>();

    let has_code = |expected: &[&str]| {
        envelopes.iter().any(|value| {
            value
                .get("code")
                .and_then(Value::as_str)
                .is_some_and(|code| expected.iter().any(|item| code.eq_ignore_ascii_case(item)))
        })
    };
    let has_type = |expected: &[&str]| {
        envelopes.iter().any(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| expected.iter().any(|item| kind.eq_ignore_ascii_case(item)))
        })
    };

    if *status == 401 && has_type(&["authentication_error"]) {
        return false;
    }
    if has_code(&["model_not_found", "model_not_found_error"]) {
        return false;
    }
    if has_code(&[
        "cyber_policy",
        "context_length_exceeded",
        "message_too_big",
        "string_above_max_length",
        "invalid_prompt",
        "invalid_value",
        "unsupported_value",
        "invalid_request_error",
        "previous_response_not_found",
    ]) || has_type(&[
        "invalid_request",
        "invalid_request_error",
        "bad_request_error",
        "invalid_prompt",
    ]) {
        return true;
    }

    let lower = body.to_ascii_lowercase();
    if lower.contains("item with id")
        && lower.contains("not found")
        && lower.contains("items are not persisted when `store` is set to false")
    {
        return true;
    }

    matches!(*status, 400 | 409 | 413 | 422)
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

    #[test]
    fn mirrors_cpa_request_fault_boundary() {
        for (status, body, expected) in [
            (400, r#"{"error":{"code":"invalid_request_error"}}"#, true),
            (409, "conflict", true),
            (413, "too large", true),
            (422, "unprocessable", true),
            (429, r#"{"error":{"code":"invalid_request_error"}}"#, false),
            (400, r#"{"error":{"code":"model_not_found"}}"#, false),
            (401, r#"{"error":{"type":"authentication_error"}}"#, false),
            (502, r#"{"error":{"code":"context_length_exceeded"}}"#, true),
        ] {
            let error = ProxyError::UpstreamError {
                status,
                body: Some(body.to_string()),
            };
            assert_eq!(is_request_fault(&error), expected, "{status}: {body}");
        }
    }

    #[test]
    fn mirrors_cpa_model_support_detection() {
        for (status, body, expected) in [
            (400, r#"{"error":{"code":"model_not_found"}}"#, true),
            (404, r#"{"error":{"message":"unsupported model"}}"#, true),
            (422, r#"{"error":{"message":"not available for your plan"}}"#, true),
            (400, r#"{"error":{"message":"unsupported tool_call_id"}}"#, false),
        ] {
            let error = ProxyError::UpstreamError {
                status,
                body: Some(body.to_string()),
            };
            assert_eq!(
                is_model_support_rejection(&error),
                expected,
                "{status}: {body}"
            );
        }
    }
}
