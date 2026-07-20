//! Unified local LLM gateway configuration and routing.
//!
//! The gateway deliberately reuses LLM Gateway Desktop's mature proxy/transform stack.
//! A single generic upstream is materialized as a generated `claude` provider
//! and a generated `codex` provider, while model aliases select an ordered
//! provider chain per request.

use crate::database::Database;
use crate::error::AppError;
use crate::provider::{LocalProxyRequestOverrides, Provider, ProviderMeta};
use crate::proxy::types::{ProxyServerInfo, ProxyStatus};
use crate::store::AppState;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;
use tauri::menu::{Menu, MenuBuilder, MenuItem};
use tauri::Manager;

const CONFIG_KEY: &str = "unified_gateway_config_v1";
const GENERATED_CATEGORY: &str = "unified_gateway";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub api_format: GatewayApiFormat,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
    #[serde(default)]
    pub custom_headers: HashMap<String, String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRouteTarget {
    pub provider_id: String,
    pub upstream_model: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRoute {
    pub alias: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub targets: Vec<GatewayRouteTarget>,
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
    #[serde(default)]
    pub routes: Vec<GatewayRoute>,
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
            routes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySnapshot {
    pub config: GatewayConfig,
    pub status: ProxyStatus,
}

fn default_true() -> bool {
    true
}

fn default_auth_style() -> String {
    "auto".to_string()
}

pub fn generate_local_key() -> String {
    format!("local-sk-{}", Uuid::new_v4().simple())
}

pub fn load_config(db: &Database) -> Result<GatewayConfig, AppError> {
    match db.get_setting(CONFIG_KEY)? {
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|e| AppError::Config(format!("统一网关配置解析失败: {e}"))),
        None => Ok(GatewayConfig::default()),
    }
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
        provider.notes = provider.notes.trim().to_string();
        provider.custom_headers = provider
            .custom_headers
            .drain()
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .filter(|(key, _)| !key.is_empty())
            .collect();
    }
    for route in &mut config.routes {
        route.alias = route.alias.trim().to_string();
        for target in &mut route.targets {
            target.provider_id = target.provider_id.trim().to_string();
            target.upstream_model = target.upstream_model.trim().to_string();
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
        if provider.id.trim().is_empty() || provider.name.trim().is_empty() {
            return Err("供应商 ID 和名称不能为空".to_string());
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
        if !matches!(provider.auth_style.as_str(), "auto" | "bearer" | "x-api-key") {
            return Err(format!("供应商 {} 的鉴权方式无效", provider.name));
        }
    }

    let mut aliases = HashSet::new();
    for route in &config.routes {
        let alias = route.alias.trim();
        if alias.is_empty() {
            return Err("模型别名不能为空".to_string());
        }
        if !aliases.insert(alias.to_string()) {
            return Err(format!("模型别名重复: {alias}"));
        }
        let mut route_provider_ids = HashSet::new();
        if route.enabled
            && !route.targets.iter().any(|target| {
                target.enabled
                    && config
                        .providers
                        .iter()
                        .any(|provider| provider.id == target.provider_id && provider.enabled)
            })
        {
            return Err(format!(
                "模型别名 {alias} 至少需要一个启用且可用的供应商目标"
            ));
        }
        for target in &route.targets {
            if !provider_ids.contains(&target.provider_id) {
                return Err(format!(
                    "模型别名 {alias} 引用了不存在的供应商 {}",
                    target.provider_id
                ));
            }
            if !route_provider_ids.insert(target.provider_id.as_str()) {
                return Err(format!(
                    "模型别名 {alias} 不能重复引用同一个供应商 {}",
                    target.provider_id
                ));
            }
            if target.upstream_model.trim().is_empty() {
                return Err(format!("模型别名 {alias} 存在空的上游模型名"));
            }
        }
    }

    Ok(())
}

fn provider_model_map(config: &GatewayConfig, provider_id: &str) -> Map<String, Value> {
    let mut map = Map::new();
    for route in config.routes.iter().filter(|route| route.enabled) {
        if let Some(target) = route
            .targets
            .iter()
            .find(|target| target.enabled && target.provider_id == provider_id)
        {
            map.insert(
                route.alias.trim().to_string(),
                Value::String(target.upstream_model.trim().to_string()),
            );
        }
    }
    map
}

fn provider_meta(provider: &GatewayProvider) -> ProviderMeta {
    let mut meta = ProviderMeta::default();
    meta.api_format = Some(provider.api_format.as_wire_name().to_string());
    meta.api_key_field = match provider.auth_style.as_str() {
        "x-api-key" => Some("ANTHROPIC_API_KEY".to_string()),
        "bearer" => Some("ANTHROPIC_AUTH_TOKEN".to_string()),
        _ if provider.api_format == GatewayApiFormat::Anthropic => {
            Some("ANTHROPIC_API_KEY".to_string())
        }
        _ => Some("ANTHROPIC_AUTH_TOKEN".to_string()),
    };
    if !provider.custom_headers.is_empty() {
        meta.local_proxy_request_overrides = Some(LocalProxyRequestOverrides {
            headers: provider.custom_headers.clone(),
            body: None,
        });
    }
    meta
}

fn materialize_provider(
    config: &GatewayConfig,
    provider: &GatewayProvider,
    app_type: &str,
    sort_index: usize,
) -> Provider {
    let exact_model_map = provider_model_map(config, &provider.id);
    let auth_is_x_api_key = matches!(provider.auth_style.as_str(), "x-api-key")
        || (provider.auth_style == "auto" && provider.api_format == GatewayApiFormat::Anthropic);

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
            "apiFormat": provider.api_format.as_wire_name(),
            "gateway_model_map": exact_model_map,
        })
    } else {
        json!({
            "base_url": provider.base_url.trim_end_matches('/'),
            "apiKey": provider.api_key.clone(),
            "apiFormat": provider.api_format.as_wire_name(),
            "gateway_model_map": exact_model_map,
        })
    };

    Provider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        settings_config,
        website_url: None,
        category: Some(GENERATED_CATEGORY.to_string()),
        created_at: Some(chrono::Utc::now().timestamp_millis()),
        sort_index: Some(sort_index),
        notes: (!provider.notes.trim().is_empty()).then(|| provider.notes.clone()),
        meta: Some(provider_meta(provider)),
        icon: Some(match provider.api_format {
            GatewayApiFormat::Anthropic => "anthropic".to_string(),
            _ => "openai".to_string(),
        }),
        icon_color: None,
        in_failover_queue: false,
    }
}

fn sync_generated_providers(db: &Database, config: &GatewayConfig) -> Result<(), AppError> {
    let wanted: HashSet<String> = config
        .providers
        .iter()
        .map(|provider| provider.id.clone())
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

        for (index, provider) in config.providers.iter().enumerate() {
            db.save_provider(
                app_type,
                &materialize_provider(config, provider, app_type, index),
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
    let Some(route) = config
        .routes
        .iter()
        .find(|route| route.enabled && route.alias == alias)
    else {
        return Ok(None);
    };

    let configured: HashMap<&str, &GatewayProvider> = config
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| (provider.id.as_str(), provider))
        .collect();

    let mut result = Vec::new();
    for target in route.targets.iter().filter(|target| target.enabled) {
        if !configured.contains_key(target.provider_id.as_str()) {
            continue;
        }

        if let Some(provider) = db.get_provider_by_id(&target.provider_id, app_type)? {
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

    if bearer == Some(expected) || x_api_key == Some(expected) {
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
    let data: Vec<Value> = config
        .routes
        .iter()
        .filter(|route| route.enabled)
        .map(|route| {
            json!({
                "id": route.alias.clone(),
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

    // Route order is managed by the gateway itself, so allow the inherited
    // forwarder to attempt every target in the selected route chain.
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

#[tauri::command]
pub fn generate_gateway_api_key() -> String {
    generate_local_key()
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
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_skip_taskbar(false);
                let _ = window.set_focus();
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

    fn provider(id: &str, format: GatewayApiFormat) -> GatewayProvider {
        GatewayProvider {
            id: id.to_string(),
            name: id.to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: "test-key".to_string(),
            api_format: format,
            enabled: true,
            auth_style: "auto".to_string(),
            custom_headers: HashMap::new(),
            notes: String::new(),
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

    #[test]
    fn duplicate_provider_in_one_route_is_rejected() {
        let mut config = GatewayConfig::default();
        config.providers.push(provider("p1", GatewayApiFormat::OpenaiChat));
        config.routes.push(GatewayRoute {
            alias: "local".to_string(),
            enabled: true,
            targets: vec![
                GatewayRouteTarget {
                    provider_id: "p1".to_string(),
                    upstream_model: "model-a".to_string(),
                    enabled: true,
                },
                GatewayRouteTarget {
                    provider_id: "p1".to_string(),
                    upstream_model: "model-b".to_string(),
                    enabled: true,
                },
            ],
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn enabled_route_requires_an_enabled_provider() {
        let mut config = GatewayConfig::default();
        let mut disabled = provider("p1", GatewayApiFormat::OpenaiChat);
        disabled.enabled = false;
        config.providers.push(disabled);
        config.routes.push(GatewayRoute {
            alias: "local".to_string(),
            enabled: true,
            targets: vec![GatewayRouteTarget {
                provider_id: "p1".to_string(),
                upstream_model: "model-a".to_string(),
                enabled: true,
            }],
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn materialized_provider_contains_exact_alias_mapping() {
        let mut config = GatewayConfig::default();
        config.providers.push(provider("p1", GatewayApiFormat::OpenaiResponses));
        config.routes.push(GatewayRoute {
            alias: "best-code".to_string(),
            enabled: true,
            targets: vec![GatewayRouteTarget {
                provider_id: "p1".to_string(),
                upstream_model: "gpt-test".to_string(),
                enabled: true,
            }],
        });
        let generated = materialize_provider(&config, &config.providers[0], "codex", 0);
        assert_eq!(
            generated.settings_config["gateway_model_map"]["best-code"],
            Value::String("gpt-test".to_string())
        );
        assert_eq!(
            generated.meta.and_then(|meta| meta.api_format),
            Some("openai_responses".to_string())
        );
    }
}
