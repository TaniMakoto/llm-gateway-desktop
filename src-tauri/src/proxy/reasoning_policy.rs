//! Final, candidate-local reasoning validation. Runs after translations and overrides.
use super::ProxyError;
use crate::provider::Provider;
use serde_json::{json, Value};
use std::sync::OnceLock;

#[derive(Clone, Default)]
struct Capability {
    levels: Vec<String>,
    budget: Option<(i64, i64)>,
    zero: bool,
    dynamic: bool,
    family: Option<String>,
    source: &'static str,
}

fn catalog(model: &str, format: &str) -> Option<&'static Value> {
    static DATA: OnceLock<Value> = OnceLock::new();
    let data = DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/cpa/models.json")).expect("CPA catalog")
    });
    // Exact IDs only: an arbitrary provider alias must not inherit a guessed capability.
    let rows: Vec<_> = data
        .as_object()?
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.eq_ignore_ascii_case(model))
        })
        .collect();
    rows.iter()
        .copied()
        .find(|r| {
            r.get("type")
                .and_then(Value::as_str)
                .is_some_and(|f| family(f) == family(format))
        })
        .or_else(|| rows.first().copied())
}

fn capability(provider: &Provider, model: &str, format: &str) -> Option<Capability> {
    let row = catalog(model, format);
    let mut result = Capability {
        family: row
            .and_then(|r| r.get("type"))
            .and_then(Value::as_str)
            .filter(|f| *f != "antigravity")
            .map(str::to_owned),
        ..Default::default()
    };
    if let Some(levels) = provider.meta.as_ref().and_then(|m| {
        m.reasoning_model_levels
            .get(&model.trim().to_ascii_lowercase())
    }) {
        // Empty declarations from legacy settings mean unknown, never "unsupported".
        if !levels.is_empty() {
            result.levels = levels
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .collect();
            result.source = "当前路由模型能力声明";
            return Some(result);
        }
    }
    if let Some(known) = crate::model_capabilities::registry_model_capabilities(model, Some(format))
    {
        if !known.reasoning_levels.is_empty() {
            result.levels = known.reasoning_levels;
            result.source = "内置模型目录";
        }
    }
    if let Some(row) = row {
        if let Some(thinking) = row.get("thinking").filter(|v| v.is_object()) {
            if result.levels.is_empty() {
                result.levels = thinking
                    .get("levels")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                result.source = "CPA 模型目录";
            }
            let min = thinking.get("min").and_then(Value::as_i64).unwrap_or(0);
            let max = thinking.get("max").and_then(Value::as_i64).unwrap_or(0);
            if max > 0 {
                result.budget = Some((min, max));
            }
            result.zero = thinking
                .get("zero_allowed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            result.dynamic = thinking
                .get("dynamic_allowed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        }
        // Missing thinking metadata is not enough evidence to strip client controls.
    }
    (!result.levels.is_empty() || result.budget.is_some()).then_some(result)
}

fn family(format: &str) -> &str {
    match format {
        "openai_chat" | "openai_responses" | "codex" | "openai" => "openai",
        "anthropic" | "claude" => "claude",
        "gemini_native" | "gemini" | "gemini-cli" => "gemini",
        other => other,
    }
}

fn level(body: &Value) -> Option<String> {
    [
        "/reasoning_effort",
        "/reasoning/effort",
        "/output_config/effort",
        "/generationConfig/thinkingConfig/thinkingLevel",
        "/generation_config/thinking_config/thinking_level",
    ]
    .iter()
    .find_map(|p| body.pointer(p).and_then(Value::as_str))
    .map(|s| s.trim().to_ascii_lowercase())
}

fn budget(body: &Value) -> Option<i64> {
    [
        "/thinking/budget_tokens",
        "/generationConfig/thinkingConfig/thinkingBudget",
        "/generation_config/thinking_config/thinking_budget",
    ]
    .iter()
    .find_map(|p| body.pointer(p).and_then(Value::as_i64))
}

fn clear(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        o.remove("reasoning_effort");
        o.remove("thinking");
        o.remove("enable_thinking");
    }
    for (path, keys) in [
        ("/reasoning", &["effort"][..]),
        ("/output_config", &["effort"][..]),
        (
            "/generationConfig/thinkingConfig",
            &["thinkingLevel", "thinkingBudget"][..],
        ),
        (
            "/generation_config/thinking_config",
            &["thinking_level", "thinking_budget"][..],
        ),
    ] {
        if let Some(o) = body.pointer_mut(path).and_then(Value::as_object_mut) {
            for key in keys {
                o.remove(*key);
            }
        }
    }
}

fn write_level(body: &mut Value, format: &str, value: &str) {
    match format {
        "openai_chat"
            if body.pointer("/reasoning/effort").is_some()
                && body.get("reasoning_effort").is_none() =>
        {
            body["reasoning"]["effort"] = json!(value)
        }
        "openai_chat" => body["reasoning_effort"] = json!(value),
        "openai_responses" => body["reasoning"]["effort"] = json!(value),
        "anthropic" => {
            body["output_config"]["effort"] = json!(value);
            if body.get("thinking").is_none() {
                body["thinking"] = json!({"type":"adaptive"});
            }
        }
        "gemini_native" => {
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"] =
                json!(value.to_ascii_uppercase())
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(model: &str, levels: &[&str], mode: &str) -> Provider {
        serde_json::from_value(
            json!({"id":"test", "name":"test", "settingsConfig":{}, "meta":{
                "reasoningRequestMode":mode, "reasoningModelLevels":{model:levels}
            }}),
        )
        .unwrap()
    }

    #[test]
    fn native_chat_and_responses_reject_unsupported_xhigh_without_mutation() {
        let p = provider("custom", &["low", "medium", "high"], "auto");
        for (format, original) in [
            (
                "openai_chat",
                json!({"model":"custom","reasoning_effort":"xhigh"}),
            ),
            (
                "openai_responses",
                json!({"model":"custom","reasoning":{"effort":"xhigh","summary":"auto"}}),
            ),
        ] {
            let mut body = original.clone();
            assert!(matches!(
                apply(&mut body, &original, &p, format, format, false),
                Err(ProxyError::InvalidRequest(_))
            ));
            assert_eq!(body, original);
        }
    }

    #[test]
    fn each_candidate_uses_original_intent_and_preserves_summary() {
        let original = json!({"output_config":{"effort":"max"}});
        for (levels, expected) in [
            (vec!["high"], "high"),
            (vec!["high", "xhigh"], "xhigh"),
            (vec!["high", "max"], "max"),
        ] {
            let p = provider("custom", &levels, "auto");
            let mut body = json!({"model":"custom","reasoning":{"effort":"high","summary":"auto"}});
            apply(
                &mut body,
                &original,
                &p,
                "anthropic",
                "openai_responses",
                false,
            )
            .unwrap();
            assert_eq!(body["reasoning"]["effort"], expected);
            assert_eq!(body["reasoning"]["summary"], "auto");
        }
        assert_eq!(original["output_config"]["effort"], "max");
    }

    #[test]
    fn unknown_and_empty_capabilities_do_not_imply_disabled() {
        let p = provider("unlisted-model", &[], "auto");
        let original = json!({"reasoning_effort":"xhigh"});
        let mut body = json!({"model":"unlisted-model"});
        let note = apply(
            &mut body,
            &original,
            &p,
            "openai_chat",
            "openai_chat",
            false,
        )
        .unwrap();
        assert!(note.contains("能力未知"));
        assert_eq!(body["reasoning_effort"], "xhigh");
    }

    #[test]
    fn overrides_cannot_bypass_validation_and_disabled_keeps_summary() {
        let original = json!({"reasoning":{"effort":"low"}});
        let mut body = json!({"model":"custom", "reasoning":{"effort":"xhigh","summary":"auto"}});
        let p = provider("custom", &["low", "high"], "force");
        assert!(apply(
            &mut body,
            &original,
            &p,
            "openai_responses",
            "openai_responses",
            true
        )
        .is_err());
        let p = provider("custom", &["low", "high"], "disabled");
        apply(
            &mut body,
            &original,
            &p,
            "openai_responses",
            "openai_responses",
            true,
        )
        .unwrap();
        assert!(body["reasoning"].get("effort").is_none());
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn cpa_budget_conversion_respects_output_limit_and_native_budget_is_strict() {
        let p = provider("unused", &[], "auto");
        let original = json!({"reasoning":{"effort":"xhigh"}});
        let mut body = json!({"model":"claude-sonnet-4-5-20250929", "max_tokens":4096});
        apply(
            &mut body,
            &original,
            &p,
            "openai_responses",
            "anthropic",
            false,
        )
        .unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 4095);
        let original = json!({"thinking":{"type":"enabled","budget_tokens":8000}});
        assert!(apply(&mut body, &original, &p, "anthropic", "anthropic", false).is_err());
    }
}

fn map_level(requested: &str, levels: &[String]) -> Option<String> {
    if levels.iter().any(|s| s == requested) {
        return Some(requested.into());
    }
    let candidates: &[&str] = match requested {
        "xhigh" => &["max", "high"],
        "max" => &["xhigh", "high"],
        _ => &[],
    };
    if let Some(found) = candidates.iter().find(|s| levels.iter().any(|l| l == **s)) {
        return Some((*found).into());
    }
    let order = ["minimal", "low", "medium", "high", "xhigh", "max"];
    let index = order.iter().position(|s| *s == requested)?;
    order
        .iter()
        .enumerate()
        .filter(|(_, s)| levels.iter().any(|l| l == **s))
        .min_by_key(|(i, _)| (i.abs_diff(index), *i))
        .map(|(_, s)| (*s).into())
}

/// Original ingress remains immutable for every fallback. Explicit body overrides
/// win over ingress, but still pass the same capability validation.
pub(crate) fn apply(
    body: &mut Value,
    original: &Value,
    provider: &Provider,
    source: &str,
    target: &str,
    overridden: bool,
) -> Result<String, ProxyError> {
    for value in [original, &*body] {
        for key in [
            "reasoning",
            "thinking",
            "output_config",
            "generationConfig",
            "generation_config",
        ] {
            if value
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_object())
            {
                return Err(ProxyError::InvalidRequest(format!(
                    "思考配置 {key} 必须是对象"
                )));
            }
        }
        for path in [
            "/generationConfig/thinkingConfig",
            "/generation_config/thinking_config",
        ] {
            if value
                .pointer(path)
                .is_some_and(|v| !v.is_null() && !v.is_object())
            {
                return Err(ProxyError::InvalidRequest(
                    "thinkingConfig 必须是对象".into(),
                ));
            }
        }
    }
    let mode = provider
        .meta
        .as_ref()
        .and_then(|m| m.reasoning_request_mode.as_deref())
        .unwrap_or("auto");
    if mode == "disabled" {
        clear(body);
        return Ok("供应商配置禁用思考控制".into());
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let profile = (target == "openai_chat")
        .then(|| super::providers::resolve_codex_chat_reasoning_config(provider, body))
        .flatten();
    if let Some(profile) = &profile {
        if profile.supports_effort == Some(false) {
            // The profile may support only a thinking on/off switch. Do not
            // resurrect an effort field that the vendor adapter deliberately removed.
            if let Some(o) = body.as_object_mut() {
                o.remove("reasoning_effort");
            }
            if let Some(o) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
                o.remove("effort");
            }
            return Ok("供应商协议配置不发送等级；保留其思考开关设置".into());
        }
    }
    let profile_mapping = profile
        .as_ref()
        .and_then(|p| p.effort_value_mode.as_deref())
        .is_some_and(|m| m != "passthrough");
    let cap = capability(provider, &model, target);
    // Explicit vendor profiles already translate spelling and field location.
    // Validate that final value without undoing the provider's wire contract.
    let intent = if overridden || profile_mapping {
        &*body
    } else {
        original
    };
    let requested = level(intent);
    let requested_budget = budget(intent);
    let disabled = intent.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled");
    let adaptive = intent.pointer("/thinking/type").and_then(Value::as_str) == Some("adaptive");
    if target == "anthropic" && (disabled || requested.as_deref() == Some("none")) {
        clear(body);
        body["thinking"] = json!({"type":"disabled"});
        return Ok("按客户端请求关闭 Anthropic 思考，移除互斥的 effort".into());
    }
    let Some(cap) = cap else {
        if let Some(value) = requested.as_deref() {
            write_level(body, target, value);
        }
        return Ok(if profile_mapping {
            "已按供应商协议配置转换；模型等级能力未知，由上游校验"
        } else {
            "能力未知：保留请求，由上游校验（未验证支持）"
        }
        .into());
    };
    let cross = family(source) != family(target)
        || cap
            .family
            .as_deref()
            .is_some_and(|f| family(f) != family(source));
    let requested = requested
        .or_else(|| {
            requested_budget.map(|n| {
                match n {
                    0 => "none",
                    -1 => "auto",
                    1..=512 => "minimal",
                    513..=1024 => "low",
                    1025..=8192 => "medium",
                    8193..=24576 => "high",
                    _ => "xhigh",
                }
                .to_string()
            })
        })
        .or_else(|| disabled.then(|| "none".into()))
        .or_else(|| adaptive.then(|| "auto".into()));
    if !cap.levels.is_empty()
        && !(requested_budget.is_some()
            && cap.budget.is_some()
            && matches!(target, "anthropic" | "gemini_native"))
    {
        if let Some(requested) = requested {
            let supported = cap.levels.iter().any(|s| s == &requested);
            let mapped = if supported {
                Some(requested.clone())
            } else if cross || requested_budget.is_some() || adaptive {
                map_level(
                    if requested == "auto" {
                        "medium"
                    } else {
                        &requested
                    },
                    &cap.levels,
                )
            } else {
                None
            };
            let Some(mapped) = mapped else {
                return Err(ProxyError::InvalidRequest(format!(
                    "模型 {model} 不支持思考等级 {requested}；支持：{}（{}）",
                    cap.levels.join(", "),
                    cap.source
                )));
            };
            write_level(body, target, &mapped);
            return Ok(format!(
                "{}：{} → {}{}",
                cap.source,
                requested,
                mapped,
                if supported {
                    "（声明支持）"
                } else {
                    "（跨协议/预算映射）"
                }
            ));
        }
    } else if let Some((min, max)) = cap.budget {
        let n = requested_budget.or_else(|| {
            requested.as_deref().and_then(|s| match s {
                "none" => Some(0),
                "auto" => Some(-1),
                "minimal" => Some(512),
                "low" => Some(1024),
                "medium" => Some(8192),
                "high" => Some(24576),
                "xhigh" => Some(32768),
                "max" => Some(128000),
                _ => None,
            })
        });
        if n.is_none() && requested.is_some() {
            return Err(ProxyError::InvalidRequest(format!(
                "模型 {model} 无法将思考等级 {} 换算为预算",
                requested.unwrap()
            )));
        }
        if let Some(n) = n {
            let special = (n == 0 && cap.zero) || (n == -1 && cap.dynamic);
            let mut upper = max;
            if target == "anthropic" {
                if let Some(tokens) = body.get("max_tokens").and_then(Value::as_i64) {
                    upper = upper.min(tokens - 1);
                }
            }
            if !special && upper < min {
                return Err(ProxyError::InvalidRequest(format!(
                    "模型 {model} 的输出上限不足以容纳最小思考预算 {min}"
                )));
            }
            let mapped = if special {
                n
            } else if n == -1 {
                (min + upper) / 2
            } else {
                n.clamp(min, upper)
            };
            if !cross && requested_budget.is_some() && n != mapped {
                return Err(ProxyError::InvalidRequest(format!(
                    "模型 {model} 思考预算 {n} 超出支持范围 {min}..{upper}"
                )));
            }
            match target {
                "anthropic" => {
                    if let Some(o) = body.get_mut("output_config").and_then(Value::as_object_mut) {
                        o.remove("effort");
                    }
                    body["thinking"] = if mapped == 0 {
                        json!({"type":"disabled"})
                    } else if mapped == -1 {
                        json!({"type":"adaptive"})
                    } else {
                        json!({"type":"enabled", "budget_tokens":mapped})
                    };
                }
                "gemini_native" => {
                    if let Some(o) = body
                        .pointer_mut("/generationConfig/thinkingConfig")
                        .and_then(Value::as_object_mut)
                    {
                        o.remove("thinkingLevel");
                    }
                    body["generationConfig"]["thinkingConfig"]["thinkingBudget"] = json!(mapped);
                }
                _ => {
                    return Ok(format!(
                        "{}：预算能力无法验证此协议的等级，保留转换结果",
                        cap.source
                    ))
                }
            }
            return Ok(format!(
                "{}：思考预算 {n} → {mapped}（跨协议换算为估算值）",
                cap.source
            ));
        }
    }
    Ok(format!("{}：未指定思考控制，保留上游默认值", cap.source))
}
