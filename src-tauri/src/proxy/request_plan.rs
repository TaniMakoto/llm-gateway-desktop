//! Candidate-local protocol planning, inspired by CPA's source/target translator boundary.
//! The retry loop owns the original payload; no candidate may rewrite the next one's input.
use crate::provider::Provider;
use super::{providers::codex_provider_uses_chat_completions, ProxyError};
use serde_json::Value;

pub(crate) struct OpenAiRequestPlan {
    pub endpoint: String,
    pub body: Value,
    pub native_chat: bool,
}

impl OpenAiRequestPlan {
    pub fn for_provider(endpoint: &str, body: &Value, provider: &Provider) -> Result<Self, ProxyError> {
        let (path, query) = endpoint.split_once('?').map_or((endpoint, None), |(p, q)| (p, Some(q)));
        let source_chat = matches!(path, "/chat/completions" | "/v1/chat/completions");
        let native_chat = source_chat && codex_provider_uses_chat_completions(provider);
        if source_chat && !native_chat {
            Ok(Self {
                endpoint: query.map_or_else(|| "/responses".into(), |q| format!("/responses?{q}")),
                body: crate::gateway_chat::chat_request_to_responses(body.clone())
                    .map_err(|error| ProxyError::TransformError(format!("Chat bridge capability: {error}")))?,
                native_chat: false,
            })
        } else {
            Ok(Self { endpoint: endpoint.into(), body: body.clone(), native_chat })
        }
    }
}

/// Compatibility is opt-in and schema-aware. Never rewrite enum values, examples,
/// explicit nulls, required constraints or strict function definitions.
pub(crate) fn default_chat_tool_required_arrays(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(function) = tool.get_mut("function") {
                if function.get("strict").and_then(Value::as_bool) == Some(true) { continue; }
                if let Some(schema) = function.get_mut("parameters") { default_required(schema); }
            }
        }
    }
}

fn default_required(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else { return; };
    if object.get("type").and_then(Value::as_str) == Some("object") || object.contains_key("properties") {
        object.entry("required").or_insert_with(|| serde_json::json!([]));
    }
    for key in ["properties", "patternProperties", "$defs", "definitions", "dependentSchemas"] {
        if let Some(map) = object.get_mut(key).and_then(Value::as_object_mut) {
            for child in map.values_mut() { default_required(child); }
        }
    }
    for key in ["items", "additionalProperties", "additionalItems", "contains", "not", "if", "then", "else", "propertyNames", "unevaluatedProperties", "unevaluatedItems"] {
        if let Some(child) = object.get_mut(key) {
            if let Some(items) = child.as_array_mut() {
                for item in items { default_required(item); }
            } else { default_required(child); }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for child in children { default_required(child); }
        }
    }
}

pub(crate) fn validate_openai_request(body: &Value, chat: bool) -> Result<(), ProxyError> {
    if body.get("model").and_then(Value::as_str).is_none_or(|m| m.trim().is_empty()) {
        return Err(ProxyError::InvalidRequest("缺少 model".into()));
    }
    if chat && !body.get("messages").is_some_and(Value::is_array) {
        return Err(ProxyError::InvalidRequest("缺少 messages 数组".into()));
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_defaults_do_not_rewrite_constraints_or_data() {
        let schema = json!({"type":"object","required":null,"properties":{
            "nested":{"type":"array","items":{"type":"object","properties":{}}},
            "mode":{"enum":[null,{"properties":{}}]}},
            "examples":[{"properties":{}}],"anyOf":[{"type":"object"}]});
        let mut body = json!({"tools":[
            {"type":"function","function":{"name":"strict","strict":true,"parameters":schema}},
            {"type":"function","function":{"name":"loose","parameters":schema}}]});
        let strict = body["tools"][0].clone();
        default_chat_tool_required_arrays(&mut body);
        assert_eq!(body["tools"][0], strict);
        let result = &body["tools"][1]["function"]["parameters"];
        assert!(result["required"].is_null());
        assert_eq!(result["properties"]["nested"]["items"]["required"], json!([]));
        assert_eq!(result["anyOf"][0]["required"], json!([]));
        assert_eq!(result["properties"]["mode"]["enum"], schema["properties"]["mode"]["enum"]);
        assert_eq!(result["examples"], schema["examples"]);
        let once = body.clone();
        default_chat_tool_required_arrays(&mut body);
        assert_eq!(body, once);
    }
}
