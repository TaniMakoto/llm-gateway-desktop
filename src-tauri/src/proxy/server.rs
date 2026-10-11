//! HTTP代理服务器
//!
//! 基于Axum的HTTP服务器，处理代理请求
//!
//! Uses a manual hyper HTTP/1.1 accept loop with `preserve_header_case(true)` so
//! that the original header-name casing from the CLI client is captured in a
//! `HeaderCaseMap` extension.  This map is later forwarded to the upstream via
//! the hyper-based HTTP client, producing wire-level header casing identical to
//! a direct (non-proxied) CLI request.

use super::{
    failover_switch::FailoverSwitchManager,
    handlers,
    log_codes::srv as log_srv,
    provider_router::ProviderRouter,
    providers::{codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore},
    session_affinity::SessionAffinityStore,
    types::*,
    ProxyError,
};
use crate::database::Database;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;

/// 代理服务器状态（共享）
#[derive(Clone)]
pub struct ProxyState {
    pub db: Arc<Database>,
    pub config: Arc<RwLock<ProxyConfig>>,
    pub status: Arc<RwLock<ProxyStatus>>,
    pub start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// 每个应用类型当前使用的 provider (app_type -> (provider_id, provider_name))
    pub current_providers: Arc<RwLock<std::collections::HashMap<String, (String, String)>>>,
    /// 正在处理请求的实际上游 ((app_type, provider_id) -> (request_count, fallback_name))
    pub active_provider_counts:
        Arc<RwLock<std::collections::HashMap<(String, String), (usize, String)>>>,
    /// 共享的 ProviderRouter（持有熔断器状态，跨请求保持）
    pub provider_router: Arc<ProviderRouter>,
    /// Gemini Native shadow state，用于 thoughtSignature / tool call 回放
    pub gemini_shadow: Arc<GeminiShadowStore>,
    /// Codex Chat bridge history，用于恢复 previous_response_id 指向的 tool call
    pub codex_chat_history: Arc<CodexChatHistoryStore>,
    /// 会话 → provider 绑定表（会话亲和，跨请求保持）
    ///
    /// 见 `proxy::session_affinity`。只对**稳定**会话键建立绑定：
    /// 客户端显式提供的会话 ID，或由内容哈希派生的稳定键。
    pub session_affinity: Arc<RwLock<SessionAffinityStore>>,
    /// AppHandle，用于发射事件和更新托盘菜单
    pub app_handle: Option<tauri::AppHandle>,
    /// 故障转移切换管理器
    pub failover_manager: Arc<FailoverSwitchManager>,
}

/// 代理HTTP服务器
pub struct ProxyServer {
    config: ProxyConfig,
    state: ProxyState,
    shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    /// 服务器任务句柄，用于等待服务器实际关闭
    server_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    /// Shared by replacement listeners so retiring one cannot clear the new status.
    listener_generation: Arc<AtomicU64>,
}

impl ProxyServer {
    pub fn new(
        config: ProxyConfig,
        db: Arc<Database>,
        app_handle: Option<tauri::AppHandle>,
    ) -> Self {
        // 创建共享的 ProviderRouter（熔断器状态将跨所有请求保持）
        let provider_router = Arc::new(ProviderRouter::new(db.clone()));
        // 创建故障转移切换管理器
        let failover_manager = Arc::new(FailoverSwitchManager::new(db.clone()));

        let state = ProxyState {
            db,
            config: Arc::new(RwLock::new(config.clone())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            start_time: Arc::new(RwLock::new(None)),
            current_providers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            active_provider_counts: Arc::new(RwLock::new(std::collections::HashMap::new())),
            provider_router,
            gemini_shadow: Arc::new(GeminiShadowStore::default()),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
            session_affinity: Arc::new(RwLock::new(SessionAffinityStore::default())),
            app_handle,
            failover_manager,
        };

        Self {
            config,
            state,
            shutdown_tx: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
            listener_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Rebinding preserves capacity, health, affinity and active connection state.
    pub(crate) fn replacement(&self, config: ProxyConfig) -> Self {
        Self {
            config,
            state: self.state.clone(),
            shutdown_tx: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
            listener_generation: self.listener_generation.clone(),
        }
    }

    pub async fn start(&self) -> Result<ProxyServerInfo, ProxyError> {
        let listener = self.prepare_listener().await?;
        self.start_with_listener(listener).await
    }

    /// Reserve the socket without accepting requests or changing runtime state.
    pub(crate) async fn prepare_listener(&self) -> Result<tokio::net::TcpListener, ProxyError> {
        // 检查是否已在运行
        if self.shutdown_tx.read().await.is_some() {
            return Err(ProxyError::AlreadyRunning);
        }

        let ip = self.config.listen_address.parse::<std::net::IpAddr>()
            .map_err(|error| ProxyError::BindFailed(format!("无效的地址: {error}")))?;
        let addr = SocketAddr::new(ip, self.config.listen_port);

        tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ProxyError::BindFailed(e.to_string()))
    }

    pub(crate) async fn start_with_listener(
        &self,
        listener: tokio::net::TcpListener,
    ) -> Result<ProxyServerInfo, ProxyError> {
        if self.shutdown_tx.read().await.is_some() {
            return Err(ProxyError::AlreadyRunning);
        }
        let local_addr = listener
            .local_addr()
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        Ok(self.start_prepared(listener, local_addr).await)
    }

    /// All socket validation has already happened; activation has no fallible I/O.
    pub(crate) async fn start_prepared(
        &self,
        listener: tokio::net::TcpListener,
        local_addr: SocketAddr,
    ) -> ProxyServerInfo {
        let actual_port = local_addr.port();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let app = self.build_router();

        log::info!("[{}] 代理服务器启动于 {local_addr}", log_srv::STARTED);

        // 更新全局代理端口，用于系统代理检测
        crate::proxy::http_client::set_proxy_port(actual_port);

        // 保存关闭句柄
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // 更新状态
        let mut status = self.state.status.write().await;
        let generation = self.listener_generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        status.running = true;
        status.address = self.config.listen_address.clone();
        status.port = actual_port;
        // 记录启动时间
        *self.state.start_time.write().await = Some(std::time::Instant::now());
        drop(status);

        // 启动服务器 — 使用手动 hyper HTTP/1.1 accept loop
        // 开启 preserve_header_case 以捕获客户端请求头的原始大小写
        let state = self.state.clone();
        let listener_generation = self.listener_generation.clone();
        let handle = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            let (connection_shutdown, _) = tokio::sync::watch::channel(false);
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _remote_addr) = match result {
                            Ok(v) => v,
                            Err(e) => {
                                log::error!("[{SRV}] accept 失败: {e}", SRV = log_srv::ACCEPT_ERR);
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };

                        let app = app.clone();
                        let mut connection_shutdown = connection_shutdown.subscribe();
                        tokio::spawn(async move {
                            // Peek raw TCP bytes to capture original header casing
                            // before hyper parses (and lowercases) the header names.
                            let original_cases = {
                                let mut peek_buf = vec![0u8; 8192];
                                let peek = tokio::select! {
                                    biased;
                                    _ = connection_shutdown.changed() => return,
                                    result = stream.peek(&mut peek_buf) => result,
                                };
                                match peek {
                                    Ok(n) => {
                                        let cases = super::hyper_client::OriginalHeaderCases::from_raw_bytes(&peek_buf[..n]);
                                        log::debug!(
                                            "[ProxyServer] Peeked {} bytes, captured {} header casings",
                                            n, cases.cases.len()
                                        );
                                        cases
                                    }
                                    Err(e) => {
                                        log::debug!("[ProxyServer] peek failed (non-fatal): {e}");
                                        super::hyper_client::OriginalHeaderCases::default()
                                    }
                                }
                            };

                            // service_fn 将 axum Router（tower::Service）桥接到 hyper
                            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                                let mut router = app.clone();
                                let cases = original_cases.clone();
                                async move {
                                    // 将 hyper::body::Incoming 转为 axum::body::Body，保留 extensions
                                    let (mut parts, body) = req.into_parts();

                                    // Insert our own header case map alongside hyper's internal one
                                    parts.extensions.insert(cases);

                                    let body = axum::body::Body::new(body);
                                    let axum_req = http::Request::from_parts(parts, body);
                                    <Router as tower::Service<http::Request<axum::body::Body>>>::call(&mut router, axum_req).await
                                }
                            });

                            let connection = hyper::server::conn::http1::Builder::new()
                                .preserve_header_case(true)
                                .serve_connection(TokioIo::new(stream), service);
                            tokio::pin!(connection);
                            let result = tokio::select! {
                                biased;
                                _ = connection_shutdown.changed() => {
                                    connection.as_mut().graceful_shutdown();
                                    connection.await
                                },
                                result = connection.as_mut() => result,
                            };
                            if let Err(e) = result {
                                // Connection reset / broken pipe 等在代理场景下很常见，debug 级别
                                log::debug!("[{SRV}] connection error: {e}", SRV = log_srv::CONN_ERR);
                            }
                        });
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }

            // Retire idle keepalive connections and finish an already-started
            // response; the old address must not keep accepting new requests.
            let _ = connection_shutdown.send(true);
            // A retired listener shares state with its replacement. Clear status
            // only while owning the current generation, under the same lock used
            // to activate it, so old shutdown cannot erase the new start time.
            let mut status = state.status.write().await;
            if listener_generation.load(Ordering::SeqCst) == generation {
                status.running = false;
                *state.start_time.write().await = None;
            }
        });

        // 保存服务器任务句柄
        *self.server_handle.write().await = Some(handle);

        ProxyServerInfo {
            address: self.config.listen_address.clone(),
            port: actual_port,
            started_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub async fn stop(&self) -> Result<(), ProxyError> {
        // 1. 发送关闭信号
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        } else {
            return Err(ProxyError::NotRunning);
        }

        // 2. 等待服务器任务结束（带 5 秒超时保护）
        if let Some(handle) = self.server_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    log::info!("[{}] 代理服务器已完全停止", log_srv::STOPPED);
                    Ok(())
                }
                Ok(Err(e)) => {
                    log::warn!("[{}] 代理服务器任务异常终止: {e}", log_srv::TASK_ERROR);
                    Err(ProxyError::StopFailed(e.to_string()))
                }
                Err(_) => {
                    log::warn!(
                        "[{}] 代理服务器停止超时（5秒），强制继续",
                        log_srv::STOP_TIMEOUT
                    );
                    Err(ProxyError::StopTimeout)
                }
            }
        } else {
            Ok(())
        }
    }

    pub async fn get_status(&self) -> ProxyStatus {
        let mut status = self.state.status.read().await.clone();

        // 计算运行时间
        if let Some(start) = *self.state.start_time.read().await {
            status.uptime_seconds = start.elapsed().as_secs();
        }

        // 从 current_providers HashMap 获取每个应用类型当前正在使用的 provider
        let current_providers = self.state.current_providers.read().await;
        status.active_targets = current_providers
            .iter()
            .map(|(app_type, (provider_id, provider_name))| ActiveTarget {
                app_type: app_type.clone(),
                provider_id: provider_id.clone(),
                provider_name: provider_name.clone(),
            })
            .collect();

        // 活跃请求按实际上游聚合。名称优先从数据库实时解析，因此供应商改名后
        // 无需等待下一次请求，状态卡也能在下一轮轮询中立即显示新名称。
        let active_provider_counts = self.state.active_provider_counts.read().await;
        status.active_providers = active_provider_counts
            .iter()
            .filter_map(
                |((app_type, provider_id), (request_count, fallback_name))| {
                    if *request_count == 0 {
                        return None;
                    }
                    let provider_name = self
                        .state
                        .db
                        .get_provider_by_id(provider_id, app_type)
                        .ok()
                        .flatten()
                        .map(|provider| provider.name)
                        .unwrap_or_else(|| fallback_name.clone());
                    Some(ActiveProvider {
                        app_type: app_type.clone(),
                        provider_name,
                        provider_id: provider_id.clone(),
                        request_count: *request_count,
                    })
                },
            )
            .collect();
        status.active_providers.sort_by(|a, b| {
            a.provider_name
                .cmp(&b.provider_name)
                .then_with(|| a.app_type.cmp(&b.app_type))
                .then_with(|| a.provider_id.cmp(&b.provider_id))
        });

        // current_provider 现在表示“最近一次成功使用”。同样按稳定 ID 动态解析名称，
        // 修复配置改名后状态中仍长期保留旧字符串的问题。
        if let (Some(provider_id), Some(app_type)) = (
            status.current_provider_id.clone(),
            status.current_provider_app_type.clone(),
        ) {
            if let Ok(Some(provider)) = self.state.db.get_provider_by_id(&provider_id, &app_type) {
                status.current_provider = Some(provider.name);
            }
        }

        status
    }

    /// 更新某个应用类型当前“目标供应商”（用于 UI 展示 active_targets）
    ///
    /// 注意：这不代表该供应商一定已经处理过请求，而是用于“热切换/启用故障转移立即切 P1”
    /// 等场景下，让 UI 能立刻反映最新目标。
    pub async fn set_active_target(&self, app_type: &str, provider_id: &str, provider_name: &str) {
        let mut current_providers = self.state.current_providers.write().await;
        current_providers.insert(
            app_type.to_string(),
            (provider_id.to_string(), provider_name.to_string()),
        );
    }

    fn build_router(&self) -> Router {
        Router::new()
            .route("/health", get(handlers::health_check))
            .route("/v1/gateway/status", get(handlers::handle_gateway_status))
            .route("/v1/models", get(handlers::handle_models))
            .route("/v1/messages", post(handlers::handle_messages))
            .route(
                "/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route("/v1/responses", post(handlers::handle_responses))
            .route(
                "/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
            .with_state(self.state.clone())
    }

    /// 在不重启服务的情况下更新运行时配置
    pub async fn apply_runtime_config(&self, config: &ProxyConfig) {
        *self.state.config.write().await = config.clone();
    }

    /// 热更新熔断器配置
    ///
    /// 将新配置应用到所有已创建的熔断器实例
    pub async fn update_circuit_breaker_configs(
        &self,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state.provider_router.update_all_configs(config).await;
    }

    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state
            .provider_router
            .update_app_configs(app_type, config)
            .await;
    }

    /// 重置指定 Provider 的熔断器
    pub async fn reset_provider_circuit_breaker(&self, provider_id: &str, app_type: &str) {
        self.state
            .provider_router
            .reset_provider_breaker(provider_id, app_type)
            .await;
    }

    /// 读取指定 Provider 当前进程内的熔断器统计。
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<super::circuit_breaker::CircuitBreakerStats> {
        self.state
            .provider_router
            .get_circuit_breaker_stats(provider_id, app_type)
            .await
    }

    pub async fn get_provider_cooldown_remaining_seconds(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<u64> {
        self.state
            .provider_router
            .provider_cooldown_remaining_seconds(provider_id, app_type)
            .await
    }

    pub fn get_provider_capacity_snapshot(&self, source_provider_id: &str) -> (u32, u32, u32) {
        self.state
            .provider_router
            .provider_capacity_snapshot(source_provider_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderMeta};

    #[tokio::test]
    #[serial_test::serial]
    async fn stopping_listener_closes_an_idle_keepalive_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server = ProxyServer::new(
            ProxyConfig { listen_port: 0, ..Default::default() },
            Arc::new(Database::memory().unwrap()), None,
        );
        let info = server.start().await.unwrap();
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", info.port)).await.unwrap();
        socket.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n").await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut bytes = [0; 1024];
                let read = socket.read(&mut bytes).await.unwrap();
                assert!(read > 0);
                response.extend_from_slice(&bytes[..read]);
                if let Some(end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
                    let length = headers.lines().find_map(|line| line.strip_prefix("content-length:").map(|value| value.trim().parse::<usize>().unwrap())).unwrap();
                    if response.len() >= end + 4 + length { break; }
                }
            }
        }).await.unwrap();
        server.stop().await.unwrap();
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut [0; 1])).await.unwrap().unwrap();
        assert_eq!(closed, 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rebind_preserves_inflight_capacity_and_new_listener_status() {
        let config = ProxyConfig { listen_port: 0, ..Default::default() };
        let old = ProxyServer::new(config.clone(), Arc::new(Database::memory().unwrap()), None);
        let old_info = old.start().await.unwrap();
        let mut provider = Provider::with_id("source::chat".into(), "fixture".into(), serde_json::json!({}), None);
        provider.meta = Some(ProviderMeta {
            gateway_source_provider_id: Some("source".into()),
            gateway_max_concurrent_requests: Some(1),
            ..Default::default()
        });
        let capacity = old.state.provider_router.try_acquire_provider_capacity(&provider).unwrap();
        let new = old.replacement(config.clone());
        let new_info = new.start().await.unwrap();
        assert_ne!(old_info.port, new_info.port);
        old.stop().await.unwrap();
        let status = new.get_status().await;
        assert!(status.running);
        assert_eq!(status.port, new_info.port);
        assert_eq!(new.get_provider_capacity_snapshot("source"), (1, 1, 0));
        assert!(!new.state.provider_router.provider_capacity_available(&provider));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(client.get(format!("http://127.0.0.1:{}/health", new_info.port)).send().await.unwrap().status().is_success());
        drop(capacity);
        assert_eq!(new.get_provider_capacity_snapshot("source"), (1, 0, 0));
        new.stop().await.unwrap();
        assert!(!new.get_status().await.running);
    }
}
