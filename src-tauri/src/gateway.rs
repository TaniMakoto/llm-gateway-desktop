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

const CONFIG_KEY: &str = "unified_gateway_config_v1";
const GENERATED_CATEGORY: &str = "unified_gateway";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GatewayApiFormat {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayCachedModel {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayProviderModel {
    pub alias: String,
    pub upstream_model: String,
    pub api_format: GatewayApiFormat,
    #[serde(default = "default_true")]
    pub enabled: bool,
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
            providers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySnapshot {
    pub config: GatewayConfig,
    pub status: ProxyStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayModelFetchResult {
    pub models: Vec<GatewayCachedModel>,
    pub fetched_at: String,
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
        providers,
    })
}

fn normalize_config(mut config: GatewayConfig) -> GatewayConfig {
    config.listen_address = config.listen_address.trim().to_string();
    config.local_api_key = config.local_api_key.trim().to_string();
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
        }
    }
    config
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
) -> Result<Option<Vec<Provider>>, AppError> {
    let config = load_config(db)?;
    let alias = alias.trim();
    if alias.is_empty() {
        return Ok(None);
    }

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
    Ok(Some(result))
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
    let mut aliases: BTreeMap<String, ()> = BTreeMap::new();
    for provider in &config.providers {
        if !provider.enabled {
            continue;
        }
        for model in &provider.models {
            if !model.enabled {
                continue;
            }
            aliases.insert(model.alias.clone(), ());
        }
    }
    let data: Vec<Value> = aliases
        .into_keys()
        .map(|alias| {
            json!({
                "id": alias,
                "object": "model",
                "created": created,
                "owned_by": "local-gateway"
            })
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
    Ok(GatewaySnapshot { config, status })
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

// ============================================================================
// 单模型测试对话
// ============================================================================

const TEST_MAX_OUTPUT_TOKENS_MIN: u32 = 1;
const TEST_MAX_OUTPUT_TOKENS_MAX: u32 = 16_384;
const TEST_MESSAGE_COUNT_MAX: usize = 100;
const TEST_MESSAGE_BYTES_MAX: usize = 64 * 1024;
const TEST_MESSAGES_TOTAL_BYTES_MAX: usize = 1024 * 1024;
const TEST_RESPONSE_BYTES_MAX: usize = 16 * 1024 * 1024;
const TEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
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
    pub raw_body: String,
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
                length_truncated: is_length_truncated(finish_reason.as_deref()),
                finish_reason,
                usage,
            }
        }
    }
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
) -> Result<Value, String> {
    match format {
        GatewayApiFormat::OpenaiChat => Ok(json!({
            "model": model,
            "messages": chat_messages_value(messages),
            "max_tokens": max_output_tokens,
            "stream": false,
        })),
        GatewayApiFormat::OpenaiResponses => {
            let chat_shaped = json!({
                "model": model,
                "messages": chat_messages_value(messages),
                "max_tokens": max_output_tokens,
                "stream": false,
            });
            crate::gateway_chat::chat_request_to_responses(chat_shaped).map_err(|error| error.to_string())
        }
        GatewayApiFormat::Anthropic => Ok(json!({
            "model": model,
            "messages": chat_messages_value(messages),
            "max_tokens": max_output_tokens,
            "stream": false,
        })),
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

async fn run_direct_test(
    request: &GatewayModelTestRequest,
    client: &reqwest::Client,
) -> Result<(u16, String), String> {
    let url = join_url(&request.provider.base_url, endpoint_path(request.api_format));
    let payload = build_test_payload(
        request.api_format,
        &request.upstream_model,
        &request.messages,
        request.max_output_tokens,
    )?;

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
    let status = response.status().as_u16();
    let text = read_response_text(response).await?;
    Ok((status, text))
}

async fn run_gateway_test(
    state: &AppState,
    request: &GatewayModelTestRequest,
    client: &reqwest::Client,
) -> Result<(u16, String), String> {
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
    let payload = build_test_payload(
        request.api_format,
        alias,
        &request.messages,
        request.max_output_tokens,
    )?;

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
    let status = response.status().as_u16();
    let text = read_response_text(response).await?;
    Ok((status, text))
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
        raw_body: String::new(),
        error,
        path_used: path_used.to_string(),
        proxy_effective,
        finish_reason: None,
        length_truncated: false,
        usage: None,
    }
}

#[tauri::command]
pub async fn test_gateway_model(
    state: tauri::State<'_, AppState>,
    request: GatewayModelTestRequest,
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

    let result = if request.via_gateway {
        run_gateway_test(&state, &request, &client).await
    } else {
        run_direct_test(&request, &client).await
    };

    let latency_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok((status, text)) => {
            let ok = (200..300).contains(&status);
            let raw_body = pretty_raw_body(&text);
            let body: Option<Value> = serde_json::from_str(&text).ok();
            let parsed = body
                .as_ref()
                .map(|value| parse_test_response(request.api_format, value))
                .unwrap_or_default();
            let error = if ok {
                None
            } else {
                body.as_ref()
                    .and_then(extract_structured_error)
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
                raw_body,
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
            models: vec![GatewayProviderModel {
                alias: "local".to_string(),
                upstream_model: "model-a".to_string(),
                api_format: format,
                enabled: true,
            }],
        }
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

    fn test_messages_value_of(payload: &Value, field: &str) -> &Vec<Value> {
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
        assert_eq!(
            input[0]["content"][0]["text"],
            "你好",
            "responses input text should be preserved"
        );
    }

    #[test]
    fn anthropic_payload_preserves_multi_turn_and_max_tokens() {
        let messages = multi_turn_messages();
        let payload = build_test_payload(
            GatewayApiFormat::Anthropic,
            "model-a",
            &messages,
            4096,
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

        request.max_output_tokens = 16_385;
        assert!(validate_test_request(&request).is_err());

        request.max_output_tokens = 16_384;
        validate_test_request(&request).expect("16384 is the upper bound");
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
}
