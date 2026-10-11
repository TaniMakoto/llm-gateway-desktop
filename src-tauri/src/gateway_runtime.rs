//! Standalone gateway lifecycle. This service never reads or writes CLI live configs.
use crate::{
    database::Database,
    proxy::{server::ProxyServer, CircuitBreakerStats, ProxyConfig, ProxyServerInfo, ProxyStatus},
};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

pub struct GatewayRuntime {
    db: Arc<Database>,
    // One lock covers the entire start/stop/reconfigure transaction. Concurrent
    // tray/UI requests cannot start two listeners or publish a stopped instance.
    server: Arc<Mutex<Option<ProxyServer>>>,
    app_handle: RwLock<Option<tauri::AppHandle>>,
}

impl GatewayRuntime {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            server: Arc::new(Mutex::new(None)),
            app_handle: RwLock::new(None),
        }
    }

    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        *self.app_handle.write().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    fn create_server(&self, config: ProxyConfig) -> ProxyServer {
        let handle = self
            .app_handle
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        ProxyServer::new(config, self.db.clone(), handle)
    }

    pub async fn start(&self) -> Result<ProxyServerInfo, String> {
        let mut guard = self.server.lock().await;
        if let Some(server) = guard.as_ref() {
            let status = server.get_status().await;
            return Ok(ProxyServerInfo {
                address: status.address,
                port: status.port,
                started_at: chrono::Utc::now().to_rfc3339(),
            });
        }
        let config = self.get_config().await?;
        let server = self.create_server(config);
        let info = server.start().await.map_err(|e| e.to_string())?;
        *guard = Some(server);
        Ok(info)
    }

    pub async fn stop(&self) -> Result<(), String> {
        let mut guard = self.server.lock().await;
        if let Some(server) = guard.take() {
            server.stop().await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub async fn get_status(&self) -> Result<ProxyStatus, String> {
        let guard = self.server.lock().await;
        Ok(match guard.as_ref() {
            Some(server) => server.get_status().await,
            None => ProxyStatus::default(),
        })
    }

    pub async fn is_running(&self) -> bool {
        self.server.lock().await.is_some()
    }

    pub async fn get_config(&self) -> Result<ProxyConfig, String> {
        self.db.get_proxy_config().await.map_err(|e| e.to_string())
    }

    pub async fn update_config(&self, config: &ProxyConfig) -> Result<(), String> {
        self.commit_reconfiguration(config, || {
            self.db
                .update_proxy_config_sync(config)
                .map_err(|error| error.to_string())
        })
        .await
    }

    pub(crate) async fn commit_reconfiguration(
        &self,
        config: &ProxyConfig,
        persist: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut guard = self.server.clone().lock_owned().await;
        let previous = self.get_config().await?;
        let restart = guard.is_some()
            && (previous.listen_address != config.listen_address
                || previous.listen_port != config.listen_port);
        // Bind the replacement before touching the working listener or persisted
        // configuration. A port conflict must not take a healthy gateway offline.
        let replacement = if let Some(current) = guard.as_ref().filter(|_| restart) {
            let server = current.replacement(config.clone());
            let listener = server
                .prepare_listener()
                .await
                .map_err(|error| error.to_string())?;
            let address = listener.local_addr().map_err(|error| error.to_string())?;
            Some((server, listener, address))
        } else {
            None
        };
        // Every fallible preparation happens before changing persistent state.
        // A rejected transaction drops the reserved listener and leaves the old
        // server untouched. No replacement request runs against a partial config.
        persist()?;
        let config = config.clone();
        // Once storage commits, publication must finish even if the invoking UI
        // task is cancelled. The owned lock stays with this publication task.
        tokio::spawn(async move {
            if let Some((server, listener, address)) = replacement {
                server.apply_runtime_config(&config).await;
                server.start_prepared(listener, address).await;
                if let Some(old) = guard.replace(server) {
                    if let Err(error) = old.stop().await {
                        log::warn!("新配置已发布，旧监听停止异常: {error}");
                    }
                }
            } else if let Some(server) = guard.as_ref() {
                server.apply_runtime_config(&config).await;
            }
        })
        .await
        .map_err(|error| format!("配置发布任务异常: {error}"))
    }

    pub async fn get_circuit_breaker_stats(
        &self,
        id: &str,
        app_type: &str,
    ) -> Option<CircuitBreakerStats> {
        match self.server.lock().await.as_ref() {
            Some(server) => server.get_circuit_breaker_stats(id, app_type).await,
            None => None,
        }
    }
    pub async fn get_provider_capacity_snapshot(&self, id: &str) -> (u32, u32, u32) {
        match self.server.lock().await.as_ref() {
            Some(server) => server.get_provider_capacity_snapshot(id),
            None => (0, 0, 0),
        }
    }
    pub async fn get_provider_cooldown_remaining_seconds(
        &self,
        id: &str,
        app_type: &str,
    ) -> Option<u64> {
        match self.server.lock().await.as_ref() {
            Some(server) => {
                server
                    .get_provider_cooldown_remaining_seconds(id, app_type)
                    .await
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn rejected_persistence_keeps_old_listener_and_releases_reserved_port() {
        let db = Arc::new(Database::memory().unwrap());
        let runtime = GatewayRuntime::new(db);
        let previous = ProxyConfig {
            listen_port: 0,
            ..Default::default()
        };
        runtime.update_config(&previous).await.unwrap();
        let info = runtime.start().await.unwrap();
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let new_port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let next = ProxyConfig {
            listen_port: new_port,
            ..previous.clone()
        };
        assert!(runtime
            .commit_reconfiguration(&next, || Err("write failed".into()))
            .await
            .is_err());
        assert_eq!(
            runtime.get_config().await.unwrap().listen_port,
            previous.listen_port
        );
        assert_eq!(runtime.get_status().await.unwrap().port, info.port);
        let available = tokio::net::TcpListener::bind(("127.0.0.1", new_port))
            .await
            .unwrap();
        drop(available);
        runtime.stop().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_start_and_failed_rebind_preserve_the_live_listener() {
        let db = Arc::new(Database::memory().unwrap());
        let runtime = GatewayRuntime::new(db.clone());
        let config = ProxyConfig {
            listen_port: 0,
            enable_logging: false,
            ..Default::default()
        };
        runtime.update_config(&config).await.unwrap();
        let (a, b) = tokio::join!(runtime.start(), runtime.start());
        let a = a.unwrap();
        assert_eq!(a.port, b.unwrap().port);
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blocked = ProxyConfig {
            listen_port: occupied.local_addr().unwrap().port(),
            ..config.clone()
        };
        assert!(runtime.update_config(&blocked).await.is_err());
        assert_eq!(runtime.get_config().await.unwrap().listen_port, 0);
        assert_eq!(runtime.get_status().await.unwrap().port, a.port);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(client
            .get(format!("http://127.0.0.1:{}/health", a.port))
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        runtime.stop().await.unwrap();
        runtime.stop().await.unwrap();
        assert!(!runtime.is_running().await);
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", a.port))
            .await
            .is_err());
        // Runtime must never enable CLI takeover as a side effect of starting.
        for app in ["claude", "codex"] {
            assert!(!db.get_proxy_config_for_app(app).await.unwrap().enabled);
        }
    }
}
