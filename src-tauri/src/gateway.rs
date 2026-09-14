//! Unified local LLM gateway configuration and routing.
//!
//! 数据模型：一个 `GatewayProvider` 记录一个上游端点（Base URL + API Key），
//! 内含一组 `GatewayProviderModel`，每条模型条目独立声明协议（api_format）、
//! 上游真实模型名与对外的本地别名。
//!
//! 路由由所有供应商下的 model 条目按 alias 聚合派生：同 alias 的多条目构成
//! 一条 failover 链，顺序 = 供应商顺序 + 供应商内条目顺序。

use crate::database::Database;
use crate::error::AppError;
use crate::provider::{LocalProxyRequestOverrides, Provider, ProviderMeta};
use crate::proxy::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use crate::proxy::types::{ProxyServerInfo, ProxyStatus};
use crate::services::model_fetch;
use crate::store::AppState;
use futures::StreamExt;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;
use tauri::menu::{Menu, MenuBuilder, MenuItem};
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

const CONFIG_KEY: &str = "unified_gateway_config_v1";
const GENERATED_CATEGORY: &str = "unified_gateway";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GatewayApiFormat {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
}

pub fn routing_weights_for_alias(
    config: &GatewayConfig,
    alias: &str,
) -> HashMap<String, u32> {
    config
        .routing_weights
        .get(alias.trim())
        .cloned()
        .unwrap_or_default()
}

#[tauri::command]
pub fn resolve_gateway_model_capabilities(
    request: ResolveGatewayModelCapabilitiesRequest,
) -> GatewayModelCapabilityResolution {
    let canonical_model = crate::model_capabilities::canonical_model_key(&request.model_id);
    let format = request.api_format.unwrap_or(GatewayApiFormat::OpenaiChat);
    match crate::model_capabilities::registry_model_capabilities(
        &request.model_id,
        Some(format.as_wire_name()),
    ) {
        Some(capabilities) => GatewayModelCapabilityResolution {
            canonical_model,
            metadata: GatewayModelMetadata {
                context_length: capabilities.context_length,
                max_output_tokens: capabilities.max_output_tokens,
                input_modalities: capabilities.input_modalities,
                reasoning_levels: capabilities.reasoning_levels,
            },
            default_reasoning_level: capabilities.default_reasoning_level,
            source: "registry".to_string(),
        },
        None => GatewayModelCapabilityResolution {
            canonical_model,
            metadata: GatewayModelMetadata::default(),
            default_reasoning_level: None,
            source: "unknown".to_string(),
        },
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GatewayRoutingPolicy {
    #[default]
    Priority,
    RoundRobin,
    WeightedRoundRobin,
    LeastOutstanding,
}

impl GatewayApiFormat {
    pub fn as_wire_name(&self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai_chat",
            Self::OpenaiResponses => "openai_responses",
            Self::Anthropic => "anthropic",
        }
    }

    fn generated_suffix(&self) -> &'static str {
        match self {
            Self::OpenaiChat => "chat",
            Self::OpenaiResponses => "responses",
            Self::Anthropic => "anthropic",
        }
    }
}

fn effective_provider_model_metadata(
    provider: &GatewayProvider,
    model: &GatewayProviderModel,
    registry_overrides: &HashMap<String, GatewayModelMetadata>,
) -> GatewayModelMetadata {
    let mut metadata = GatewayModelMetadata::default();
    metadata.apply_registry_fallback(&model.upstream_model, model.api_format);
    let canonical = crate::model_capabilities::canonical_model_key(&model.upstream_model);
    if let Some(global_override) = registry_overrides.get(&canonical) {
        metadata.apply_explicit_override(global_override);
    }
    if let Some(cached) = provider
        .cached_models
        .iter()
        .find(|cached| cached.id == model.upstream_model)
    {
        metadata.constrain_with_provider_metadata(&cached.metadata);
    }
    metadata.apply_explicit_override(&model.metadata);
    metadata
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayCachedModel {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub metadata: GatewayModelMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_levels: Vec<String>,
}

impl GatewayModelMetadata {
    /// Merge capabilities for one public alias that can fail over across several
    /// upstream providers. Numeric limits use the minimum value only when every
    /// target reports one; list capabilities use intersection. Unknown metadata
    /// removes that public guarantee rather than over-advertising a capability
    /// that a failover target may not actually support.
    fn merge_failover_capabilities(&mut self, other: &Self) {
        self.context_length = min_known(self.context_length, other.context_length);
        self.max_output_tokens = min_known(self.max_output_tokens, other.max_output_tokens);
        merge_known_intersection(&mut self.input_modalities, &other.input_modalities);
        merge_known_intersection(&mut self.reasoning_levels, &other.reasoning_levels);
    }

    fn apply_registry_fallback(&mut self, model_id: &str, api_format: GatewayApiFormat) {
        let Some(fallback) = crate::model_capabilities::registry_model_capabilities(
            model_id,
            Some(api_format.as_wire_name()),
        ) else {
            return;
        };
        if self.context_length.is_none() {
            self.context_length = fallback.context_length;
        }
        if self.max_output_tokens.is_none() {
            self.max_output_tokens = fallback.max_output_tokens;
        }
        if self.input_modalities.is_empty() {
            self.input_modalities = fallback.input_modalities;
        }
        if self.reasoning_levels.is_empty() {
            self.reasoning_levels = fallback.reasoning_levels;
        }
    }

    /// Provider `/models` metadata constrains the canonical model capability.
    /// Missing provider fields mean "not reported" and therefore do not erase
    /// registry knowledge; explicit provider limits are treated conservatively.
    fn constrain_with_provider_metadata(&mut self, provider: &Self) {
        if let Some(value) = provider.context_length {
            self.context_length = Some(
                self.context_length
                    .map(|current| current.min(value))
                    .unwrap_or(value),
            );
        }
        if let Some(value) = provider.max_output_tokens {
            self.max_output_tokens = Some(
                self.max_output_tokens
                    .map(|current| current.min(value))
                    .unwrap_or(value),
            );
        }
        if !provider.input_modalities.is_empty() {
            if self.input_modalities.is_empty() {
                self.input_modalities = provider.input_modalities.clone();
            } else {
                self.input_modalities
                    .retain(|value| provider.input_modalities.contains(value));
            }
        }
        if !provider.reasoning_levels.is_empty() {
            if self.reasoning_levels.is_empty() {
                self.reasoning_levels = provider.reasoning_levels.clone();
            } else {
                self.reasoning_levels
                    .retain(|value| provider.reasoning_levels.contains(value));
            }
        }
    }

    /// Legacy/per-route metadata is retained only as an explicit advanced
    /// override. Empty fields continue to inherit automatic capability data.
    fn apply_explicit_override(&mut self, override_metadata: &Self) {
        if override_metadata.context_length.is_some() {
            self.context_length = override_metadata.context_length;
        }
        if override_metadata.max_output_tokens.is_some() {
            self.max_output_tokens = override_metadata.max_output_tokens;
        }
        if !override_metadata.input_modalities.is_empty() {
            self.input_modalities = override_metadata.input_modalities.clone();
        }
        if !override_metadata.reasoning_levels.is_empty() {
            self.reasoning_levels = override_metadata.reasoning_levels.clone();
        }
    }
}

fn min_known(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        _ => None,
    }
}

fn merge_known_intersection(left: &mut Vec<String>, right: &[String]) {
    if left.is_empty() || right.is_empty() {
        left.clear();
        return;
    }
    left.retain(|value| right.iter().any(|candidate| candidate == value));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayProviderModel {
    pub alias: String,
    pub upstream_model: String,
    pub api_format: GatewayApiFormat,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 单模型级请求体录制开关：Some(true) 强制开启，Some(false) 强制关闭，
    /// None 跟随供应商级 recordBodies。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_bodies: Option<bool>,
    #[serde(default)]
    pub metadata: GatewayModelMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
    #[serde(default)]
    pub custom_user_agent: String,
    #[serde(default)]
    pub models_url: String,
    #[serde(default)]
    pub cached_models: Vec<GatewayCachedModel>,
    #[serde(default)]
    pub models_fetched_at: Option<String>,
    #[serde(default)]
    pub custom_headers: HashMap<String, String>,
    #[serde(default)]
    pub impersonate_codex_client: bool,
    #[serde(default)]
    pub codex_client_version: String,
    /// Claude -> OpenAI reasoning 请求参数映射：auto / force / disabled。
    #[serde(default = "default_auto_mode")]
    pub reasoning_request_mode: String,
    /// Claude -> OpenAI Chat 历史 reasoning 回传：auto / reasoning_content / disabled。
    #[serde(default = "default_auto_mode")]
    pub reasoning_history_mode: String,
    /// 原生 Anthropic adaptive thinking 展示：auto / summarized / omitted。
    #[serde(default = "default_auto_mode")]
    pub adaptive_thinking_display: String,
    #[serde(default)]
    pub notes: String,
    /// 该供应商下的模型条目，协议下沉到条目上。
    #[serde(default)]
    pub models: Vec<GatewayProviderModel>,
    /// 请求体录制（诊断用）：开启后把发往该供应商的最终请求体与
    /// 上游最终响应体转储为 JSONL 文件，用于核对中转站实际收到的内容。
    #[serde(default)]
    pub record_bodies: bool,
    /// Provider 级并发上限；0 表示不限。
    #[serde(default)]
    pub max_concurrent_requests: u32,
    /// 达到并发上限后允许排队的请求数；仅在 max_concurrent_requests > 0 时生效。
    #[serde(default = "default_queue_limit")]
    pub queue_limit: u32,
    /// 排队等待并发名额的最长时间（毫秒）。
    #[serde(default = "default_queue_timeout_ms")]
    pub queue_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    pub listen_address: String,
    pub listen_port: u16,
    #[serde(default = "default_true")]
    pub require_auth: bool,
    #[serde(default = "generate_local_key")]
    pub local_api_key: String,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default = "default_true")]
    pub enable_logging: bool,
    #[serde(default)]
    pub routing_policies: HashMap<String, GatewayRoutingPolicy>,
    /// alias -> source provider id -> weight。仅 WeightedRoundRobin 使用。
    #[serde(default)]
    pub routing_weights: HashMap<String, HashMap<String, u32>>,
    /// Canonical model-level manual corrections. These apply across all
    /// providers/routes that resolve to the same canonical model id.
    #[serde(default)]
    pub model_registry_overrides: HashMap<String, GatewayModelMetadata>,
    #[serde(default)]
    pub providers: Vec<GatewayProvider>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_address: "127.0.0.1".to_string(),
            listen_port: 10888,
            require_auth: true,
            local_api_key: generate_local_key(),
            auto_start: false,
            enable_logging: true,
            routing_policies: HashMap::new(),
            routing_weights: HashMap::new(),
            model_registry_overrides: HashMap::new(),
            providers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySnapshot {
    pub config: GatewayConfig,
    pub status: ProxyStatus,
    pub provider_runtime: Vec<GatewayProviderRuntimeStatus>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayProviderRuntimeStatus {
    pub source_provider_id: String,
    pub materialized_provider_id: String,
    pub provider_name: String,
    pub api_format: GatewayApiFormat,
    pub app_type: String,
    pub circuit_state: String,
    pub cooldown_seconds: Option<u64>,
    pub consecutive_failures: u32,
    pub total_requests: u32,
    pub max_concurrent_requests: u32,
    pub active_requests: u32,
    pub queued_requests: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelFetchResult {
    pub models: Vec<GatewayCachedModel>,
    pub fetched_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveGatewayModelCapabilitiesRequest {
    pub model_id: String,
    #[serde(default)]
    pub api_format: Option<GatewayApiFormat>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelCapabilityResolution {
    pub canonical_model: String,
    pub metadata: GatewayModelMetadata,
    pub default_reasoning_level: Option<String>,
    pub source: String,
}

fn default_true() -> bool {
    true
}

fn default_auth_style() -> String {
    "auto".to_string()
}

fn default_auto_mode() -> String {
    "auto".to_string()
}

fn default_queue_limit() -> u32 {
    32
}

fn default_queue_timeout_ms() -> u64 {
    30_000
}

pub fn generate_local_key() -> String {
    format!("local-sk-{}", Uuid::new_v4().simple())
}

pub fn load_config(db: &Database) -> Result<GatewayConfig, AppError> {
    match db.get_setting(CONFIG_KEY)? {
        Some(raw) => parse_config_with_migration(&raw)
            .map_err(|e| AppError::Config(format!("统一网关配置解析失败: {e}"))),
        None => Ok(GatewayConfig::default()),
    }
}

/// 先按新结构解析；失败或未含 `models` 时，按旧结构（providers 顶层带 apiFormat +
/// 顶层 routes[].targets[]）解析并转换为新结构。
fn parse_config_with_migration(raw: &str) -> Result<GatewayConfig, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;

    let has_new_models = value
        .get("providers")
        .and_then(|p| p.as_array())
        .map(|arr| arr.iter().any(|p| p.get("models").is_some()))
        .unwrap_or(false);
    let has_legacy_top_routes = value.get("routes").is_some();
    let has_legacy_provider_api_format = value
        .get("providers")
        .and_then(|p| p.as_array())
        .map(|arr| arr.iter().any(|p| p.get("apiFormat").is_some()))
        .unwrap_or(false);

    if has_new_models && !has_legacy_top_routes {
        return serde_json::from_value(value).map_err(|e| e.to_string());
    }

    if has_legacy_top_routes || has_legacy_provider_api_format {
        return migrate_legacy_config(&value).map_err(|e| e.to_string());
    }

    // 空 providers 或既无新字段也无旧字段：交给默认反序列化。
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn migrate_legacy_config(value: &Value) -> Result<GatewayConfig, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LegacyTarget {
        provider_id: String,
        #[serde(default)]
        upstream_model: String,
        #[serde(default = "default_true")]
        enabled: bool,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LegacyRoute {
        #[serde(default)]
        alias: String,
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        targets: Vec<LegacyTarget>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LegacyProvider {
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        base_url: String,
        #[serde(default)]
        api_key: String,
        #[serde(default)]
        api_format: Option<GatewayApiFormat>,
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_auth_style")]
        auth_style: String,
        #[serde(default)]
        custom_user_agent: String,
        #[serde(default)]
        models_url: String,
        #[serde(default)]
        cached_models: Vec<GatewayCachedModel>,
        #[serde(default)]
        models_fetched_at: Option<String>,
        #[serde(default)]
        custom_headers: HashMap<String, String>,
        #[serde(default)]
        impersonate_codex_client: bool,
        #[serde(default)]
        codex_client_version: String,
        #[serde(default)]
        notes: String,
        #[serde(default)]
        models: Vec<GatewayProviderModel>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LegacyConfig {
        listen_address: String,
        listen_port: u16,
        #[serde(default = "default_true")]
        require_auth: bool,
        #[serde(default = "generate_local_key")]
        local_api_key: String,
        #[serde(default)]
        auto_start: bool,
        #[serde(default = "default_true")]
        enable_logging: bool,
        #[serde(default)]
        providers: Vec<LegacyProvider>,
        #[serde(default)]
        routes: Vec<LegacyRoute>,
    }

    let legacy: LegacyConfig =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;

    let default_format_by_provider: HashMap<String, GatewayApiFormat> = legacy
        .providers
        .iter()
        .map(|p| (p.id.clone(), p.api_format.unwrap_or(GatewayApiFormat::OpenaiChat)))
        .collect();

    let mut provider_models: HashMap<String, Vec<GatewayProviderModel>> = HashMap::new();

    for provider in &legacy.providers {
        provider_models.insert(provider.id.clone(), provider.models.clone());
    }

    for route in legacy.routes {
        if route.alias.trim().is_empty() {
            continue;
        }
        for target in route.targets {
            if target.upstream_model.trim().is_empty() {
                continue;
            }
            let format = default_format_by_provider
                .get(&target.provider_id)
                .copied()
                .unwrap_or(GatewayApiFormat::OpenaiChat);
            provider_models
                .entry(target.provider_id.clone())
                .or_default()
                .push(GatewayProviderModel {
                    alias: route.alias.clone(),
                    upstream_model: target.upstream_model.clone(),
                    api_format: format,
                    enabled: route.enabled && target.enabled,
                    record_bodies: None,
                    metadata: GatewayModelMetadata::default(),
                });
        }
    }

    let providers = legacy
        .providers
        .into_iter()
        .map(|p| GatewayProvider {
            id: p.id.clone(),
            name: p.name,
            base_url: p.base_url,
            api_key: p.api_key,
            enabled: p.enabled,
            auth_style: p.auth_style,
            custom_user_agent: p.custom_user_agent,
            models_url: p.models_url,
            cached_models: p.cached_models,
            models_fetched_at: p.models_fetched_at,
            custom_headers: p.custom_headers,
            impersonate_codex_client: p.impersonate_codex_client,
            codex_client_version: p.codex_client_version,
            reasoning_request_mode: default_auto_mode(),
            reasoning_history_mode: default_auto_mode(),
            adaptive_thinking_display: default_auto_mode(),
            notes: p.notes,
            record_bodies: false,
            max_concurrent_requests: 0,
            queue_limit: default_queue_limit(),
            queue_timeout_ms: default_queue_timeout_ms(),
            models: provider_models.remove(&p.id).unwrap_or_default(),
        })
        .collect();
    Ok(GatewayConfig {
        listen_address: legacy.listen_address,
        listen_port: legacy.listen_port,
        require_auth: legacy.require_auth,
        local_api_key: legacy.local_api_key,
        auto_start: legacy.auto_start,
        enable_logging: legacy.enable_logging,
        routing_policies: HashMap::new(),
        routing_weights: HashMap::new(),
        model_registry_overrides: HashMap::new(),
        providers,
    })
}

fn normalize_config(mut config: GatewayConfig) -> GatewayConfig {
    config.listen_address = config.listen_address.trim().to_string();
    config.local_api_key = config.local_api_key.trim().to_string();
    config.routing_policies = config
        .routing_policies
        .into_iter()
        .filter_map(|(alias, policy)| {
            let alias = alias.trim().to_string();
            (!alias.is_empty()).then_some((alias, policy))
        })
        .collect();
    config.routing_weights = config
        .routing_weights
        .into_iter()
        .filter_map(|(alias, weights)| {
            let alias = alias.trim().to_string();
            if alias.is_empty() {
                return None;
            }
            let weights = weights
                .into_iter()
                .filter_map(|(provider_id, weight)| {
                    let provider_id = provider_id.trim().to_string();
                    (!provider_id.is_empty() && weight > 0).then_some((provider_id, weight))
                })
                .collect::<HashMap<_, _>>();
            Some((alias, weights))
        })
        .collect();
    config.model_registry_overrides = config
        .model_registry_overrides
        .into_iter()
        .filter_map(|(model_id, mut metadata)| {
            let canonical = crate::model_capabilities::canonical_model_key(&model_id);
            if canonical.is_empty() {
                return None;
            }
            normalize_model_metadata(&mut metadata);
            Some((canonical, metadata))
        })
        .collect();
    for provider in &mut config.providers {
        provider.id = provider.id.trim().to_string();
        provider.name = provider.name.trim().to_string();
        provider.base_url = provider.base_url.trim().trim_end_matches('/').to_string();
        provider.api_key = provider.api_key.trim().to_string();
        provider.auth_style = provider.auth_style.trim().to_ascii_lowercase();
        provider.custom_user_agent = provider.custom_user_agent.trim().to_string();
        provider.codex_client_version = provider.codex_client_version.trim().to_string();
        provider.reasoning_request_mode =
            provider.reasoning_request_mode.trim().to_ascii_lowercase();
        provider.reasoning_history_mode =
            provider.reasoning_history_mode.trim().to_ascii_lowercase();
        provider.adaptive_thinking_display = provider
            .adaptive_thinking_display
            .trim()
            .to_ascii_lowercase();
        provider.models_url = provider.models_url.trim().to_string();
        provider.notes = provider.notes.trim().to_string();
        provider.cached_models.sort_by(|a, b| a.id.cmp(&b.id));
        provider.cached_models.dedup_by(|a, b| a.id == b.id);
        provider.custom_headers = provider
            .custom_headers
            .drain()
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .filter(|(key, _)| !key.is_empty())
            .collect();
        for model in &mut provider.models {
            model.alias = model.alias.trim().to_string();
            model.upstream_model = model.upstream_model.trim().to_string();
            normalize_model_metadata(&mut model.metadata);
        }
    }
    config
}

fn normalize_model_metadata(metadata: &mut GatewayModelMetadata) {
    metadata.input_modalities = metadata
        .input_modalities
        .drain(..)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    metadata.input_modalities.sort();
    metadata.input_modalities.dedup();
    metadata.reasoning_levels = metadata
        .reasoning_levels
        .drain(..)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    metadata.reasoning_levels.sort();
    metadata.reasoning_levels.dedup();
}

pub fn routing_policy_for_alias(config: &GatewayConfig, alias: &str) -> GatewayRoutingPolicy {
    config
        .routing_policies
        .get(alias.trim())
        .copied()
        .unwrap_or_default()
}

fn validate_config(config: &GatewayConfig) -> Result<(), String> {
    if config.listen_address.trim().is_empty() {
        return Err("监听地址不能为空".to_string());
    }
    config
        .listen_address
        .parse::<std::net::IpAddr>()
        .map_err(|_| "监听地址必须是 IPv4 或 IPv6 地址".to_string())?;
    if config.listen_port == 0 {
        return Err("监听端口必须在 1-65535 范围内".to_string());
    }
    if config.require_auth && config.local_api_key.trim().is_empty() {
        return Err("启用本地鉴权时，本地 API Key 不能为空".to_string());
    }

    let mut provider_ids = HashSet::new();
    for provider in &config.providers {
        if !matches!(
            provider.reasoning_request_mode.as_str(),
            "auto" | "force" | "disabled"
        ) {
            return Err(format!(
                "供应商 {} 的推理请求模式无效: {}",
                provider.name, provider.reasoning_request_mode
            ));
        }
        if !matches!(
            provider.reasoning_history_mode.as_str(),
            "auto" | "reasoning_content" | "disabled"
        ) {
            return Err(format!(
                "供应商 {} 的推理历史模式无效: {}",
                provider.name, provider.reasoning_history_mode
            ));
        }
        if !matches!(
            provider.adaptive_thinking_display.as_str(),
            "auto" | "summarized" | "omitted"
        ) {
            return Err(format!(
                "供应商 {} 的 adaptive thinking 展示模式无效: {}",
                provider.name, provider.adaptive_thinking_display
            ));
        }

        if provider.id.trim().is_empty() || provider.name.trim().is_empty() {
            return Err("供应商 ID 和名称不能为空".to_string());
        }
        if provider.id.contains("::") {
            return Err(format!(
                "供应商 ID {} 不能包含 :: 分隔符（保留给内部路由使用）",
                provider.id
            ));
        }
        if !provider_ids.insert(provider.id.clone()) {
            return Err(format!("供应商 ID 重复: {}", provider.id));
        }
        if provider.base_url.trim().is_empty() {
            return Err(format!("供应商 {} 缺少 Base URL", provider.name));
        }
        let parsed_url = url::Url::parse(provider.base_url.trim())
            .map_err(|_| format!("供应商 {} 的 Base URL 无效", provider.name))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(format!("供应商 {} 的 Base URL 仅支持 HTTP/HTTPS", provider.name));
        }
        if !provider.models_url.trim().is_empty() {
            let models_url = url::Url::parse(provider.models_url.trim())
                .map_err(|_| format!("供应商 {} 的模型列表 URL 无效", provider.name))?;
            if !matches!(models_url.scheme(), "http" | "https") {
                return Err(format!("供应商 {} 的模型列表 URL 仅支持 HTTP/HTTPS", provider.name));
            }
        }
        if crate::provider::parse_custom_user_agent(Some(&provider.custom_user_agent)).is_err() {
            return Err(format!("供应商 {} 的 User-Agent 包含非法控制字符", provider.name));
        }
        if provider.impersonate_codex_client && !provider.codex_client_version.trim().is_empty() {
            HeaderValue::from_str(provider.codex_client_version.trim()).map_err(|_| {
                format!("供应商 {} 的 Codex 版本号包含非法字符", provider.name)
            })?;
        }
        for (name, value) in &provider.custom_headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "x-api-key" | "host" | "content-length" | "user-agent"
            ) {
                return Err(format!(
                    "供应商 {} 的请求头 {} 应使用专用配置项，而不是自定义请求头",
                    provider.name, name
                ));
            }
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                format!("供应商 {} 的请求头名称无效: {}", provider.name, name)
            })?;
            HeaderValue::from_str(value).map_err(|_| {
                format!("供应商 {} 的请求头值无效: {}", provider.name, name)
            })?;
        }
        if !matches!(provider.auth_style.as_str(), "auto" | "bearer" | "x-api-key") {
            return Err(format!("供应商 {} 的鉴权方式无效", provider.name));
        }

        let mut alias_seen = HashSet::new();
        for model in &provider.models {
            if model.alias.trim().is_empty() {
                return Err(format!("供应商 {} 存在空的本地别名", provider.name));
            }
            if model.upstream_model.trim().is_empty() {
                return Err(format!(
                    "供应商 {} 的模型 {} 缺少上游模型名",
                    provider.name, model.alias
                ));
            }
            if !alias_seen.insert(model.alias.clone()) {
                return Err(format!(
                    "供应商 {} 内本地别名重复: {}（同供应商同别名请合并到一条）",
                    provider.name, model.alias
                ));
            }
        }
    }

    Ok(())
}

/// 返回给定 (provider, api_format) 组合下的 `alias → upstream_model` 精确映射。
fn provider_model_map(
    provider: &GatewayProvider,
    format: GatewayApiFormat,
) -> Map<String, Value> {
    let mut map = Map::new();
    for model in &provider.models {
        if !model.enabled || model.api_format != format {
            continue;
        }
        map.insert(
            model.alias.trim().to_string(),
            Value::String(model.upstream_model.trim().to_string()),
        );
    }
    map
}

fn generated_provider_id(provider_id: &str, format: GatewayApiFormat) -> String {
    format!("{}::{}", provider_id, format.generated_suffix())
}

fn provider_meta(
    provider: &GatewayProvider,
    format: GatewayApiFormat,
) -> ProviderMeta {
    let mut meta = ProviderMeta::default();
    meta.gateway_source_provider_id = Some(provider.id.clone());
    meta.gateway_max_concurrent_requests = Some(provider.max_concurrent_requests);
    meta.gateway_queue_limit = Some(provider.queue_limit);
    meta.gateway_queue_timeout_ms = Some(provider.queue_timeout_ms);
    meta.api_format = Some(format.as_wire_name().to_string());
    meta.reasoning_request_mode = Some(provider.reasoning_request_mode.clone());
    meta.reasoning_history_mode = Some(provider.reasoning_history_mode.clone());
    meta.adaptive_thinking_display =
        Some(provider.adaptive_thinking_display.clone());
    meta.api_key_field = match provider.auth_style.as_str() {
        "x-api-key" => Some("ANTHROPIC_API_KEY".to_string()),
        "bearer" => Some("ANTHROPIC_AUTH_TOKEN".to_string()),
        _ if format == GatewayApiFormat::Anthropic => Some("ANTHROPIC_API_KEY".to_string()),
        _ => Some("ANTHROPIC_AUTH_TOKEN".to_string()),
    };
    let (user_agent, fingerprint_headers) = client_fingerprint(provider);
    meta.custom_user_agent = user_agent;

    let mut override_headers = provider.custom_headers.clone();
    override_headers.extend(fingerprint_headers);
    if !override_headers.is_empty() {
        meta.local_proxy_request_overrides = Some(LocalProxyRequestOverrides {
            headers: override_headers,
            body: None,
        });
    }

    // 请求体录制开关物化：None=关闭；Some(空列表)=全量录制；
    // Some(非空列表)=仅录制命中的出站模型。模型级 record_bodies 优先于
    // 供应商级开关（true 强制开，false 强制排除）。
    let mut forced_on_models: Vec<String> = Vec::new();
    let mut normal_models: Vec<String> = Vec::new();
    let mut forced_off_count = 0usize;
    for model in &provider.models {
        if !model.enabled || model.api_format != format {
            continue;
        }
        let upstream = model.upstream_model.trim().to_string();
        match model.record_bodies {
            Some(true) => forced_on_models.push(upstream),
            Some(false) => forced_off_count += 1,
            None => normal_models.push(upstream),
        }
    }
    meta.body_recording_models = if provider.record_bodies {
        if forced_off_count == 0 {
            // 无排除项 → 全量录制
            Some(Vec::new())
        } else {
            let mut models = normal_models;
            models.extend(forced_on_models);
            // 全部被排除 → 关闭该协议的录制（空列表语义是"全量"，不能复用）
            (!models.is_empty()).then_some(models)
        }
    } else {
        (!forced_on_models.is_empty()).then_some(forced_on_models)
    };
    meta
}

/// 计算一个供应商对外发请求时应使用的“客户端指纹”：
/// 返回 (有效 User-Agent, 需要额外注入的请求头)。
///
/// 与 `provider_meta` 中的注入规则保持一致，供“获取模型”“测试对话”等
/// 直连上游的场景复用，确保 `impersonateCodexClient` 开关在所有出站路径生效。
fn client_fingerprint(provider: &GatewayProvider) -> (Option<String>, HashMap<String, String>) {
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut user_agent = (!provider.custom_user_agent.trim().is_empty())
        .then(|| provider.custom_user_agent.trim().to_string());

    if provider.impersonate_codex_client {
        use crate::proxy::providers::{CODEX_OAUTH_CLIENT_VERSION, CODEX_OAUTH_ORIGINATOR};
        let version = if provider.codex_client_version.trim().is_empty() {
            CODEX_OAUTH_CLIENT_VERSION
        } else {
            provider.codex_client_version.trim()
        };
        // 用户显式填写的自定义 UA 优先；否则合成 codex_cli_rs/<version>。
        if user_agent.is_none() {
            user_agent = Some(format!("{CODEX_OAUTH_ORIGINATOR}/{version}"));
        }
        headers.insert("originator".to_string(), CODEX_OAUTH_ORIGINATOR.to_string());
        headers.insert("version".to_string(), version.to_string());
    }

    (user_agent, headers)
}

fn materialize_provider(
    provider: &GatewayProvider,
    format: GatewayApiFormat,
    app_type: &str,
    sort_index: usize,
) -> Provider {
    let exact_model_map = provider_model_map(provider, format);
    let auth_is_x_api_key = matches!(provider.auth_style.as_str(), "x-api-key")
        || (provider.auth_style == "auto" && format == GatewayApiFormat::Anthropic);

    let settings_config = if app_type == "claude" {
        let mut env = Map::new();
        env.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            Value::String(provider.base_url.trim_end_matches('/').to_string()),
        );
        let key_name = if auth_is_x_api_key {
            "ANTHROPIC_API_KEY"
        } else {
            "ANTHROPIC_AUTH_TOKEN"
        };
        env.insert(key_name.to_string(), Value::String(provider.api_key.clone()));
        json!({
            "env": env,
            "apiFormat": format.as_wire_name(),
            "gateway_model_map": exact_model_map,
        })
    } else {
        json!({
            "base_url": provider.base_url.trim_end_matches('/'),
            "apiKey": provider.api_key.clone(),
            "apiFormat": format.as_wire_name(),
            "gateway_model_map": exact_model_map,
        })
    };

    Provider {
        id: generated_provider_id(&provider.id, format),
        name: format!("{} · {}", provider.name, format.as_wire_name()),
        settings_config,
        website_url: None,
        category: Some(GENERATED_CATEGORY.to_string()),
        created_at: Some(chrono::Utc::now().timestamp_millis()),
        sort_index: Some(sort_index),
        notes: (!provider.notes.trim().is_empty()).then(|| provider.notes.clone()),
        meta: Some(provider_meta(provider, format)),
        icon: Some(match format {
            GatewayApiFormat::Anthropic => "anthropic".to_string(),
            _ => "openai".to_string(),
        }),
        icon_color: None,
        in_failover_queue: false,
    }
}

/// 收集所有 (provider, format) 组合。保留原始供应商顺序，供应商内 formats
/// 按其首次出现顺序排列，用作路由 failover 的稳定顺序。
fn iter_materialized_combos(
    config: &GatewayConfig,
) -> Vec<(usize, &GatewayProvider, GatewayApiFormat)> {
    let mut result = Vec::new();
    for (idx, provider) in config.providers.iter().enumerate() {
        let mut seen: Vec<GatewayApiFormat> = Vec::new();
        for model in &provider.models {
            if !model.enabled {
                continue;
            }
            if !seen.contains(&model.api_format) {
                seen.push(model.api_format);
            }
        }
        for format in seen {
            result.push((idx, provider, format));
        }
    }
    result
}

pub(crate) async fn gateway_runtime_statuses_for_router(
    db: &Database,
    router: &crate::proxy::ProviderRouter,
) -> Result<Vec<GatewayProviderRuntimeStatus>, AppError> {
    let config = load_config(db)?;
    let mut result = Vec::new();
    for (_, provider, format) in iter_materialized_combos(&config) {
        if !provider.enabled {
            continue;
        }
        let materialized_provider_id = generated_provider_id(&provider.id, format);
        let (_, active_requests, queued_requests) =
            router.provider_capacity_snapshot(&provider.id);
        for app_type in ["claude", "codex"] {
            let stats = router
                .get_circuit_breaker_stats(&materialized_provider_id, app_type)
                .await;
            let cooldown_seconds = router
                .provider_cooldown_remaining_seconds(&materialized_provider_id, app_type)
                .await;
            result.push(GatewayProviderRuntimeStatus {
                source_provider_id: provider.id.clone(),
                materialized_provider_id: materialized_provider_id.clone(),
                provider_name: provider.name.clone(),
                api_format: format,
                app_type: app_type.to_string(),
                circuit_state: stats
                    .as_ref()
                    .map(|value| value.state.to_string())
                    .unwrap_or_else(|| "closed".to_string()),
                cooldown_seconds,
                consecutive_failures: stats
                    .as_ref()
                    .map(|value| value.consecutive_failures)
                    .unwrap_or(0),
                total_requests: stats.as_ref().map(|value| value.total_requests).unwrap_or(0),
                max_concurrent_requests: provider.max_concurrent_requests,
                active_requests,
                queued_requests,
            });
        }
    }
    Ok(result)
}

fn sync_generated_providers(db: &Database, config: &GatewayConfig) -> Result<(), AppError> {
    let combos = iter_materialized_combos(config);
    let wanted: HashSet<String> = combos
        .iter()
        .map(|(_, provider, format)| generated_provider_id(&provider.id, *format))
        .collect();

    for app_type in ["claude", "codex"] {
        let existing = db.get_all_providers(app_type)?;
        for provider in existing.values() {
            if provider.category.as_deref() == Some(GENERATED_CATEGORY)
                && !wanted.contains(&provider.id)
            {
                db.delete_provider(app_type, &provider.id)?;
            }
        }

        for (index, (_, provider, format)) in combos.iter().enumerate() {
            db.save_provider(
                app_type,
                &materialize_provider(provider, *format, app_type, index),
            )?;
        }
    }
    Ok(())
}

pub fn resolve_route_providers(
    db: &Database,
    app_type: &str,
    _downstream_format: Option<&str>,
    alias: &str,
) -> Result<Option<(Vec<Provider>, GatewayRoutingPolicy, HashMap<String, u32>)>, AppError> {
    let config = load_config(db)?;
    let alias = alias.trim();
    if alias.is_empty() {
        return Ok(None);
    }
    let routing_policy = routing_policy_for_alias(&config, alias);
    let routing_weights = routing_weights_for_alias(&config, alias);

    let mut matched_ids: Vec<String> = Vec::new();
    let mut any_alias_defined = false;
    for provider in &config.providers {
        let mut formats_for_alias: Vec<GatewayApiFormat> = Vec::new();
        for model in &provider.models {
            if model.alias == alias {
                any_alias_defined = true;
            }
            if !model.enabled || model.alias != alias {
                continue;
            }
            if !provider.enabled {
                continue;
            }
            if !formats_for_alias.contains(&model.api_format) {
                formats_for_alias.push(model.api_format);
            }
        }
        for format in formats_for_alias {
            matched_ids.push(generated_provider_id(&provider.id, format));
        }
    }

    if !any_alias_defined {
        return Ok(None);
    }

    let mut result = Vec::new();
    for id in matched_ids {
        if let Some(provider) = db.get_provider_by_id(&id, app_type)? {
            result.push(provider);
        }
    }
    Ok(Some((result, routing_policy, routing_weights)))
}

pub fn validate_local_auth(db: &Database, headers: &HeaderMap) -> Result<(), crate::proxy::ProxyError> {
    let config = load_config(db)
        .map_err(|e| crate::proxy::ProxyError::ConfigError(e.to_string()))?;
    if !config.require_auth {
        return Ok(());
    }

    let expected = config.local_api_key.trim();
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim);
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);

    let candidate = bearer.or(x_api_key).unwrap_or("");
    if !candidate.is_empty() && candidate == expected {
        Ok(())
    } else {
        Err(crate::proxy::ProxyError::AuthError(
            "本地网关 API Key 无效或缺失".to_string(),
        ))
    }
}

pub fn openai_models_response(db: &Database) -> Result<Value, AppError> {
    let config = load_config(db)?;
    let created = chrono::Utc::now().timestamp();
    // 用 BTreeMap 去重并稳定排序
    let mut aliases: BTreeMap<String, GatewayModelMetadata> = BTreeMap::new();
    for provider in &config.providers {
        if !provider.enabled {
            continue;
        }
        for model in &provider.models {
            if !model.enabled {
                continue;
            }
            let effective_metadata = effective_provider_model_metadata(
                provider,
                model,
                &config.model_registry_overrides,
            );
            aliases
                .entry(model.alias.clone())
                .and_modify(|metadata| metadata.merge_failover_capabilities(&effective_metadata))
                .or_insert(effective_metadata);
        }
    }
    let data: Vec<Value> = aliases
        .into_iter()
        .map(|(alias, metadata)| {
            let mut value = json!({
                "id": alias,
                "object": "model",
                "created": created,
                "owned_by": "local-gateway"
            });
            let object = value.as_object_mut().expect("model response is an object");
            if let Some(context_length) = metadata.context_length {
                object.insert("context_length".to_string(), json!(context_length));
            }
            if let Some(max_output_tokens) = metadata.max_output_tokens {
                object.insert("max_output_tokens".to_string(), json!(max_output_tokens));
                object.insert(
                    "max_completion_tokens".to_string(),
                    json!(max_output_tokens),
                );
            }
            if !metadata.input_modalities.is_empty() {
                object.insert(
                    "input_modalities".to_string(),
                    json!(metadata.input_modalities),
                );
            }
            if !metadata.reasoning_levels.is_empty() {
                object.insert(
                    "reasoning".to_string(),
                    json!({
                        "supported": true,
                        "levels": metadata.reasoning_levels,
                    }),
                );
            }
            value
        })
        .collect();
    Ok(json!({ "object": "list", "data": data }))
}

#[tauri::command]
pub async fn get_gateway_snapshot(
    state: tauri::State<'_, AppState>,
) -> Result<GatewaySnapshot, String> {
    let config = load_config(&state.db).map_err(|e| e.to_string())?;
    let status = state.proxy_service.get_status().await?;
    let provider_runtime = gateway_runtime_statuses_for_service(&state.proxy_service, &config).await;
    Ok(GatewaySnapshot {
        config,
        status,
        provider_runtime,
    })
}

async fn gateway_runtime_statuses_for_service(
    proxy_service: &crate::services::ProxyService,
    config: &GatewayConfig,
) -> Vec<GatewayProviderRuntimeStatus> {
    let mut result = Vec::new();
    for (_, provider, format) in iter_materialized_combos(config) {
        if !provider.enabled {
            continue;
        }
        let materialized_provider_id = generated_provider_id(&provider.id, format);
        let (_, active_requests, queued_requests) = proxy_service
            .get_provider_capacity_snapshot(&provider.id)
            .await;
        for app_type in ["claude", "codex"] {
            let stats = proxy_service
                .get_circuit_breaker_stats(&materialized_provider_id, app_type)
                .await;
            let cooldown_seconds = proxy_service
                .get_provider_cooldown_remaining_seconds(&materialized_provider_id, app_type)
                .await;
            result.push(GatewayProviderRuntimeStatus {
                source_provider_id: provider.id.clone(),
                materialized_provider_id: materialized_provider_id.clone(),
                provider_name: provider.name.clone(),
                api_format: format,
                app_type: app_type.to_string(),
                circuit_state: stats
                    .as_ref()
                    .map(|value| value.state.to_string())
                    .unwrap_or_else(|| "closed".to_string()),
                cooldown_seconds,
                consecutive_failures: stats
                    .as_ref()
                    .map(|value| value.consecutive_failures)
                    .unwrap_or(0),
                total_requests: stats.as_ref().map(|value| value.total_requests).unwrap_or(0),
                max_concurrent_requests: provider.max_concurrent_requests,
                active_requests,
                queued_requests,
            });
        }
    }
    result
}

pub(crate) async fn apply_runtime_config(state: &AppState, config: &GatewayConfig) -> Result<(), String> {
    sync_generated_providers(&state.db, config).map_err(|e| e.to_string())?;

    let mut proxy_config = state.proxy_service.get_config().await?;
    proxy_config.listen_address = config.listen_address.clone();
    proxy_config.listen_port = config.listen_port;
    proxy_config.enable_logging = config.enable_logging;
    state.proxy_service.update_config(&proxy_config).await?;

    for app_type in ["claude", "codex"] {
        if let Ok(mut app_config) = state.db.get_proxy_config_for_app(app_type).await {
            app_config.auto_failover_enabled = true;
            app_config.max_retries = 10;
            state
                .db
                .update_proxy_config_for_app(app_config)
                .await
                .map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn save_gateway_config(
    state: tauri::State<'_, AppState>,
    config: GatewayConfig,
) -> Result<(), String> {
    let config = normalize_config(config);
    validate_config(&config)?;
    apply_runtime_config(&state, &config).await?;
    let serialized = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    state
        .db
        .set_setting(CONFIG_KEY, &serialized)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn start_gateway(
    state: tauri::State<'_, AppState>,
) -> Result<ProxyServerInfo, String> {
    let config = normalize_config(load_config(&state.db).map_err(|e| e.to_string())?);
    validate_config(&config)?;
    apply_runtime_config(&state, &config).await?;
    state.proxy_service.start().await
}

#[tauri::command]
pub async fn stop_gateway(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.proxy_service.stop().await
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchProviderModelsRequest {
    pub provider: GatewayProvider,
    #[serde(default)]
    pub api_format: Option<GatewayApiFormat>,
}

#[tauri::command]
pub async fn fetch_gateway_provider_models(
    request: FetchProviderModelsRequest,
) -> Result<GatewayModelFetchResult, String> {
    let provider = request.provider;
    let (effective_user_agent, fingerprint_headers) = client_fingerprint(&provider);
    let user_agent = crate::provider::parse_custom_user_agent(effective_user_agent.as_deref())
        .map_err(|e| format!("User-Agent 无效: {e}"))?;
    // Codex 身份对在 clone 之后覆盖，确保 originator/version 与 UA 版本一致。
    let mut request_headers = provider.custom_headers.clone();
    request_headers.extend(fingerprint_headers);
    let format = request.api_format.unwrap_or(GatewayApiFormat::OpenaiChat);
    let models = model_fetch::fetch_models_with_options(
        provider.base_url.trim(),
        provider.api_key.trim(),
        false,
        (!provider.models_url.trim().is_empty()).then_some(provider.models_url.trim()),
        user_agent,
        provider.auth_style.trim(),
        format.as_wire_name(),
        &request_headers,
    )
    .await?
    .into_iter()
    .map(|model| GatewayCachedModel {
        id: model.id,
        owned_by: model.owned_by,
        display_name: model.display_name,
        metadata: GatewayModelMetadata {
            context_length: model.context_length,
            max_output_tokens: model.max_output_tokens,
            input_modalities: model.input_modalities,
            reasoning_levels: model.reasoning_levels,
        },
    })
    .collect();

    Ok(GatewayModelFetchResult {
        models,
        fetched_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[tauri::command]
pub fn generate_gateway_api_key() -> String {
    generate_local_key()
}

/// 打开请求体录制目录（`<日志目录>/request-bodies`）。
///
/// 目录不存在时先建出来——用户常在开启录制后才想去看文件，此时目录可能
/// 还没被第一次写入创建。路径口径复用 `body_recorder::recording_dir()`。
#[tauri::command]
pub async fn open_gateway_recording_folder(handle: tauri::AppHandle) -> Result<bool, String> {
    let dir = crate::proxy::body_recorder::recording_dir()
        .ok_or_else(|| "无法解析日志目录".to_string())?;

    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    handle
        .opener()
        .open_path(dir.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| format!("打开文件夹失败: {e}"))?;

    Ok(true)
}

// ============================================================================
// 单模型测试对话
// ============================================================================

const TEST_MAX_OUTPUT_TOKENS_MIN: u32 = 1;
const TEST_MAX_OUTPUT_TOKENS_MAX: u32 = 131_072;
const TEST_MESSAGE_COUNT_MAX: usize = 100;
const TEST_MESSAGE_BYTES_MAX: usize = 64 * 1024;
const TEST_MESSAGES_TOTAL_BYTES_MAX: usize = 1024 * 1024;
const TEST_RESPONSE_BYTES_MAX: usize = 16 * 1024 * 1024;
/// 单次测试的整体超时。要放得下大 max_output_tokens 的推理请求：实测上游约
/// 250 tok/s，128K 输出需要约 9 分钟，120s 会在返回结果前就掐断。取值与网关
/// 自身对上游的超时（`proxy/http_client.rs`）保持一致。
const TEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const TEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TEST_GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayTestProxyMode {
    /// 沿用当前进程内的全局代理设置（即共享 http_client）
    FollowGlobal,
    /// 忽略全局代理，本次测试强制直连
    Bypass,
    /// 使用弹窗内临时输入的代理地址
    Custom,
}

/// 测试时注入的思考/推理档位。后端按目标协议翻译为各自字段：
/// - OpenAI Chat / Responses：`reasoning_effort`（low/medium/high），关闭则不写。
/// - Anthropic：`thinking`（enabled + budget_tokens，按 max_output_tokens 比例映射），关闭为 disabled。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GatewayTestThinkingLevel {
    #[default]
    Disabled,
    Low,
    Medium,
    High,
}

impl GatewayTestThinkingLevel {
    /// OpenAI 风格 reasoning_effort 值；关闭档返回 None（不写该字段）。
    fn reasoning_effort(self) -> Option<&'static str> {
        match self {
            Self::Disabled => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
        }
    }

    /// Anthropic thinking budget_tokens，按 max_output_tokens 比例映射（低 25% / 中 50% / 高 75%）。
    fn anthropic_budget(self, max_output_tokens: u32) -> Option<u32> {
        let ratio = match self {
            Self::Disabled => return None,
            Self::Low => 0.25,
            Self::Medium => 0.50,
            Self::High => 0.75,
        };
        let budget = ((max_output_tokens as f64) * ratio).floor() as u32;
        Some(budget.max(1))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayModelTestRole {
    User,
    Assistant,
}

impl GatewayModelTestRole {
    fn as_wire_name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelTestMessage {
    pub role: GatewayModelTestRole,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelTestRequest {
    pub provider: GatewayProvider,
    pub upstream_model: String,
    #[serde(default)]
    pub alias: String,
    pub api_format: GatewayApiFormat,
    pub messages: Vec<GatewayModelTestMessage>,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub via_gateway: bool,
    pub proxy_mode: GatewayTestProxyMode,
    #[serde(default)]
    pub custom_proxy_url: String,
    #[serde(default)]
    pub thinking_level: GatewayTestThinkingLevel,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// 流式测试：向上游请求 SSE，并把增量通过 `on_event` channel 实时推给前端。
    /// 关闭时仍走一次性的非流式请求。
    #[serde(default)]
    pub stream: bool,
}

/// 流式测试过程中推给前端的事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GatewayModelTestStreamEvent {
    /// 上游已返回响应头，流即将开始。`status` 为 HTTP 状态码。
    Start { status: u16 },
    /// 增量内容：正文与思考分开推送，同一事件里最多一个非空。
    Delta { text: String, reasoning: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelTestUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl GatewayModelTestUsage {
    fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.total_tokens.is_none()
            && self.cache_read_input_tokens.is_none()
            && self.cache_creation_input_tokens.is_none()
            && self.reasoning_tokens.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelTestResult {
    pub ok: bool,
    pub status: u16,
    pub latency_ms: u64,
    pub reply_text: String,
    /// 上游单独返回的思考/推理内容（OpenAI `reasoning_content`、Responses
    /// reasoning item、Anthropic thinking block）。与 `reply_text` 分开保存，
    /// 便于在“思考耗尽预算、未产出正文”时仍能看到模型实际返回了什么。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reasoning_text: String,
    pub raw_body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub raw_request: String,
    pub error: Option<String>,
    pub path_used: String,
    pub proxy_effective: Option<String>,
    pub finish_reason: Option<String>,
    pub length_truncated: bool,
    pub usage: Option<GatewayModelTestUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedTestResponse {
    reply_text: String,
    reasoning_text: String,
    finish_reason: Option<String>,
    length_truncated: bool,
    usage: Option<GatewayModelTestUsage>,
}

fn validate_test_request(request: &GatewayModelTestRequest) -> Result<(), String> {
    if request.upstream_model.trim().is_empty() {
        return Err("上游模型名不能为空".to_string());
    }
    if request.provider.base_url.trim().is_empty() {
        return Err("Base URL 不能为空".to_string());
    }
    if !(TEST_MAX_OUTPUT_TOKENS_MIN..=TEST_MAX_OUTPUT_TOKENS_MAX)
        .contains(&request.max_output_tokens)
    {
        return Err(format!(
            "max_output_tokens 必须在 {TEST_MAX_OUTPUT_TOKENS_MIN} 到 {TEST_MAX_OUTPUT_TOKENS_MAX} 之间"
        ));
    }
    validate_test_messages(&request.messages)?;
    Ok(())
}

fn validate_test_messages(messages: &[GatewayModelTestMessage]) -> Result<(), String> {
    if messages.is_empty() {
        return Err("测试消息不能为空".to_string());
    }
    if messages.len() > TEST_MESSAGE_COUNT_MAX {
        return Err(format!("测试消息最多 {TEST_MESSAGE_COUNT_MAX} 条"));
    }
    if messages[0].role != GatewayModelTestRole::User {
        return Err("第一条测试消息必须是 user".to_string());
    }
    if messages.last().map(|message| message.role) != Some(GatewayModelTestRole::User) {
        return Err("最后一条测试消息必须是 user".to_string());
    }

    let mut total_bytes = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if message.content.trim().is_empty() {
            return Err(format!("第 {} 条测试消息内容不能为空", index + 1));
        }
        let bytes = message.content.len();
        if bytes > TEST_MESSAGE_BYTES_MAX {
            return Err(format!(
                "第 {} 条测试消息超过 {} 字节上限",
                index + 1,
                TEST_MESSAGE_BYTES_MAX
            ));
        }
        total_bytes = total_bytes.saturating_add(bytes);
        if total_bytes > TEST_MESSAGES_TOTAL_BYTES_MAX {
            return Err(format!(
                "测试消息总长度超过 {} 字节上限",
                TEST_MESSAGES_TOTAL_BYTES_MAX
            ));
        }
    }
    Ok(())
}

fn pretty_raw_body(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string())
}

/// 把即将发送的请求 payload 格式化为可读的 JSON 字符串。
fn pretty_json_value(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).or_else(|| {
        value
            .and_then(Value::as_f64)
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number as u64)
    })
}

fn extract_structured_error(body: &Value) -> Option<String> {
    body.pointer("/error/message")
        .or_else(|| body.get("message"))
        .or_else(|| body.get("detail"))
        .and_then(|value| match value {
            Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
            other if !other.is_null() => Some(other.to_string()),
            _ => None,
        })
}

fn is_length_truncated(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some("length")
            | Some("max_tokens")
            | Some("max_output_tokens")
            | Some("model_context_window_exceeded")
    )
}

fn parse_chat_usage(usage: &Value) -> GatewayModelTestUsage {
    let mut parsed = GatewayModelTestUsage {
        input_tokens: json_u64(usage.get("prompt_tokens")).or_else(|| json_u64(usage.get("input_tokens"))),
        output_tokens: json_u64(usage.get("completion_tokens"))
            .or_else(|| json_u64(usage.get("output_tokens"))),
        total_tokens: json_u64(usage.get("total_tokens")),
        cache_read_input_tokens: json_u64(usage.pointer("/prompt_tokens_details/cached_tokens"))
            .or_else(|| json_u64(usage.get("cache_read_input_tokens"))),
        cache_creation_input_tokens: json_u64(usage.get("cache_creation_input_tokens")),
        reasoning_tokens: json_u64(usage.pointer("/completion_tokens_details/reasoning_tokens"))
            .or_else(|| json_u64(usage.pointer("/output_tokens_details/reasoning_tokens"))),
    };
    if parsed.total_tokens.is_none() {
        if let (Some(input), Some(output)) = (parsed.input_tokens, parsed.output_tokens) {
            parsed.total_tokens = Some(input.saturating_add(output));
        }
    }
    parsed
}

fn parse_responses_usage(usage: &Value) -> GatewayModelTestUsage {
    let mut parsed = GatewayModelTestUsage {
        input_tokens: json_u64(usage.get("input_tokens")),
        output_tokens: json_u64(usage.get("output_tokens")),
        total_tokens: json_u64(usage.get("total_tokens")),
        cache_read_input_tokens: json_u64(usage.pointer("/input_tokens_details/cached_tokens"))
            .or_else(|| json_u64(usage.get("cache_read_input_tokens"))),
        cache_creation_input_tokens: json_u64(usage.get("cache_creation_input_tokens")),
        reasoning_tokens: json_u64(usage.pointer("/output_tokens_details/reasoning_tokens")),
    };
    if parsed.total_tokens.is_none() {
        if let (Some(input), Some(output)) = (parsed.input_tokens, parsed.output_tokens) {
            parsed.total_tokens = Some(input.saturating_add(output));
        }
    }
    parsed
}

fn parse_anthropic_usage(usage: &Value) -> GatewayModelTestUsage {
    let mut parsed = GatewayModelTestUsage {
        input_tokens: json_u64(usage.get("input_tokens")),
        output_tokens: json_u64(usage.get("output_tokens")),
        total_tokens: json_u64(usage.get("total_tokens")),
        cache_read_input_tokens: json_u64(usage.get("cache_read_input_tokens")),
        cache_creation_input_tokens: json_u64(usage.get("cache_creation_input_tokens")),
        reasoning_tokens: json_u64(usage.pointer("/output_tokens_details/reasoning_tokens")),
    };
    if parsed.total_tokens.is_none() {
        if let (Some(input), Some(output)) = (parsed.input_tokens, parsed.output_tokens) {
            parsed.total_tokens = Some(input.saturating_add(output));
        }
    }
    parsed
}

fn extract_openai_chat_reply(body: &Value) -> String {
    let content = body
        .pointer("/choices/0/message/content")
        .cloned()
        .unwrap_or(Value::Null);
    match content {
        Value::String(text) => text,
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn extract_responses_reply(body: &Value) -> String {
    if let Some(text) = body.get("output_text").and_then(Value::as_str) {
        if !text.is_empty() {
            return text.to_string();
        }
    }
    let mut out = String::new();
    if let Some(outputs) = body.get("output").and_then(Value::as_array) {
        for item in outputs {
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
        }
    }
    out
}

fn extract_anthropic_reply(body: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = body.get("content").and_then(Value::as_array) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
    }
    out
}

/// OpenAI Chat 风格思考内容：优先 `reasoning_content`，兼容部分上游的
/// `reasoning` 字符串字段。返回 `content` 之外单独存放，避免二者混淆。
fn extract_openai_chat_reasoning(body: &Value) -> String {
    body.pointer("/choices/0/message/reasoning_content")
        .or_else(|| body.pointer("/choices/0/message/reasoning"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// OpenAI Responses 风格思考内容：`output` 中 `type == "reasoning"` 的条目，
/// 汇总其 `summary`（summary_text）与 `content` 中的文本。
fn extract_responses_reasoning(body: &Value) -> String {
    let mut out = String::new();
    if let Some(outputs) = body.get("output").and_then(Value::as_array) {
        for item in outputs {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                for part in summary {
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("summary_text"))
                        .and_then(Value::as_str)
                    {
                        out.push_str(text);
                    }
                }
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
        }
    }
    out
}

/// Anthropic 风格思考内容：`content` 中 `type == "thinking"` 的 `thinking` 字段。
/// `redacted_thinking` 只有加密载荷，无法展示，跳过。
fn extract_anthropic_reasoning(body: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = body.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("thinking") {
                if let Some(text) = part.get("thinking").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

fn parse_test_response(format: GatewayApiFormat, body: &Value) -> ParsedTestResponse {
    match format {
        GatewayApiFormat::OpenaiChat => {
            let finish_reason = body
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let usage = body
                .get("usage")
                .map(parse_chat_usage)
                .filter(|usage| !usage.is_empty());
            ParsedTestResponse {
                reply_text: extract_openai_chat_reply(body),
                reasoning_text: extract_openai_chat_reasoning(body),
                length_truncated: is_length_truncated(finish_reason.as_deref()),
                finish_reason,
                usage,
            }
        }
        GatewayApiFormat::OpenaiResponses => {
            let finish_reason = body
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| body.get("status").and_then(Value::as_str))
                .map(str::to_string);
            let usage = body
                .get("usage")
                .map(parse_responses_usage)
                .filter(|usage| !usage.is_empty());
            ParsedTestResponse {
                reply_text: extract_responses_reply(body),
                reasoning_text: extract_responses_reasoning(body),
                length_truncated: is_length_truncated(finish_reason.as_deref()),
                finish_reason,
                usage,
            }
        }
        GatewayApiFormat::Anthropic => {
            let finish_reason = body
                .get("stop_reason")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let usage = body
                .get("usage")
                .map(parse_anthropic_usage)
                .filter(|usage| !usage.is_empty());
            ParsedTestResponse {
                reply_text: extract_anthropic_reply(body),
                reasoning_text: extract_anthropic_reasoning(body),
                length_truncated: is_length_truncated(finish_reason.as_deref()),
                finish_reason,
                usage,
            }
        }
    }
}

/// 单个 SSE 事件块对累积结果与前端增量的贡献。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StreamBlockEffect {
    /// 本块新增的正文
    text: String,
    /// 本块新增的思考内容
    reasoning: String,
    /// 本块的错误信息（上游在流内报错，如 Anthropic `event: error`）
    error: Option<String>,
    /// 本块是否就是本协议流的**最后一个**事件块。
    ///
    /// 注意只有真正的收尾标记才算：Chat 的 `finish_reason` 之后还有一条带 usage
    /// 的 chunk，Anthropic 的 `message_delta` 之后还有 `message_stop`，
    /// 提前收工会把 usage 丢掉。
    is_terminal: bool,
}

impl StreamBlockEffect {
    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.reasoning.is_empty() && self.error.is_none()
    }
}

/// 把增量 usage 合并进已有统计。流式响应会把 usage 拆到多个事件里
/// （如 Anthropic 在 `message_start` 给 input_tokens、`message_delta` 给
/// output_tokens），而且这些数字是**累计值**、不是分片量。
///
/// 所以合并规则是按字段「后来者覆盖」，前提是后来者真的带了这个字段：
/// 早期事件里可能是 0 或只有一半（`message_start` 的 output_tokens 常为 0），
/// 若改成「只填补缺失字段」，那个早期 0 会永远压住最终的准确值。
/// 字段缺失（`None`）时才保留上一次的值。
fn merge_test_usage(slot: &mut Option<GatewayModelTestUsage>, next: GatewayModelTestUsage) {
    if next.is_empty() {
        return;
    }
    if slot.is_none() {
        *slot = Some(next);
        return;
    }
    // 用 `as_mut()` 拿到独立借用：`let Some(x) = slot else { .. }` 会在
    // else 分支里和 `*slot` 的赋值打架。
    let Some(current) = slot.as_mut() else {
        return;
    };
    fn fill(target: &mut Option<u64>, value: Option<u64>) {
        if value.is_some() {
            *target = value;
        }
    }
    fill(&mut current.input_tokens, next.input_tokens);
    fill(&mut current.output_tokens, next.output_tokens);
    fill(&mut current.total_tokens, next.total_tokens);
    fill(
        &mut current.cache_read_input_tokens,
        next.cache_read_input_tokens,
    );
    fill(
        &mut current.cache_creation_input_tokens,
        next.cache_creation_input_tokens,
    );
    fill(&mut current.reasoning_tokens, next.reasoning_tokens);
    if current.total_tokens.is_none() {
        if let (Some(input), Some(output)) = (current.input_tokens, current.output_tokens) {
            current.total_tokens = Some(input.saturating_add(output));
        }
    }
}

/// 按目标协议解析一个 SSE 事件块：把增量累加进 `parsed`，并把增量本身返回给调用方
/// （用于实时推送给前端）。
///
/// 客户端协议与上游协议在这里是同一个，所以三种格式各写一份原生解析即可，
/// 不需要走 `proxy/` 的跨协议转换。
fn apply_stream_block(
    format: GatewayApiFormat,
    block: &str,
    parsed: &mut ParsedTestResponse,
) -> StreamBlockEffect {
    let mut effect = StreamBlockEffect::default();

    for payload in block.lines().filter_map(|line| strip_sse_field(line, "data")) {
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        // Chat 的结束标记是字面量 [DONE]，不是 JSON。
        if payload == "[DONE]" {
            effect.is_terminal = true;
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        match format {
            GatewayApiFormat::OpenaiChat => {
                if event.get("error").is_some_and(|value| !value.is_null()) {
                    effect.error = Some(
                        extract_structured_error(&event)
                            .unwrap_or_else(|| "上游在流中返回错误".to_string()),
                    );
                }
                if let Some(delta) = event.pointer("/choices/0/delta") {
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        effect.text.push_str(text);
                    }
                    let reasoning = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(Value::as_str);
                    if let Some(text) = reasoning {
                        effect.reasoning.push_str(text);
                    }
                }
                if let Some(reason) = event
                    .pointer("/choices/0/finish_reason")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    parsed.finish_reason = Some(reason.to_string());
                    parsed.length_truncated = is_length_truncated(Some(reason));
                }
                if let Some(usage) = event.get("usage").filter(|value| !value.is_null()) {
                    merge_test_usage(&mut parsed.usage, parse_chat_usage(usage));
                }
            }
            GatewayApiFormat::OpenaiResponses => {
                match event.get("type").and_then(Value::as_str) {
                    Some("response.output_text.delta") => {
                        if let Some(text) = event.get("delta").and_then(Value::as_str) {
                            effect.text.push_str(text);
                        }
                    }
                    Some(
                        "response.reasoning_summary_text.delta"
                        | "response.reasoning_text.delta",
                    ) => {
                        if let Some(text) = event.get("delta").and_then(Value::as_str) {
                            effect.reasoning.push_str(text);
                        }
                    }
                    // 终态事件携带完整的 response 对象，usage/finish_reason 都在里面。
                    Some("response.completed" | "response.incomplete") => {
                        effect.is_terminal = true;
                        if let Some(full) = event.get("response") {
                            let done = parse_test_response(GatewayApiFormat::OpenaiResponses, full);
                            if let Some(reason) = done.finish_reason {
                                parsed.finish_reason = Some(reason);
                            }
                            parsed.length_truncated = done.length_truncated;
                            if let Some(usage) = done.usage {
                                merge_test_usage(&mut parsed.usage, usage);
                            }
                            // 少数上游只在终态里给全文，不逐字发 delta。
                            if effect.text.is_empty() && parsed.reply_text.is_empty() {
                                effect.text = done.reply_text;
                            }
                            if effect.reasoning.is_empty() && parsed.reasoning_text.is_empty() {
                                effect.reasoning = done.reasoning_text;
                            }
                        }
                    }
                    Some("response.failed" | "error") => {
                        effect.error = Some(
                            event
                                .pointer("/response/error/message")
                                .or_else(|| event.pointer("/error/message"))
                                .and_then(Value::as_str)
                                .unwrap_or("上游在流中返回错误")
                                .to_string(),
                        );
                    }
                    _ => {}
                }
            }
            GatewayApiFormat::Anthropic => match event.get("type").and_then(Value::as_str) {
                Some("content_block_delta") => {
                    let delta = event.get("delta");
                    match delta.and_then(|value| value.get("type")).and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta
                                .and_then(|value| value.get("text"))
                                .and_then(Value::as_str)
                            {
                                effect.text.push_str(text);
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta
                                .and_then(|value| value.get("thinking"))
                                .and_then(Value::as_str)
                            {
                                effect.reasoning.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_start") => {
                    if let Some(usage) = event.pointer("/message/usage") {
                        merge_test_usage(&mut parsed.usage, parse_anthropic_usage(usage));
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = event
                        .pointer("/delta/stop_reason")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        parsed.finish_reason = Some(reason.to_string());
                        parsed.length_truncated = is_length_truncated(Some(reason));
                    }
                    if let Some(usage) = event.get("usage") {
                        merge_test_usage(&mut parsed.usage, parse_anthropic_usage(usage));
                    }
                }
                Some("message_stop") => effect.is_terminal = true,
                Some("error") => {
                    effect.error = Some(
                        event
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("上游在流中返回错误")
                            .to_string(),
                    );
                }
                _ => {}
            },
        }
    }

    parsed.reply_text.push_str(&effect.text);
    parsed.reasoning_text.push_str(&effect.reasoning);
    effect
}

fn chat_messages_value(messages: &[GatewayModelTestMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                json!({
                    "role": message.role.as_wire_name(),
                    "content": message.content,
                })
            })
            .collect(),
    )
}

/// 同 chat_messages_value，但可选地在最前面插一条 system 消息（OpenAI Chat/Responses 风格）。
fn chat_messages_value_with_system(
    messages: &[GatewayModelTestMessage],
    system: Option<&str>,
) -> Value {
    let mut value = chat_messages_value(messages);
    if let Some(text) = system {
        if let Value::Array(arr) = &mut value {
            arr.insert(
                0,
                json!({ "role": "system", "content": text }),
            );
        }
    }
    value
}

fn build_test_client(
    mode: GatewayTestProxyMode,
    custom_proxy_url: &str,
) -> Result<(reqwest::Client, Option<String>), String> {
    match mode {
        GatewayTestProxyMode::FollowGlobal => {
            let url = crate::proxy::http_client::get_current_proxy_url();
            Ok((crate::proxy::http_client::get(), url))
        }
        GatewayTestProxyMode::Bypass => {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(TEST_REQUEST_TIMEOUT)
                .connect_timeout(TEST_CONNECT_TIMEOUT)
                .build()
                .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
            Ok((client, None))
        }
        GatewayTestProxyMode::Custom => {
            let trimmed = custom_proxy_url.trim();
            if trimmed.is_empty() {
                return Err("自定义代理 URL 不能为空".to_string());
            }
            crate::proxy::http_client::validate_proxy(Some(trimmed))?;
            let proxy = reqwest::Proxy::all(trimmed)
                .map_err(|e| format!("代理配置无效: {e}"))?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .proxy(proxy)
                .timeout(TEST_REQUEST_TIMEOUT)
                .connect_timeout(TEST_CONNECT_TIMEOUT)
                .build()
                .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
            Ok((client, Some(trimmed.to_string())))
        }
    }
}

fn build_test_payload(
    format: GatewayApiFormat,
    model: &str,
    messages: &[GatewayModelTestMessage],
    max_output_tokens: u32,
    thinking_level: GatewayTestThinkingLevel,
    system_prompt: Option<&str>,
) -> Result<Value, String> {
    let system = system_prompt
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match format {
        GatewayApiFormat::OpenaiChat => {
            let mut payload = json!({
                "model": model,
                "messages": chat_messages_value_with_system(messages, system),
                "max_tokens": max_output_tokens,
                "stream": false,
            });
            if let Some(effort) = thinking_level.reasoning_effort() {
                payload["reasoning_effort"] = json!(effort);
            }
            Ok(payload)
        }
        GatewayApiFormat::OpenaiResponses => {
            let mut chat_shaped = json!({
                "model": model,
                "messages": chat_messages_value_with_system(messages, system),
                "max_tokens": max_output_tokens,
                "stream": false,
            });
            if let Some(effort) = thinking_level.reasoning_effort() {
                chat_shaped["reasoning_effort"] = json!(effort);
            }
            crate::gateway_chat::chat_request_to_responses(chat_shaped)
                .map_err(|error| error.to_string())
        }
        GatewayApiFormat::Anthropic => {
            let mut payload = json!({
                "model": model,
                "messages": chat_messages_value(messages),
                "max_tokens": max_output_tokens,
                "stream": false,
            });
            if let Some(text) = system {
                payload["system"] = json!(text);
            }
            match thinking_level {
                GatewayTestThinkingLevel::Disabled => {
                    payload["thinking"] = json!({ "type": "disabled" });
                }
                _ => {
                    if let Some(budget) =
                        thinking_level.anthropic_budget(max_output_tokens)
                    {
                        payload["thinking"] = json!({
                            "type": "enabled",
                            "budget_tokens": budget
                        });
                    }
                }
            }
            Ok(payload)
        }
    }
}

/// 在非流式 payload 的基础上打开流式开关。
///
/// 只改这两个字段，其余请求体与非流式路径保持逐字节一致——避免"流式/非流式"
/// 两条链路各自演化出不同的参数组合。
fn apply_test_stream(payload: &mut Value, format: GatewayApiFormat, stream: bool) {
    if !stream {
        return;
    }
    payload["stream"] = json!(true);
    if format == GatewayApiFormat::OpenaiChat {
        // Chat 流式默认不返回 usage；让上游在最后一个 chunk 里带上，
        // 否则测试页拿不到 token 统计。OpenAI 兼容上游普遍支持该字段。
        payload["stream_options"] = json!({ "include_usage": true });
    }
}

async fn read_response_text(response: reqwest::Response) -> Result<String, String> {
    let mut body = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("读取响应失败: {e}"))?;
        let bytes: &[u8] = chunk.as_ref();
        if collected.len().saturating_add(bytes.len()) > TEST_RESPONSE_BYTES_MAX {
            return Err(format!(
                "响应体超过 {} 字节上限",
                TEST_RESPONSE_BYTES_MAX
            ));
        }
        collected.extend_from_slice(bytes);
    }
    String::from_utf8(collected).map_err(|e| format!("响应不是有效 UTF-8: {e}"))
}

/// 读取 SSE 流：边收边解析，把增量推给前端，同时累积成与非流式一致的结果。
///
/// 返回 `(原始响应文本, 解析结果, 流内错误)`。原始文本保留 SSE 帧原貌，
/// 便于在测试页里直接排查上游到底发了什么。
async fn read_stream_response(
    format: GatewayApiFormat,
    response: reqwest::Response,
    status: u16,
    sink: &tauri::ipc::Channel<GatewayModelTestStreamEvent>,
) -> Result<(String, ParsedTestResponse, Option<String>), String> {
    let emit = |event: GatewayModelTestStreamEvent| {
        // 前端窗口已关闭时 send 会失败；这不该中断本次测试。
        let _ = sink.send(event);
    };
    emit(GatewayModelTestStreamEvent::Start { status });

    let mut raw = String::new();
    let mut buffer = String::new();
    let mut remainder: Vec<u8> = Vec::new();
    let mut parsed = ParsedTestResponse::default();
    let mut stream_error: Option<String> = None;
    let mut saw_any_block = false;
    let mut terminal = false;
    let mut body = response.bytes_stream();

    'stream: while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("读取响应失败: {e}"))?;
        let bytes: &[u8] = chunk.as_ref();
        if raw.len().saturating_add(bytes.len()) > TEST_RESPONSE_BYTES_MAX {
            return Err(format!("响应体超过 {} 字节上限", TEST_RESPONSE_BYTES_MAX));
        }
        raw.push_str(&String::from_utf8_lossy(bytes));
        append_utf8_safe(&mut buffer, &mut remainder, bytes);

        while let Some(block) = take_sse_block(&mut buffer) {
            saw_any_block = true;
            let effect = apply_stream_block(format, &block, &mut parsed);
            if stream_error.is_none() {
                stream_error = effect.error.clone();
            }
            if !effect.is_empty() {
                emit(GatewayModelTestStreamEvent::Delta {
                    text: effect.text,
                    reasoning: effect.reasoning,
                });
            }
            // 见到收尾标记就收工：部分上游发完 [DONE] 仍挂着连接不关，
            // 继续等只会一直耗到整体超时。
            if effect.is_terminal {
                terminal = true;
                break;
            }
        }
        if stream_error.is_some() || terminal {
            break 'stream;
        }
    }

    // 流末尾可能残留一个没有空行收尾的事件块（如直接以 `data: [DONE]` 结束）。
    if !terminal && stream_error.is_none() && !buffer.trim().is_empty() {
        saw_any_block = true;
        let effect = apply_stream_block(format, &buffer, &mut parsed);
        if !effect.is_empty() {
            emit(GatewayModelTestStreamEvent::Delta {
                text: effect.text,
                reasoning: effect.reasoning,
            });
        }
    }

    // 上游忽略 stream:true、直接回了一整个 JSON：按非流式解析，别让测试页白跑。
    if !saw_any_block {
        if let Ok(body) = serde_json::from_str::<Value>(&raw) {
            parsed = parse_test_response(format, &body);
        }
    }

    Ok((raw, parsed, stream_error))
}

fn endpoint_path(format: GatewayApiFormat) -> &'static str {
    match format {
        GatewayApiFormat::OpenaiChat => "/v1/chat/completions",
        GatewayApiFormat::OpenaiResponses => "/v1/responses",
        GatewayApiFormat::Anthropic => "/v1/messages",
    }
}

fn join_url(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    // 若 base 已经包含 /v1，则用 path 去掉前导 /v1 以避免重复
    if base.ends_with("/v1") {
        format!("{}{}", base, &path[3..])
    } else {
        format!("{}{}", base, path)
    }
}

/// 发送直连测试请求。返回响应（尚未读取响应体）与已发送的原始请求体，
/// 由调用方决定按流式还是非流式读取。
async fn run_direct_test(
    request: &GatewayModelTestRequest,
    client: &reqwest::Client,
) -> Result<(reqwest::Response, String), String> {
    let url = join_url(&request.provider.base_url, endpoint_path(request.api_format));
    let mut payload = build_test_payload(
        request.api_format,
        &request.upstream_model,
        &request.messages,
        request.max_output_tokens,
        request.thinking_level,
        request.system_prompt.as_deref(),
    )?;
    apply_test_stream(&mut payload, request.api_format, request.stream);
    let raw_request = pretty_json_value(&payload);

    let mut req = client.post(&url).json(&payload);

    let auth_style = request.provider.auth_style.trim().to_ascii_lowercase();
    let use_x_api_key = auth_style == "x-api-key"
        || (auth_style == "auto" && request.api_format == GatewayApiFormat::Anthropic);
    let key = request.provider.api_key.trim();
    if !key.is_empty() {
        if use_x_api_key {
            req = req.header("x-api-key", key);
            if request.api_format == GatewayApiFormat::Anthropic {
                req = req.header("anthropic-version", "2023-06-01");
            }
        } else {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
    } else if request.api_format == GatewayApiFormat::Anthropic {
        req = req.header("anthropic-version", "2023-06-01");
    }

    // 自定义 UA / Codex 伪装：复用与真实转发、获取模型相同的指纹规则。
    let (user_agent, fingerprint_headers) = client_fingerprint(&request.provider);
    if let Some(user_agent) = user_agent {
        req = req.header("User-Agent", user_agent);
    }

    // 自定义头（禁保留头）；Codex 身份对最后覆盖，确保 originator/version 成对一致。
    let mut request_headers = request.provider.custom_headers.clone();
    request_headers.extend(fingerprint_headers);
    for (name, value) in &request_headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization" | "x-api-key" | "host" | "content-length" | "user-agent"
        ) {
            continue;
        }
        req = req.header(name.as_str(), value.as_str());
    }

    let response = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
    Ok((response, raw_request))
}

async fn run_gateway_test(
    state: &AppState,
    request: &GatewayModelTestRequest,
    client: &reqwest::Client,
) -> Result<(reqwest::Response, String), String> {
    let config = load_config(&state.db).map_err(|e| e.to_string())?;
    let alias = if request.alias.trim().is_empty() {
        request.upstream_model.trim()
    } else {
        request.alias.trim()
    };
    if alias.is_empty() {
        return Err("通过网关测试需要一个已保存的本地别名".to_string());
    }
    let address = if config.listen_address == "0.0.0.0" || config.listen_address == "::" {
        "127.0.0.1".to_string()
    } else if config.listen_address.contains(':') && !config.listen_address.starts_with('[') {
        format!("[{}]", config.listen_address)
    } else {
        config.listen_address.clone()
    };
    let base = format!("http://{}:{}", address, config.listen_port);
    let url = format!("{}{}", base, endpoint_path(request.api_format));
    let mut payload = build_test_payload(
        request.api_format,
        alias,
        &request.messages,
        request.max_output_tokens,
        request.thinking_level,
        request.system_prompt.as_deref(),
    )?;
    apply_test_stream(&mut payload, request.api_format, request.stream);
    let raw_request = pretty_json_value(&payload);

    let mut req = client.post(&url).json(&payload);
    let local_key = config.local_api_key.trim();
    if config.require_auth && !local_key.is_empty() {
        if request.api_format == GatewayApiFormat::Anthropic {
            req = req.header("x-api-key", local_key);
            req = req.header("anthropic-version", "2023-06-01");
        } else {
            req = req.header("Authorization", format!("Bearer {}", local_key));
        }
    } else if request.api_format == GatewayApiFormat::Anthropic {
        req = req.header("anthropic-version", "2023-06-01");
    }

    let response = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
    Ok((response, raw_request))
}

fn empty_test_result(
    path_used: &str,
    latency_ms: u64,
    proxy_effective: Option<String>,
    error: Option<String>,
) -> GatewayModelTestResult {
    GatewayModelTestResult {
        ok: false,
        status: 0,
        latency_ms,
        reply_text: String::new(),
        reasoning_text: String::new(),
        raw_body: String::new(),
        raw_request: String::new(),
        error,
        path_used: path_used.to_string(),
        proxy_effective,
        finish_reason: None,
        length_truncated: false,
        usage: None,
    }
}

/// 单次测试的结果载荷：非流式与流式两条路径最终都归一到这里。
struct TestOutcome {
    status: u16,
    /// 展示用的原始响应文本（非流式是完整 JSON，流式是 SSE 帧串）
    text: String,
    raw_request: String,
    parsed: ParsedTestResponse,
    /// 流内错误（上游在 SSE 中报错）；非流式路径恒为 None
    stream_error: Option<String>,
}

#[tauri::command]
pub async fn test_gateway_model(
    state: tauri::State<'_, AppState>,
    request: GatewayModelTestRequest,
    on_event: tauri::ipc::Channel<GatewayModelTestStreamEvent>,
) -> Result<GatewayModelTestResult, String> {
    validate_test_request(&request)?;

    // 通过网关测试时，本地测试请求必须严格直连本地监听端口；真正的
    // “网关 → 上游”出站段仍由网关共享客户端按全局代理配置决定。
    let (client, proxy_effective) = if request.via_gateway {
        let local_client = reqwest::Client::builder()
            .no_proxy()
            .timeout(TEST_REQUEST_TIMEOUT)
            .connect_timeout(TEST_GATEWAY_CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("构建本地网关测试客户端失败: {e}"))?;
        (
            local_client,
            crate::proxy::http_client::get_current_proxy_url(),
        )
    } else {
        build_test_client(request.proxy_mode, &request.custom_proxy_url)?
    };

    let start = Instant::now();
    let path_used = if request.via_gateway { "gateway" } else { "direct" };
    let masked_proxy = proxy_effective
        .as_ref()
        .map(|url| crate::proxy::http_client::mask_url(url));

    let sent = if request.via_gateway {
        run_gateway_test(&state, &request, &client).await
    } else {
        run_direct_test(&request, &client).await
    };

    // 只有 2xx 才值得按 SSE 解析；错误响应体一律作为整体文本读出来，
    // 好让上游的结构化报错原样呈现。
    let outcome: Result<TestOutcome, String> = match sent {
        Err(err) => Err(err),
        Ok((response, raw_request)) => {
            let status = response.status().as_u16();
            if request.stream && (200..300).contains(&status) {
                match read_stream_response(request.api_format, response, status, &on_event).await {
                    Ok((text, parsed, stream_error)) => Ok(TestOutcome {
                        status,
                        text,
                        raw_request,
                        parsed,
                        stream_error,
                    }),
                    Err(err) => Err(err),
                }
            } else {
                match read_response_text(response).await {
                    Ok(text) => {
                        let parsed = serde_json::from_str::<Value>(&text)
                            .ok()
                            .map(|value| parse_test_response(request.api_format, &value))
                            .unwrap_or_default();
                        Ok(TestOutcome {
                            status,
                            text,
                            raw_request,
                            parsed,
                            stream_error: None,
                        })
                    }
                    Err(err) => Err(err),
                }
            }
        }
    };

    let latency_ms = start.elapsed().as_millis() as u64;

    match outcome {
        Ok(outcome) => {
            let TestOutcome {
                status,
                text,
                raw_request,
                parsed,
                stream_error,
            } = outcome;
            let ok = (200..300).contains(&status) && stream_error.is_none();
            let raw_body = pretty_raw_body(&text);
            let body: Option<Value> = serde_json::from_str(&text).ok();
            let error = if ok {
                None
            } else {
                stream_error
                    .or_else(|| body.as_ref().and_then(extract_structured_error))
                    .or_else(|| {
                        let trimmed = text.trim();
                        (!trimmed.is_empty()).then(|| trimmed.to_string())
                    })
                    .or_else(|| Some(format!("HTTP {status}")))
            };
            Ok(GatewayModelTestResult {
                ok,
                status,
                latency_ms,
                reply_text: parsed.reply_text,
                reasoning_text: parsed.reasoning_text,
                raw_body,
                raw_request,
                error,
                path_used: path_used.to_string(),
                proxy_effective: masked_proxy,
                finish_reason: parsed.finish_reason,
                length_truncated: parsed.length_truncated,
                usage: parsed.usage,
            })
        }
        Err(err) => Ok(empty_test_result(path_used, latency_ms, masked_proxy, Some(err))),
    }
}

pub fn create_gateway_tray_menu(
    app: &tauri::AppHandle,
) -> tauri::Result<Menu<tauri::Wry>> {
    let show = MenuItem::with_id(app, "gateway_show", "打开主界面", true, None::<&str>)?;
    let start = MenuItem::with_id(app, "gateway_start", "启动网关", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "gateway_stop", "停止网关", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "gateway_quit", "退出", true, None::<&str>)?;
    MenuBuilder::new(app)
        .item(&show)
        .separator()
        .item(&start)
        .item(&stop)
        .separator()
        .item(&quit)
        .build()
}

pub fn handle_gateway_tray_menu_event(app: &tauri::AppHandle, id: &str) {
    match id {
        "gateway_show" => {
            if !crate::lightweight::reveal_main_window(app)
                && crate::lightweight::is_lightweight_mode()
            {
                if let Err(error) = crate::lightweight::exit_lightweight_mode(app) {
                    log::error!("托盘唤醒主窗口失败: {error}");
                }
            }
        }
        "gateway_start" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                match load_config(&state.db) {
                    Ok(config) => {
                        if let Err(error) = apply_runtime_config(&state, &config).await {
                            log::error!("应用网关配置失败: {error}");
                        } else if let Err(error) = state.proxy_service.start().await {
                            log::error!("从托盘启动网关失败: {error}");
                        }
                    }
                    Err(error) => log::error!("读取网关配置失败: {error}"),
                }
            });
        }
        "gateway_stop" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                if let Err(error) = state.proxy_service.stop().await {
                    log::error!("从托盘停止网关失败: {error}");
                }
            });
        }
        "gateway_quit" => {
            crate::remove_tray_icon_before_exit(app);
            app.exit(0);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_metadata_merge_is_conservative_across_failover_targets() {
        let mut merged = GatewayModelMetadata {
            context_length: Some(1_000_000),
            max_output_tokens: Some(128_000),
            input_modalities: vec!["text".to_string(), "image".to_string()],
            reasoning_levels: vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ],
        };
        merged.merge_failover_capabilities(&GatewayModelMetadata {
            context_length: Some(200_000),
            max_output_tokens: Some(64_000),
            input_modalities: vec!["text".to_string()],
            reasoning_levels: vec!["medium".to_string(), "high".to_string()],
        });

        assert_eq!(merged.context_length, Some(200_000));
        assert_eq!(merged.max_output_tokens, Some(64_000));
        assert_eq!(merged.input_modalities, vec!["text"]);
        assert_eq!(merged.reasoning_levels, vec!["medium", "high"]);
    }

    #[test]
    fn unknown_metadata_removes_public_capability_guarantees() {
        let mut merged = GatewayModelMetadata {
            context_length: Some(200_000),
            max_output_tokens: Some(32_000),
            input_modalities: vec!["text".to_string(), "image".to_string()],
            reasoning_levels: vec!["high".to_string()],
        };
        merged.merge_failover_capabilities(&GatewayModelMetadata::default());

        assert_eq!(merged.context_length, None);
        assert_eq!(merged.max_output_tokens, None);
        assert!(merged.input_modalities.is_empty());
        assert!(merged.reasoning_levels.is_empty());
    }

    #[test]
    fn routing_policy_defaults_to_priority_and_supports_alias_override() {
        let mut config = GatewayConfig::default();
        assert_eq!(
            routing_policy_for_alias(&config, "best-code"),
            GatewayRoutingPolicy::Priority
        );

        config
            .routing_policies
            .insert("best-code".to_string(), GatewayRoutingPolicy::RoundRobin);
        assert_eq!(
            routing_policy_for_alias(&config, "best-code"),
            GatewayRoutingPolicy::RoundRobin
        );
    }

    #[test]
    fn registry_fallback_fills_only_missing_metadata_fields() {
        let mut metadata = GatewayModelMetadata {
            context_length: Some(123_456),
            max_output_tokens: Some(7_777),
            input_modalities: Vec::new(),
            reasoning_levels: vec!["high".to_string()],
        };

        metadata.apply_registry_fallback("gpt-5.5", GatewayApiFormat::OpenaiResponses);

        assert_eq!(metadata.context_length, Some(123_456));
        assert_eq!(metadata.max_output_tokens, Some(7_777));
        assert_eq!(metadata.input_modalities, vec!["text", "image"]);
        assert_eq!(metadata.reasoning_levels, vec!["high"]);
    }

    #[test]
    fn effective_metadata_uses_registry_then_provider_constraints_then_explicit_override() {
        let mut provider = provider_with_format("p", GatewayApiFormat::OpenaiResponses);
        provider.cached_models = vec![GatewayCachedModel {
            id: "gpt-5.6-sol".to_string(),
            owned_by: None,
            display_name: None,
            metadata: GatewayModelMetadata {
                context_length: Some(200_000),
                max_output_tokens: None,
                input_modalities: vec!["text".to_string()],
                reasoning_levels: vec!["high".to_string(), "max".to_string()],
            },
        }];
        provider.models[0].upstream_model = "gpt-5.6-sol".to_string();
        provider.models[0].api_format = GatewayApiFormat::OpenaiResponses;

        let effective = effective_provider_model_metadata(
            &provider,
            &provider.models[0],
            &HashMap::new(),
        );
        assert_eq!(effective.context_length, Some(200_000));
        assert_eq!(effective.max_output_tokens, Some(128_000));
        assert_eq!(effective.input_modalities, vec!["text"]);
        assert_eq!(effective.reasoning_levels, vec!["high", "max"]);

        provider.models[0].metadata = GatewayModelMetadata {
            context_length: Some(999_999),
            max_output_tokens: None,
            input_modalities: Vec::new(),
            reasoning_levels: vec!["medium".to_string()],
        };
        let overridden = effective_provider_model_metadata(
            &provider,
            &provider.models[0],
            &HashMap::new(),
        );
        assert_eq!(overridden.context_length, Some(999_999));
        assert_eq!(overridden.max_output_tokens, Some(128_000));
        assert_eq!(overridden.input_modalities, vec!["text"]);
        assert_eq!(overridden.reasoning_levels, vec!["medium"]);
    }

    #[test]
    fn global_registry_override_applies_across_provider_routes() {
        let mut provider = provider_with_format("p", GatewayApiFormat::OpenaiResponses);
        provider.models[0].upstream_model = "openai/gpt-5.6-sol-high".to_string();
        provider.models[0].api_format = GatewayApiFormat::OpenaiResponses;

        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.6-sol".to_string(),
            GatewayModelMetadata {
                context_length: Some(500_000),
                max_output_tokens: None,
                input_modalities: Vec::new(),
                reasoning_levels: vec!["high".to_string(), "max".to_string()],
            },
        );

        let effective =
            effective_provider_model_metadata(&provider, &provider.models[0], &overrides);
        assert_eq!(effective.context_length, Some(500_000));
        assert_eq!(effective.max_output_tokens, Some(128_000));
        assert_eq!(effective.reasoning_levels, vec!["high", "max"]);
    }

    #[test]
    fn capability_resolver_returns_canonical_model_and_default_reasoning() {
        let resolved = resolve_gateway_model_capabilities(
            ResolveGatewayModelCapabilitiesRequest {
                model_id: "openai/gpt-5.6-sol-high".to_string(),
                api_format: Some(GatewayApiFormat::OpenaiResponses),
            },
        );

        assert_eq!(resolved.canonical_model, "gpt-5.6-sol");
        assert_eq!(resolved.source, "registry");
        assert_eq!(resolved.metadata.context_length, Some(1_050_000));
        assert_eq!(resolved.default_reasoning_level.as_deref(), Some("medium"));
    }

    fn provider_with_format(id: &str, format: GatewayApiFormat) -> GatewayProvider {
        GatewayProvider {
            id: id.to_string(),
            name: id.to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: "test-key".to_string(),
            enabled: true,
            auth_style: "auto".to_string(),
            custom_user_agent: String::new(),
            models_url: String::new(),
            cached_models: Vec::new(),
            models_fetched_at: None,
            custom_headers: HashMap::new(),
            impersonate_codex_client: false,
            codex_client_version: String::new(),
            reasoning_request_mode: default_auto_mode(),
            reasoning_history_mode: default_auto_mode(),
            adaptive_thinking_display: default_auto_mode(),
            notes: String::new(),
            record_bodies: false,
            max_concurrent_requests: 0,
            queue_limit: default_queue_limit(),
            queue_timeout_ms: default_queue_timeout_ms(),
            models: vec![GatewayProviderModel {
                alias: "local".to_string(),
                upstream_model: "model-a".to_string(),
                api_format: format,
                enabled: true,
                record_bodies: None,
                metadata: GatewayModelMetadata::default(),
            }],
        }
    }

    #[test]
    fn provider_meta_body_recording_scopes() {
        // 未开启：None
        let p = provider_with_format("a", GatewayApiFormat::OpenaiChat);
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiChat);
        assert!(meta.body_recording_models.is_none());

        // 供应商级开启：全量录制（空列表）
        let mut p = provider_with_format("b", GatewayApiFormat::OpenaiChat);
        p.record_bodies = true;
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiChat);
        assert_eq!(meta.body_recording_models, Some(Vec::new()));

        // 模型级强制开启：仅该模型，其他协议不受影响
        let mut p = provider_with_format("c", GatewayApiFormat::OpenaiChat);
        p.models[0].record_bodies = Some(true);
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiChat);
        assert_eq!(
            meta.body_recording_models,
            Some(vec!["model-a".to_string()])
        );
        // Anthropic 协议下该模型条目不匹配（api_format 不同）→ 关闭
        let meta = provider_meta(&p, GatewayApiFormat::Anthropic);
        assert!(meta.body_recording_models.is_none());

        // 供应商开启 + 模型级显式排除：列表为空 → 仍视为关闭该协议
        let mut p = provider_with_format("d", GatewayApiFormat::OpenaiChat);
        p.record_bodies = true;
        p.models[0].record_bodies = Some(false);
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiChat);
        assert!(meta.body_recording_models.is_none());
    }

    #[test]
    fn default_config_is_local_and_authenticated() {
        let config = GatewayConfig::default();
        assert_eq!(config.listen_address, "127.0.0.1");
        assert_eq!(config.listen_port, 10888);
        assert!(config.require_auth);
        assert!(config.local_api_key.starts_with("local-sk-"));
    }

    fn override_headers(meta: &ProviderMeta) -> HashMap<String, String> {
        meta.local_proxy_request_overrides
            .as_ref()
            .map(|o| o.headers.clone())
            .unwrap_or_default()
    }

    #[test]
    fn impersonate_codex_client_synthesizes_three_headers() {
        let mut p = provider_with_format("codex", GatewayApiFormat::OpenaiResponses);
        p.impersonate_codex_client = true;
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiResponses);
        assert_eq!(
            meta.custom_user_agent.as_deref(),
            Some("codex_cli_rs/0.144.1")
        );
        let headers = override_headers(&meta);
        assert_eq!(headers.get("originator").map(String::as_str), Some("codex_cli_rs"));
        assert_eq!(headers.get("version").map(String::as_str), Some("0.144.1"));
    }

    #[test]
    fn client_fingerprint_applies_codex_identity_to_direct_requests() {
        let mut p = provider_with_format("codex", GatewayApiFormat::OpenaiResponses);
        p.impersonate_codex_client = true;
        let (user_agent, headers) = client_fingerprint(&p);
        assert_eq!(user_agent.as_deref(), Some("codex_cli_rs/0.144.1"));
        assert_eq!(headers.get("originator").map(String::as_str), Some("codex_cli_rs"));
        assert_eq!(headers.get("version").map(String::as_str), Some("0.144.1"));
    }

    #[test]
    fn custom_user_agent_beats_spoofed_ua() {
        let mut p = provider_with_format("codex", GatewayApiFormat::OpenaiResponses);
        p.impersonate_codex_client = true;
        p.custom_user_agent = "MyClient/9.9".to_string();
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiResponses);
        assert_eq!(meta.custom_user_agent.as_deref(), Some("MyClient/9.9"));
        let headers = override_headers(&meta);
        assert_eq!(headers.get("originator").map(String::as_str), Some("codex_cli_rs"));
        assert_eq!(headers.get("version").map(String::as_str), Some("0.144.1"));
    }

    #[test]
    fn custom_version_override_applied() {
        let mut p = provider_with_format("codex", GatewayApiFormat::OpenaiResponses);
        p.impersonate_codex_client = true;
        p.codex_client_version = "0.150.0".to_string();
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiResponses);
        assert_eq!(
            meta.custom_user_agent.as_deref(),
            Some("codex_cli_rs/0.150.0")
        );
        let headers = override_headers(&meta);
        assert_eq!(headers.get("version").map(String::as_str), Some("0.150.0"));
        assert_eq!(headers.get("originator").map(String::as_str), Some("codex_cli_rs"));
    }

    #[test]
    fn toggle_off_injects_nothing() {
        let p = provider_with_format("codex", GatewayApiFormat::OpenaiResponses);
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiResponses);
        assert!(meta.custom_user_agent.is_none());
        assert!(meta.local_proxy_request_overrides.is_none());
        assert_eq!(meta.reasoning_request_mode.as_deref(), Some("auto"));
        assert_eq!(meta.reasoning_history_mode.as_deref(), Some("auto"));
        assert_eq!(meta.adaptive_thinking_display.as_deref(), Some("auto"));
    }

    #[test]
    fn provider_reasoning_modes_are_materialized_into_internal_meta() {
        let mut p = provider_with_format("reasoner", GatewayApiFormat::OpenaiChat);
        p.reasoning_request_mode = "force".to_string();
        p.reasoning_history_mode = "reasoning_content".to_string();
        p.adaptive_thinking_display = "summarized".to_string();

        let meta = provider_meta(&p, GatewayApiFormat::OpenaiChat);

        assert_eq!(meta.reasoning_request_mode.as_deref(), Some("force"));
        assert_eq!(
            meta.reasoning_history_mode.as_deref(),
            Some("reasoning_content")
        );
        assert_eq!(
            meta.adaptive_thinking_display.as_deref(),
            Some("summarized")
        );
    }

    #[test]
    fn spoof_merges_with_custom_headers() {
        let mut p = provider_with_format("codex", GatewayApiFormat::OpenaiResponses);
        p.impersonate_codex_client = true;
        p.custom_headers
            .insert("X-Title".to_string(), "foo".to_string());
        let meta = provider_meta(&p, GatewayApiFormat::OpenaiResponses);
        let headers = override_headers(&meta);
        assert_eq!(headers.get("X-Title").map(String::as_str), Some("foo"));
        assert_eq!(headers.get("originator").map(String::as_str), Some("codex_cli_rs"));
        assert_eq!(headers.get("version").map(String::as_str), Some("0.144.1"));
    }

    #[test]
    fn duplicate_alias_in_one_provider_is_rejected() {
        let mut config = GatewayConfig::default();
        let mut p = provider_with_format("p1", GatewayApiFormat::OpenaiChat);
        p.models.push(GatewayProviderModel {
            alias: "local".to_string(),
            upstream_model: "model-b".to_string(),
            api_format: GatewayApiFormat::OpenaiResponses,
            enabled: true,
            record_bodies: None,
            metadata: GatewayModelMetadata::default(),
        });
        config.providers.push(p);
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn materialized_provider_contains_exact_alias_mapping() {
        let mut config = GatewayConfig::default();
        let mut p = provider_with_format("p1", GatewayApiFormat::OpenaiResponses);
        p.models[0].alias = "best-code".to_string();
        p.models[0].upstream_model = "gpt-test".to_string();
        config.providers.push(p);

        let generated =
            materialize_provider(&config.providers[0], GatewayApiFormat::OpenaiResponses, "codex", 0);
        assert_eq!(
            generated.settings_config["gateway_model_map"]["best-code"],
            Value::String("gpt-test".to_string())
        );
        assert_eq!(
            generated.meta.and_then(|meta| meta.api_format),
            Some("openai_responses".to_string())
        );
    }

    #[test]
    fn one_provider_with_two_formats_materializes_two_internal_providers() {
        let mut config = GatewayConfig::default();
        let mut p = provider_with_format("p1", GatewayApiFormat::OpenaiChat);
        p.models[0].alias = "chat-alias".to_string();
        p.models.push(GatewayProviderModel {
            alias: "resp-alias".to_string(),
            upstream_model: "gpt-5-preview".to_string(),
            api_format: GatewayApiFormat::OpenaiResponses,
            enabled: true,
            record_bodies: None,
            metadata: GatewayModelMetadata::default(),
        });
        config.providers.push(p);
        let combos = iter_materialized_combos(&config);
        assert_eq!(combos.len(), 2);
        let mut formats: Vec<GatewayApiFormat> =
            combos.iter().map(|(_, _, f)| *f).collect();
        formats.sort_by_key(|f| f.as_wire_name());
        assert_eq!(
            formats,
            vec![GatewayApiFormat::OpenaiChat, GatewayApiFormat::OpenaiResponses]
        );
    }

    #[test]
    fn legacy_config_migrates_apiformat_and_routes_into_models() {
        let raw = json!({
            "listenAddress": "127.0.0.1",
            "listenPort": 10888,
            "requireAuth": true,
            "localApiKey": "local-sk-abc",
            "autoStart": false,
            "enableLogging": true,
            "providers": [{
                "id": "p1",
                "name": "P1",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "sk-x",
                "apiFormat": "openai_chat",
                "enabled": true,
                "authStyle": "auto",
                "customUserAgent": "",
                "modelsUrl": "",
                "cachedModels": [],
                "customHeaders": {},
                "impersonateCodexClient": false,
                "codexClientVersion": "",
                "notes": ""
            }],
            "routes": [{
                "alias": "local",
                "enabled": true,
                "targets": [{"providerId": "p1", "upstreamModel": "gpt-5", "enabled": true}]
            }]
        })
        .to_string();
        let config = parse_config_with_migration(&raw).expect("migrates");
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].models.len(), 1);
        assert_eq!(config.providers[0].models[0].alias, "local");
        assert_eq!(config.providers[0].models[0].upstream_model, "gpt-5");
        assert_eq!(
            config.providers[0].models[0].api_format,
            GatewayApiFormat::OpenaiChat
        );
        assert_eq!(config.providers[0].reasoning_request_mode, "auto");
        assert_eq!(config.providers[0].reasoning_history_mode, "auto");
        assert_eq!(config.providers[0].adaptive_thinking_display, "auto");
    }

    #[test]
    fn current_config_without_reasoning_fields_uses_backward_compatible_defaults() {
        let mut provider = serde_json::to_value(provider_with_format(
            "p1",
            GatewayApiFormat::OpenaiChat,
        ))
        .expect("serialize provider");
        let object = provider.as_object_mut().expect("provider object");
        object.remove("reasoningRequestMode");
        object.remove("reasoningHistoryMode");
        object.remove("adaptiveThinkingDisplay");

        let mut config = serde_json::to_value(GatewayConfig::default()).expect("serialize config");
        config["providers"] = json!([provider]);

        let parsed = parse_config_with_migration(&config.to_string()).expect("parse config");
        assert_eq!(parsed.providers[0].reasoning_request_mode, "auto");
        assert_eq!(parsed.providers[0].reasoning_history_mode, "auto");
        assert_eq!(parsed.providers[0].adaptive_thinking_display, "auto");
    }

    fn user_message(content: &str) -> GatewayModelTestMessage {
        GatewayModelTestMessage {
            role: GatewayModelTestRole::User,
            content: content.to_string(),
        }
    }

    fn assistant_message(content: &str) -> GatewayModelTestMessage {
        GatewayModelTestMessage {
            role: GatewayModelTestRole::Assistant,
            content: content.to_string(),
        }
    }

    fn multi_turn_messages() -> Vec<GatewayModelTestMessage> {
        vec![
            user_message("你好"),
            assistant_message("你好，有什么可以帮你？"),
            user_message("再问一个问题"),
        ]
    }

    fn test_messages_value_of<'a>(payload: &'a Value, field: &str) -> &'a Vec<Value> {
        payload
            .get(field)
            .and_then(Value::as_array)
            .expect("messages array present")
    }

    #[test]
    fn chat_payload_preserves_multi_turn_and_max_tokens() {
        let messages = multi_turn_messages();
        let payload = build_test_payload(
            GatewayApiFormat::OpenaiChat,
            "model-a",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            None,
        )
        .expect("chat payload");
        assert_eq!(payload["model"], "model-a");
        assert_eq!(payload["max_tokens"], 4096);
        assert_eq!(payload["stream"], false);
        let messages = test_messages_value_of(&payload, "messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "你好");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "你好，有什么可以帮你？");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "再问一个问题");
    }

    #[test]
    fn responses_payload_preserves_multi_turn_and_max_output_tokens() {
        let messages = multi_turn_messages();
        let payload = build_test_payload(
            GatewayApiFormat::OpenaiResponses,
            "model-a",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            None,
        )
        .expect("responses payload");
        assert_eq!(payload["model"], "model-a");
        assert_eq!(payload["max_output_tokens"], 4096);
        assert_eq!(payload["stream"], false);
        let input = payload
            .get("input")
            .and_then(Value::as_array)
            .expect("responses input array");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["role"], "user");
        // 经 chat_request_to_responses 转换后，Responses 的纯文本消息 content 仍是字符串，
        // 而非 [{type:"input_text", text:...}] 数组——断言必须与该结构对齐。
        assert_eq!(input[0]["content"], "你好", "responses input text should be preserved");
        assert_eq!(input[1]["content"], "你好，有什么可以帮你？");
        assert_eq!(input[2]["content"], "再问一个问题");
    }

    #[test]
    fn anthropic_payload_preserves_multi_turn_and_max_tokens() {
        let messages = multi_turn_messages();
        let payload = build_test_payload(
            GatewayApiFormat::Anthropic,
            "model-a",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            None,
        )
        .expect("anthropic payload");
        assert_eq!(payload["model"], "model-a");
        assert_eq!(payload["max_tokens"], 4096);
        assert_eq!(payload["stream"], false);
        let messages = test_messages_value_of(&payload, "messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
    }

    fn test_request_with_messages(messages: Vec<GatewayModelTestMessage>) -> GatewayModelTestRequest {
        GatewayModelTestRequest {
            provider: provider_with_format("p1", GatewayApiFormat::OpenaiChat),
            upstream_model: "model-a".to_string(),
            alias: "local".to_string(),
            api_format: GatewayApiFormat::OpenaiChat,
            messages,
            max_output_tokens: 4096,
            via_gateway: false,
            proxy_mode: GatewayTestProxyMode::Bypass,
            custom_proxy_url: String::new(),
            thinking_level: GatewayTestThinkingLevel::Disabled,
            system_prompt: None,
            stream: false,
        }
    }

    #[test]
    fn validate_accepts_default_output_limit() {
        let request = test_request_with_messages(vec![user_message("hi")]);
        validate_test_request(&request).expect("4096 is valid");
    }

    #[test]
    fn validate_rejects_zero_and_oversized_output_limit() {
        let mut request = test_request_with_messages(vec![user_message("hi")]);
        request.max_output_tokens = 0;
        assert!(validate_test_request(&request).is_err());

        request.max_output_tokens = 131_073;
        assert!(validate_test_request(&request).is_err());

        request.max_output_tokens = 131_072;
        validate_test_request(&request).expect("131072 is the upper bound");
    }

    #[test]
    fn validate_rejects_empty_or_blank_messages() {
        let mut request = test_request_with_messages(vec![user_message("   ")]);
        assert!(validate_test_request(&request).is_err());

        request.messages = vec![];
        assert!(validate_test_request(&request).is_err());
    }

    #[test]
    fn validate_requires_user_first_and_last_turn() {
        let request = test_request_with_messages(vec![
            assistant_message("先说话"),
            user_message("提问"),
        ]);
        assert!(validate_test_request(&request).is_err());

        let request = test_request_with_messages(vec![
            user_message("提问"),
            assistant_message("回答"),
        ]);
        assert!(validate_test_request(&request).is_err());
    }

    #[test]
    fn validate_enforces_message_size_limits() {
        let big = "a".repeat(65 * 1024);
        let request = test_request_with_messages(vec![user_message(&big)]);
        assert!(validate_test_request(&request).is_err());
    }

    #[test]
    fn parse_chat_response_reports_length_truncation_and_usage() {
        let body = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "半截回复"},
                "finish_reason": "length"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4096,
                "total_tokens": 4106,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 8}
            }
        });
        let parsed = parse_test_response(GatewayApiFormat::OpenaiChat, &body);
        assert_eq!(parsed.reply_text, "半截回复");
        assert_eq!(parsed.finish_reason.as_deref(), Some("length"));
        assert!(parsed.length_truncated);
        let usage = parsed.usage.expect("usage present");
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(4096));
        assert_eq!(usage.total_tokens, Some(4106));
        assert_eq!(usage.cache_read_input_tokens, Some(4));
        assert_eq!(usage.reasoning_tokens, Some(8));
    }

    #[test]
    fn parse_responses_response_reports_incomplete_and_usage() {
        let body = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "content": [{"type": "output_text", "text": "回复片段"}]
            }],
            "usage": {"input_tokens": 7, "output_tokens": 4096, "total_tokens": 4103}
        });
        let parsed = parse_test_response(GatewayApiFormat::OpenaiResponses, &body);
        assert_eq!(parsed.reply_text, "回复片段");
        assert_eq!(parsed.finish_reason.as_deref(), Some("max_output_tokens"));
        assert!(parsed.length_truncated);
        assert_eq!(parsed.usage.unwrap().output_tokens, Some(4096));
    }

    #[test]
    fn parse_anthropic_response_reports_stop_reason_and_usage() {
        let body = json!({
            "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "截断的回复"}],
            "usage": {"input_tokens": 5, "output_tokens": 4096, "cache_read_input_tokens": 2}
        });
        let parsed = parse_test_response(GatewayApiFormat::Anthropic, &body);
        assert_eq!(parsed.reply_text, "截断的回复");
        assert_eq!(parsed.finish_reason.as_deref(), Some("max_tokens"));
        assert!(parsed.length_truncated);
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(4096));
        assert_eq!(usage.cache_read_input_tokens, Some(2));
        assert_eq!(usage.total_tokens, Some(4101));
    }

    #[test]
    fn parse_chat_response_without_truncation() {
        let body = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "完整回复"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 5}
        });
        let parsed = parse_test_response(GatewayApiFormat::OpenaiChat, &body);
        assert_eq!(parsed.finish_reason.as_deref(), Some("stop"));
        assert!(!parsed.length_truncated);
        assert_eq!(parsed.usage.unwrap().total_tokens, Some(8));
    }

    #[test]
    fn parse_chat_response_extracts_reasoning_content_separately() {
        // 复现 deepseek 等推理模型：content 为 null、思考占满预算、finish_reason=length。
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "让我想想……"
                },
                "finish_reason": "length"
            }],
            "usage": {
                "prompt_tokens": 270,
                "completion_tokens": 16384,
                "total_tokens": 16654,
                "completion_tokens_details": {"reasoning_tokens": 16384}
            }
        });
        let parsed = parse_test_response(GatewayApiFormat::OpenaiChat, &body);
        assert_eq!(parsed.reply_text, "");
        assert_eq!(parsed.reasoning_text, "让我想想……");
        assert!(parsed.length_truncated);
        let usage = parsed.usage.expect("usage present");
        assert_eq!(usage.reasoning_tokens, Some(16384));
        assert_eq!(usage.output_tokens, Some(16384));
    }

    #[test]
    fn parse_responses_response_extracts_reasoning_summary() {
        let body = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "推理摘要"}]
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "回复片段"}]
                }
            ]
        });
        let parsed = parse_test_response(GatewayApiFormat::OpenaiResponses, &body);
        assert_eq!(parsed.reply_text, "回复片段");
        assert_eq!(parsed.reasoning_text, "推理摘要");
    }

    #[test]
    fn parse_anthropic_response_extracts_thinking_block() {
        let body = json!({
            "stop_reason": "max_tokens",
            "content": [
                {"type": "thinking", "thinking": "推理内容"},
                {"type": "text", "text": "截断的回复"}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 4096}
        });
        let parsed = parse_test_response(GatewayApiFormat::Anthropic, &body);
        assert_eq!(parsed.reply_text, "截断的回复");
        assert_eq!(parsed.reasoning_text, "推理内容");
    }

    // ------------------------------------------------------------------
    // 流式测试：请求体开关与 SSE 增量解析
    // ------------------------------------------------------------------

    /// 把一段 JSON 包成一个 SSE 事件块（`data: {...}`）。
    fn sse_data(payload: &str) -> String {
        format!("data: {payload}")
    }

    fn sse_json(payload: Value) -> String {
        sse_data(&payload.to_string())
    }

    #[test]
    fn apply_test_stream_only_flips_the_stream_switch() {
        let base = build_test_payload(
            GatewayApiFormat::OpenaiChat,
            "model-a",
            &[user_message("hi")],
            4096,
            GatewayTestThinkingLevel::Disabled,
            None,
        )
        .expect("payload");

        let mut off = base.clone();
        apply_test_stream(&mut off, GatewayApiFormat::OpenaiChat, false);
        assert_eq!(off, base, "非流式不应改动 payload");

        let mut on = base.clone();
        apply_test_stream(&mut on, GatewayApiFormat::OpenaiChat, true);
        assert_eq!(on["stream"], true);
        assert_eq!(on["stream_options"]["include_usage"], true);
        // 除了流式开关，其余字段必须与非流式逐字一致。
        assert_eq!(on["model"], base["model"]);
        assert_eq!(on["messages"], base["messages"]);
        assert_eq!(on["max_tokens"], base["max_tokens"]);
    }

    #[test]
    fn apply_test_stream_skips_chat_usage_flag_for_other_formats() {
        // Anthropic / Responses 的 usage 本来就在流里，不需要 Chat 的
        // stream_options；多写一个未知字段可能被上游直接拒绝。
        for format in [GatewayApiFormat::Anthropic, GatewayApiFormat::OpenaiResponses] {
            let mut payload = json!({ "model": "m", "stream": false });
            apply_test_stream(&mut payload, format, true);
            assert_eq!(payload["stream"], true);
            assert!(payload.get("stream_options").is_none());
        }
    }

    #[test]
    fn stream_chat_deltas_accumulate_and_keep_usage_after_finish_reason() {
        let mut parsed = ParsedTestResponse::default();

        let thinking = apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            &sse_json(json!({
                "choices": [{"delta": {"reasoning_content": "想"}}]
            })),
            &mut parsed,
        );
        assert_eq!(thinking.reasoning, "想");
        assert_eq!(thinking.text, "");
        assert!(!thinking.is_terminal);

        let text = apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            &sse_json(json!({ "choices": [{"delta": {"content": "你"}}] })),
            &mut parsed,
        );
        assert_eq!(text.text, "你");

        // finish_reason 之后还有一条带 usage 的 chunk，所以这里不能算收尾。
        let finish = apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            &sse_json(json!({
                "choices": [{"delta": {}, "finish_reason": "length"}]
            })),
            &mut parsed,
        );
        assert!(
            !finish.is_terminal,
            "finish_reason 之后仍有 usage chunk，不能提前收工"
        );
        assert_eq!(parsed.finish_reason.as_deref(), Some("length"));
        assert!(parsed.length_truncated);

        apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            &sse_json(json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 270,
                    "completion_tokens": 16384,
                    "completion_tokens_details": {"reasoning_tokens": 16384}
                }
            })),
            &mut parsed,
        );

        let done = apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            &sse_data("[DONE]"),
            &mut parsed,
        );
        assert!(done.is_terminal);

        assert_eq!(parsed.reply_text, "你");
        assert_eq!(parsed.reasoning_text, "想");
        let usage = parsed.usage.expect("usage 必须从流里保留下来");
        assert_eq!(usage.total_tokens, Some(16654));
        assert_eq!(usage.reasoning_tokens, Some(16384));
    }

    #[test]
    fn merge_test_usage_lets_later_reports_overwrite_earlier_ones() {
        // Anthropic 风格：message_start 先报 output_tokens = 0，message_delta 才给
        // 最终值。这两个数是累计值而非分片量，所以后来者必须覆盖前者——若按“只填补
        // 缺失字段”合并，那个早期 0 会永久生效，用户就会看到“输出 0”。
        let mut slot = None;
        merge_test_usage(
            &mut slot,
            GatewayModelTestUsage {
                input_tokens: Some(35),
                output_tokens: Some(0),
                ..Default::default()
            },
        );
        merge_test_usage(
            &mut slot,
            GatewayModelTestUsage {
                input_tokens: Some(35),
                output_tokens: Some(15),
                reasoning_tokens: Some(15),
                ..Default::default()
            },
        );

        let usage = slot.expect("usage 必须被合并出来");
        assert_eq!(usage.output_tokens, Some(15));
        assert_eq!(usage.reasoning_tokens, Some(15));
        // 上游没给 total_tokens，按 input + output 补算。
        assert_eq!(usage.total_tokens, Some(50));
    }

    #[test]
    fn merge_test_usage_keeps_fields_a_later_report_omits() {
        let mut slot = None;
        merge_test_usage(
            &mut slot,
            GatewayModelTestUsage {
                input_tokens: Some(314),
                output_tokens: Some(2861),
                total_tokens: Some(3175),
                cache_read_input_tokens: Some(0),
                reasoning_tokens: Some(0),
                ..Default::default()
            },
        );
        // 后续事件只带 output_tokens；它没提到的字段不能被清空。
        merge_test_usage(
            &mut slot,
            GatewayModelTestUsage {
                output_tokens: Some(2900),
                ..Default::default()
            },
        );

        let usage = slot.expect("usage 必须被合并出来");
        assert_eq!(usage.output_tokens, Some(2900));
        assert_eq!(usage.input_tokens, Some(314));
        assert_eq!(usage.reasoning_tokens, Some(0));
        // 上游明确给了 total 就不再改算，避免和上游的口径打架。
        assert_eq!(usage.total_tokens, Some(3175));
    }

    #[test]
    fn stream_anthropic_merges_usage_split_across_events() {
        let mut parsed = ParsedTestResponse::default();

        // input_tokens 只在 message_start 出现。
        apply_stream_block(
            GatewayApiFormat::Anthropic,
            &sse_json(json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 12, "cache_read_input_tokens": 3}}
            })),
            &mut parsed,
        );
        apply_stream_block(
            GatewayApiFormat::Anthropic,
            &sse_json(json!({
                "type": "content_block_delta",
                "delta": {"type": "thinking_delta", "thinking": "思考"}
            })),
            &mut parsed,
        );
        apply_stream_block(
            GatewayApiFormat::Anthropic,
            &sse_json(json!({
                "type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "正文"}
            })),
            &mut parsed,
        );
        // output_tokens 与 stop_reason 在 message_delta 才到，仍不是最后一个事件。
        let tail = apply_stream_block(
            GatewayApiFormat::Anthropic,
            &sse_json(json!({
                "type": "message_delta",
                "delta": {"stop_reason": "max_tokens"},
                "usage": {"output_tokens": 4096}
            })),
            &mut parsed,
        );
        assert!(!tail.is_terminal, "message_delta 之后还有 message_stop");

        let stop = apply_stream_block(
            GatewayApiFormat::Anthropic,
            &sse_json(json!({"type": "message_stop"})),
            &mut parsed,
        );
        assert!(stop.is_terminal);

        assert_eq!(parsed.reasoning_text, "思考");
        assert_eq!(parsed.reply_text, "正文");
        assert_eq!(parsed.finish_reason.as_deref(), Some("max_tokens"));
        assert!(parsed.length_truncated);
        let usage = parsed.usage.expect("两段 usage 必须合并");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(4096));
        assert_eq!(usage.cache_read_input_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(4108));
    }

    #[test]
    fn stream_responses_deltas_and_terminal_event() {
        let mut parsed = ParsedTestResponse::default();

        apply_stream_block(
            GatewayApiFormat::OpenaiResponses,
            &sse_json(json!({
                "type": "response.reasoning_summary_text.delta",
                "delta": "推理"
            })),
            &mut parsed,
        );
        apply_stream_block(
            GatewayApiFormat::OpenaiResponses,
            &sse_json(json!({
                "type": "response.output_text.delta",
                "delta": "正文"
            })),
            &mut parsed,
        );
        let tail = apply_stream_block(
            GatewayApiFormat::OpenaiResponses,
            &sse_json(json!({
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "output": [],
                    "usage": {
                        "input_tokens": 7,
                        "output_tokens": 100,
                        "output_tokens_details": {"reasoning_tokens": 40}
                    }
                }
            })),
            &mut parsed,
        );
        assert!(tail.is_terminal);

        assert_eq!(parsed.reasoning_text, "推理");
        assert_eq!(parsed.reply_text, "正文");
        assert_eq!(parsed.finish_reason.as_deref(), Some("max_output_tokens"));
        assert!(parsed.length_truncated);
        let usage = parsed.usage.expect("终态事件里的 usage");
        assert_eq!(usage.output_tokens, Some(100));
        assert_eq!(usage.reasoning_tokens, Some(40));
    }

    #[test]
    fn stream_block_surfaces_in_band_errors() {
        let mut parsed = ParsedTestResponse::default();
        let anthropic = apply_stream_block(
            GatewayApiFormat::Anthropic,
            &sse_json(json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "上游过载"}
            })),
            &mut parsed,
        );
        assert_eq!(anthropic.error.as_deref(), Some("上游过载"));

        let mut parsed = ParsedTestResponse::default();
        let chat = apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            &sse_json(json!({"error": {"message": "rate limited"}})),
            &mut parsed,
        );
        assert_eq!(chat.error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn stream_block_ignores_non_data_lines_and_blank_blocks() {
        let mut parsed = ParsedTestResponse::default();
        // 只有 event: 行、没有 data: 行；以及 data: 后的空行都不应产生任何增量。
        let effect = apply_stream_block(
            GatewayApiFormat::OpenaiChat,
            "event: ping\n\ndata: \n",
            &mut parsed,
        );
        assert!(effect.is_empty());
        assert!(parsed.reply_text.is_empty());
        assert!(!effect.is_terminal);
    }

    #[test]
    fn pretty_raw_body_formats_large_unicode_json() {
        let value = json!({
            "content": "你好，世界！".repeat(200),
            "nested": {"ok": true}
        });
        let raw = serde_json::to_string(&value).expect("compact json");
        assert!(raw.len() > 2 * 1024);
        let pretty = pretty_raw_body(&raw);
        assert!(pretty.contains('\n'), "pretty-printed body should be multi-line");
        let reparsed: Value = serde_json::from_str(&pretty).expect("pretty body is valid json");
        assert_eq!(reparsed, value);
    }

    #[test]
    fn pretty_raw_body_preserves_non_json_text() {
        let raw = "not a json body at all";
        assert_eq!(pretty_raw_body(raw), raw);
    }

    #[test]
    fn extract_structured_error_picks_message_fields() {
        let body = json!({"error": {"message": "boom", "type": "x"}});
        assert_eq!(extract_structured_error(&body).as_deref(), Some("boom"));

        let body = json!({"detail": "bad input"});
        assert_eq!(extract_structured_error(&body).as_deref(), Some("bad input"));

        let body = json!({"message": "oops"});
        assert_eq!(extract_structured_error(&body).as_deref(), Some("oops"));

        assert!(extract_structured_error(&json!({})).is_none());
    }

    #[test]
    fn reasoning_effort_mapping() {
        assert_eq!(GatewayTestThinkingLevel::Disabled.reasoning_effort(), None);
        assert_eq!(GatewayTestThinkingLevel::Low.reasoning_effort(), Some("low"));
        assert_eq!(
            GatewayTestThinkingLevel::Medium.reasoning_effort(),
            Some("medium")
        );
        assert_eq!(GatewayTestThinkingLevel::High.reasoning_effort(), Some("high"));
    }

    #[test]
    fn anthropic_budget_maps_to_ratio_of_max_tokens() {
        // 低 25% / 中 50% / 高 75%，且至少为 1。
        assert_eq!(
            GatewayTestThinkingLevel::Low.anthropic_budget(4096),
            Some(1024)
        );
        assert_eq!(
            GatewayTestThinkingLevel::Medium.anthropic_budget(4096),
            Some(2048)
        );
        assert_eq!(
            GatewayTestThinkingLevel::High.anthropic_budget(4096),
            Some(3072)
        );
        assert_eq!(
            GatewayTestThinkingLevel::Disabled.anthropic_budget(4096),
            None
        );
        // 小 max_tokens 也要兜底为 1。
        assert_eq!(GatewayTestThinkingLevel::Low.anthropic_budget(1), Some(1));
    }

    #[test]
    fn chat_payload_injects_reasoning_effort() {
        let messages = vec![user_message("hi")];
        let payload = build_test_payload(
            GatewayApiFormat::OpenaiChat,
            "model-a",
            &messages,
            4096,
            GatewayTestThinkingLevel::High,
            None,
        )
        .expect("chat payload builds");
        assert_eq!(payload["reasoning_effort"], json!("high"));

        let disabled = build_test_payload(
            GatewayApiFormat::OpenaiChat,
            "model-a",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            None,
        )
        .expect("chat payload builds");
        assert!(
            disabled.get("reasoning_effort").is_none(),
            "disabled must not emit reasoning_effort"
        );
    }

    #[test]
    fn anthropic_payload_injects_thinking_block() {
        let messages = vec![user_message("hi")];
        let payload = build_test_payload(
            GatewayApiFormat::Anthropic,
            "claude-model",
            &messages,
            4096,
            GatewayTestThinkingLevel::Medium,
            None,
        )
        .expect("anthropic payload builds");
        assert_eq!(payload["thinking"]["type"], json!("enabled"));
        assert_eq!(payload["thinking"]["budget_tokens"], json!(2048));

        let disabled = build_test_payload(
            GatewayApiFormat::Anthropic,
            "claude-model",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            None,
        )
        .expect("anthropic payload builds");
        assert_eq!(disabled["thinking"]["type"], json!("disabled"));
        assert!(disabled["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn system_prompt_injected_per_protocol() {
        let messages = vec![user_message("hi")];
        let system = Some("you are concise");

        // OpenAI Chat：system 作为 messages 首条。
        let chat = build_test_payload(
            GatewayApiFormat::OpenaiChat,
            "model-a",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            system,
        )
        .expect("chat payload builds");
        assert_eq!(chat["messages"][0]["role"], json!("system"));
        assert_eq!(chat["messages"][0]["content"], json!("you are concise"));
        assert_eq!(chat["messages"][1]["role"], json!("user"));

        // Anthropic：system 走顶层字段，不在 messages 里。
        let anthropic = build_test_payload(
            GatewayApiFormat::Anthropic,
            "claude-model",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            system,
        )
        .expect("anthropic payload builds");
        assert_eq!(anthropic["system"], json!("you are concise"));
        assert_eq!(anthropic["messages"][0]["role"], json!("user"));

        // 空/空白 system 不注入。
        let blank = build_test_payload(
            GatewayApiFormat::Anthropic,
            "claude-model",
            &messages,
            4096,
            GatewayTestThinkingLevel::Disabled,
            Some("   "),
        )
        .expect("anthropic payload builds");
        assert!(blank.get("system").is_none());
    }
}
