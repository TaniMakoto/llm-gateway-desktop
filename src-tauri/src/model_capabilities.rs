use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryFile {
    #[serde(default)]
    models: Vec<RegistryEntry>,
    #[serde(default)]
    text_only_models: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryEntry {
    ids: Vec<String>,
    context_length: Option<u64>,
    max_output_tokens: Option<u64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    reasoning_levels: Vec<String>,
    default_reasoning_level: Option<String>,
    #[serde(default)]
    protocol_reasoning_levels: HashMap<String, Vec<String>>,
}

fn registry_file() -> &'static RegistryFile {
    static REGISTRY: OnceLock<RegistryFile> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str(include_str!("resources/model_capabilities.json"))
            .expect("bundled model_capabilities.json must be valid")
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RegistryModelCapabilities {
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_modalities: Vec<String>,
    pub reasoning_levels: Vec<String>,
    pub default_reasoning_level: Option<String>,
}

/// Conservative built-in capability fallback used only when upstream/user
/// metadata leaves a field unknown. Keep this registry evidence-backed and
/// exact: unknown models must stay unknown rather than inheriting family guesses.
pub(crate) fn registry_model_capabilities(
    model: &str,
    api_format: Option<&str>,
) -> Option<RegistryModelCapabilities> {
    let normalized = normalize_model_id(model);
    let tail = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let base_tail = strip_known_reasoning_suffix(tail);

    if let Some(entry) = registry_file()
        .models
        .iter()
        .find(|entry| entry.ids.iter().any(|id| id == base_tail))
    {
        let reasoning_levels = api_format
            .and_then(|format| entry.protocol_reasoning_levels.get(format))
            .cloned()
            .unwrap_or_else(|| entry.reasoning_levels.clone());
        return Some(RegistryModelCapabilities {
            context_length: entry.context_length,
            max_output_tokens: entry.max_output_tokens,
            input_modalities: entry.input_modalities.clone(),
            reasoning_levels,
            default_reasoning_level: entry.default_reasoning_level.clone(),
        });
    }

    if is_confirmed_text_only_model(base_tail) {
        return Some(RegistryModelCapabilities {
            input_modalities: vec!["text".to_string()],
            ..Default::default()
        });
    }

    None
}

fn strip_known_reasoning_suffix(model: &str) -> &str {
    for suffix in ["-minimal", "-low", "-medium", "-high", "-xhigh", "-max"] {
        if let Some(stripped) = model.strip_suffix(suffix) {
            return stripped;
        }
    }
    model
}

pub(crate) fn canonical_model_key(model: &str) -> String {
    let normalized = normalize_model_id(model);
    let tail = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    strip_known_reasoning_suffix(tail).to_string()
}

/// Image-input capability shared by Codex catalog generation and proxy request
/// rectification.
///
/// `Unknown` is intentionally distinct from `Supported`: callers may choose
/// different execution policies without duplicating the model-name registry.
/// The Codex catalog treats unknown models as image-capable (fail open), while
/// the media rectifier leaves their request bodies untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageInputCapability {
    Supported,
    Unsupported,
    Unknown,
}

/// Resolve image-input capability from an explicit declaration first, then the
/// central canonical capability registry when the caller enables registry lookup.
pub(crate) fn resolve_image_input_capability(
    model: &str,
    declared_support: Option<bool>,
    use_confirmed_registry: bool,
) -> ImageInputCapability {
    match declared_support {
        Some(true) => ImageInputCapability::Supported,
        Some(false) => ImageInputCapability::Unsupported,
        None if use_confirmed_registry => registry_model_capabilities(model, None)
            .map(|capabilities| {
                if capabilities
                    .input_modalities
                    .iter()
                    .any(|value| value == "image")
                {
                    ImageInputCapability::Supported
                } else {
                    ImageInputCapability::Unsupported
                }
            })
            .unwrap_or(ImageInputCapability::Unknown),
        None => ImageInputCapability::Unknown,
    }
}

/// Resolve a model's image-input capability from the provider settings shapes
/// accepted by the proxy (`modelCatalog.models`, `modelCatalog`, or `models`).
pub(crate) fn image_input_capability_from_settings(
    settings: &Value,
    model: &str,
    use_confirmed_registry: bool,
) -> ImageInputCapability {
    resolve_image_input_capability(
        model,
        declared_model_image_support(settings, model),
        use_confirmed_registry,
    )
}

/// Convert a catalog row's explicit modality list into the shared capability
/// representation, falling back to the text-only registry when omitted.
pub(crate) fn image_input_capability_from_modalities(
    model: &str,
    modalities: Option<&[String]>,
) -> ImageInputCapability {
    let declared_support = modalities.map(|items| {
        items
            .iter()
            .any(|item| item.trim().eq_ignore_ascii_case("image"))
    });
    resolve_image_input_capability(model, declared_support, true)
}

/// Models that LLM Gateway Desktop is willing to advertise to clients as text-only.
///
/// This registry is deliberately exact and fail-open. A new suffix is not
/// inherited automatically: it remains image-capable until its capability is
/// confirmed, preventing a future `-vision`/`-vl` variant from being blocked by
/// the Codex client before a request can reach the proxy.
pub(crate) fn is_confirmed_text_only_model(model: &str) -> bool {
    let normalized = normalize_model_id(model);
    let tail = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    registry_file()
        .text_only_models
        .iter()
        .any(|candidate| candidate == tail)
}

fn declared_model_image_support(settings: &Value, model: &str) -> Option<bool> {
    [
        settings
            .get("modelCatalog")
            .and_then(|catalog| catalog.get("models")),
        settings.get("modelCatalog"),
        settings.get("models"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| declared_model_image_support_in_value(value, model))
}

fn declared_model_image_support_in_value(value: &Value, model: &str) -> Option<bool> {
    if let Some(models) = value.as_array() {
        return models.iter().find_map(|entry| {
            model_entry_matches(entry, None, model).then(|| explicit_image_support(entry))?
        });
    }

    let object = value.as_object()?;
    object.iter().find_map(|(key, entry)| {
        model_entry_matches(entry, Some(key), model).then(|| explicit_image_support(entry))?
    })
}

fn explicit_image_support(entry: &Value) -> Option<bool> {
    if let Some(value) = entry
        .get("supportsImage")
        .or_else(|| entry.get("supports_image"))
        .or_else(|| entry.get("vision"))
        .and_then(Value::as_bool)
    {
        return Some(value);
    }

    [
        entry.get("input"),
        entry.pointer("/modalities/input"),
        entry.get("input_modalities"),
        entry.get("inputModalities"),
    ]
    .into_iter()
    .flatten()
    .find_map(input_modalities_support_image)
}

fn input_modalities_support_image(value: &Value) -> Option<bool> {
    let modalities = value.as_array()?;
    Some(modalities.iter().any(|item| {
        item.as_str()
            .map(str::trim)
            .is_some_and(|item| item.eq_ignore_ascii_case("image"))
    }))
}

fn model_entry_matches(entry: &Value, key: Option<&str>, model: &str) -> bool {
    key.is_some_and(|key| model_ids_match(key, model))
        || ["model", "id", "name"]
            .into_iter()
            .filter_map(|field| entry.get(field).and_then(Value::as_str))
            .any(|candidate| model_ids_match(candidate, model))
}

fn model_ids_match(candidate: &str, model: &str) -> bool {
    let candidate = normalize_model_id(candidate);
    let model = normalize_model_id(model);
    if candidate.is_empty() || model.is_empty() {
        return false;
    }
    if candidate == model {
        return true;
    }

    let candidate_tail = candidate.rsplit('/').next().unwrap_or(candidate.as_str());
    let model_tail = model.rsplit('/').next().unwrap_or(model.as_str());
    candidate_tail == model_tail || candidate == model_tail || candidate_tail == model
}

fn normalize_model_id(value: &str) -> String {
    let mut normalized = value
        .trim()
        .trim_start_matches("models/")
        .trim()
        .to_ascii_lowercase();
    if let Some(stripped) =
        normalized.strip_suffix(crate::claude_desktop_config::ONE_M_CONTEXT_MARKER)
    {
        normalized = stripped.trim().to_string();
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gateway_registry_uses_bundled_gpt55_catalog_and_text_only_registry() {
        let gpt = registry_model_capabilities("openai/gpt-5.5-high", None).unwrap();
        assert_eq!(gpt.context_length, Some(272_000));
        assert_eq!(gpt.input_modalities, vec!["text", "image"]);
        assert_eq!(
            gpt.reasoning_levels,
            vec!["low", "medium", "high", "xhigh"]
        );

        let deepseek = registry_model_capabilities("deepseek-v4-pro", Some("openai_chat")).unwrap();
        assert_eq!(deepseek.input_modalities, vec!["text"]);
        assert_eq!(
            registry_model_capabilities("deepseek-v4-pro-high", Some("openai_chat"))
                .unwrap()
                .input_modalities,
            vec!["text"]
        );
        assert_eq!(deepseek.reasoning_levels, vec!["low", "high", "max"]);
        assert_eq!(deepseek.context_length, Some(1_048_576));
        assert_eq!(deepseek.max_output_tokens, Some(384_000));

        let deepseek_responses =
            registry_model_capabilities("deepseek-v4-pro", Some("openai_responses")).unwrap();
        assert_eq!(
            deepseek_responses.reasoning_levels,
            vec!["none", "minimal", "low", "medium", "high", "xhigh", "max"]
        );

        let gpt56 = registry_model_capabilities("gpt-5.6-sol", Some("openai_responses")).unwrap();
        assert_eq!(gpt56.context_length, Some(1_050_000));
        assert_eq!(gpt56.max_output_tokens, Some(128_000));
        assert_eq!(gpt56.input_modalities, vec!["text", "image"]);
        assert_eq!(gpt56.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(deepseek.default_reasoning_level.as_deref(), Some("high"));
        assert_eq!(canonical_model_key("openai/gpt-5.6-sol-high"), "gpt-5.6-sol");

        let astra = registry_model_capabilities("gpt-6-astra", Some("openai_responses")).unwrap();
        assert_eq!(astra.context_length, Some(1_050_000));
        assert_eq!(astra.reasoning_levels, vec!["low", "medium", "high", "xhigh", "max"]);

        let gemini = registry_model_capabilities("gemini-3.8-flash", None).unwrap();
        assert_eq!(gemini.max_output_tokens, Some(65_536));
        assert_eq!(gemini.default_reasoning_level.as_deref(), Some("medium"));
        assert!(gemini.input_modalities.contains(&"pdf".to_string()));

        let opus = registry_model_capabilities("claude-opus-5", Some("anthropic")).unwrap();
        assert_eq!(opus.context_length, Some(1_000_000));
        assert_eq!(opus.max_output_tokens, Some(128_000));
        assert_eq!(opus.default_reasoning_level.as_deref(), Some("high"));

        assert!(registry_model_capabilities("future-model-vision", None).is_none());
    }

    #[test]
    fn canonical_registry_drives_image_capability_when_known() {
        for model in ["gpt-5.5", "gpt-5.6-sol", "gemini-3.8-flash", "claude-opus-5"] {
            assert_eq!(
                resolve_image_input_capability(model, None, true),
                ImageInputCapability::Supported,
                "{model} should inherit image support from the canonical registry"
            );
        }
        assert_eq!(
            resolve_image_input_capability("deepseek-v4-pro", None, true),
            ImageInputCapability::Unsupported
        );
        for model in ["gpt-5.4", "custom-alias"] {
            assert_eq!(
                resolve_image_input_capability(model, None, true),
                ImageInputCapability::Unknown,
                "{model} must remain unknown"
            );
        }
    }

    #[test]
    fn confirmed_text_only_registry_normalizes_namespaces_and_context_markers() {
        assert!(is_confirmed_text_only_model("deepseek/deepseek-v4-pro"));
        assert!(is_confirmed_text_only_model("GLM-5.2[1M]"));
        assert!(is_confirmed_text_only_model("qwen/qwen3-coder-plus"));
        assert!(is_confirmed_text_only_model(
            "Qwen/Qwen3-Coder-480B-A35B-Instruct"
        ));
        assert!(is_confirmed_text_only_model("MiniMax-M2.7-Highspeed"));
        assert!(is_confirmed_text_only_model("step-3.5-flash-2603"));
        assert!(!is_confirmed_text_only_model("glm-5.2v"));
    }

    #[test]
    fn unconfirmed_family_suffixes_fail_open() {
        for model in [
            "minimax-m2.7-vision",
            "qwen3-coder-ultra",
            "qwen3-coder-vl",
            "step-3.5-flash-vision",
        ] {
            assert!(
                !is_confirmed_text_only_model(model),
                "unconfirmed variant {model} must not be hard-gated"
            );
        }
    }

    #[test]
    fn explicit_capability_overrides_the_registry() {
        assert_eq!(
            resolve_image_input_capability("deepseek-v4-pro", Some(true), true),
            ImageInputCapability::Supported
        );
        assert_eq!(
            resolve_image_input_capability("gpt-5.4", Some(false), true),
            ImageInputCapability::Unsupported
        );
    }

    #[test]
    fn provider_settings_support_multiple_capability_shapes() {
        let settings = json!({
            "modelCatalog": {
                "models": [
                    { "model": "vision", "modalities": { "input": ["text", "image"] } },
                    { "model": "text", "inputModalities": ["text"] }
                ]
            }
        });

        assert_eq!(
            image_input_capability_from_settings(&settings, "vision", true),
            ImageInputCapability::Supported
        );
        assert_eq!(
            image_input_capability_from_settings(&settings, "text", true),
            ImageInputCapability::Unsupported
        );
    }
}
