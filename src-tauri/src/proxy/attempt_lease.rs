//! 单次候选尝试的资源所有权；取消或换源时同步释放，不依赖异步清理任务。

use super::{
    circuit_breaker::CircuitRequestPermit,
    provider_router::{ProviderCapacityPermit, ProviderRouter},
    request_trace::StreamOutcome,
    session_affinity::SessionAffinityStore,
};
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) struct AttemptLease {
    capacity: Option<ProviderCapacityPermit>,
    circuit: CircuitRequestPermit,
}

impl AttemptLease {
    pub(crate) fn new(
        capacity: Option<ProviderCapacityPermit>,
        circuit: CircuitRequestPermit,
    ) -> Self {
        Self { capacity, circuit }
    }

    /// 成功候选把容量交给响应 body，把探测许可交给最终健康记录。
    pub(crate) fn into_response_parts(
        self,
    ) -> (Option<ProviderCapacityPermit>, CircuitRequestPermit) {
        (self.capacity, self.circuit)
    }
}

/// 最终结果回调的所有权对象。只允许消耗一次；未调用时 Drop 中性释放探测名额。
pub(crate) struct ResponseHealthGuard {
    pub(crate) router: Arc<ProviderRouter>,
    pub(crate) provider_id: String,
    pub(crate) app_type: String,
    pub(crate) circuit: CircuitRequestPermit,
    pub(crate) affinity_key: Option<String>,
    pub(crate) affinity: Arc<RwLock<SessionAffinityStore>>,
}

impl ResponseHealthGuard {
    pub(crate) fn complete(self, outcome: StreamOutcome) {
        if outcome == StreamOutcome::Cancelled {
            // 客户端取消只归还资源，不记供应商失败。
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let success = matches!(outcome, StreamOutcome::Success | StreamOutcome::Incomplete);
                if let Err(error) = self
                    .router
                    .record_result(
                        &self.provider_id,
                        &self.app_type,
                        false, // circuit 的 Drop 独占探测名额归还。
                        success,
                        (!success).then(|| "上游响应未正常完成".to_string()),
                    )
                    .await
                {
                    log::warn!("记录响应最终健康结果失败: {error}");
                }
                if !success {
                    if let Some(key) = &self.affinity_key {
                        self.affinity
                            .write()
                            .await
                            .compare_and_delete(key, &self.provider_id);
                    }
                }
                // 健康状态更新结束后才释放半开许可，避免其他探测抢先进入。
                drop(self.circuit);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        database::Database,
        provider::{Provider, ProviderMeta},
    };
    use serde_json::json;
    use std::time::Duration;

    fn provider(id: &str) -> Provider {
        let mut provider = Provider::with_id(id.into(), id.into(), json!({}), None);
        provider.meta = Some(ProviderMeta {
            gateway_source_provider_id: Some("source".into()),
            gateway_max_concurrent_requests: Some(1),
            ..Default::default()
        });
        provider
    }

    async fn router_with_probe(provider: &Provider) -> Arc<ProviderRouter> {
        let db = Arc::new(Database::memory().unwrap());
        db.save_provider("claude", provider).unwrap();
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.circuit_failure_threshold = 1;
        config.circuit_timeout_seconds = 0;
        db.update_proxy_config_for_app(config).await.unwrap();
        let router = Arc::new(ProviderRouter::new(db));
        router
            .record_result(&provider.id, "claude", false, false, None)
            .await
            .unwrap();
        router
    }

    #[tokio::test]
    async fn cancelled_send_releases_capacity_and_probe_without_health_failure() {
        let provider = provider("source::chat");
        let router = router_with_probe(&provider).await;
        let before = router
            .get_circuit_breaker_stats(&provider.id, "claude")
            .await
            .unwrap();
        let capacity = router.try_acquire_provider_capacity(&provider).unwrap();
        let lease = router
            .acquire_attempt(&provider, "claude", "alias", capacity)
            .await
            .unwrap();
        assert!(lease.circuit.is_half_open_probe());
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 1, 0));
        let (entered, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _lease = lease;
            entered.send(()).unwrap();
            futures::future::pending::<()>().await;
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 0, 0));
        let after = router
            .get_circuit_breaker_stats(&provider.id, "claude")
            .await
            .unwrap();
        assert_eq!(after.total_requests, before.total_requests);
        assert_eq!(after.failed_requests, before.failed_requests);
        let capacity = router.try_acquire_provider_capacity(&provider).unwrap();
        assert!(router
            .acquire_attempt(&provider, "claude", "alias", capacity)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn failed_attempt_releases_source_capacity_before_next_protocol_candidate() {
        let router = ProviderRouter::new(Arc::new(Database::memory().unwrap()));
        let chat = provider("source::chat");
        let responses = provider("source::responses");
        let capacity = router.try_acquire_provider_capacity(&chat).unwrap();
        let lease = router
            .acquire_attempt(&chat, "claude", "alias", capacity)
            .await
            .unwrap();
        assert!(!router.provider_capacity_available(&responses));
        // 候选失败离开作用域后，下一协议可以获取同一个源供应商的容量。
        drop(lease);
        let capacity = router.try_acquire_provider_capacity(&responses).unwrap();
        let lease = router
            .acquire_attempt(&responses, "claude", "alias", capacity)
            .await
            .unwrap();
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 1, 0));
        drop(lease);
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 0, 0));
    }

    #[tokio::test]
    async fn successful_attempt_transfers_capacity_until_response_is_dropped() {
        let provider = provider("source::chat");
        let router = router_with_probe(&provider).await;
        let capacity = router.try_acquire_provider_capacity(&provider).unwrap();
        let lease = router
            .acquire_attempt(&provider, "claude", "alias", capacity)
            .await
            .unwrap();
        let (response_capacity, circuit) = lease.into_response_parts();
        let health = ResponseHealthGuard {
            router: router.clone(),
            provider_id: provider.id.clone(),
            app_type: "claude".into(),
            circuit,
            affinity_key: None,
            affinity: Arc::new(RwLock::new(SessionAffinityStore::default())),
        };
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 1, 0));
        health.complete(StreamOutcome::Cancelled);
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 1, 0));
        assert!(router
            .acquire_attempt(&provider, "claude", "alias", None)
            .await
            .is_some());
        drop(response_capacity);
        assert_eq!(router.provider_capacity_snapshot("source"), (1, 0, 0));
    }

    #[tokio::test]
    async fn final_health_distinguishes_limit_upstream_failure_and_client_cancel() {
        for outcome in [
            StreamOutcome::Success,
            StreamOutcome::Incomplete,
            StreamOutcome::UpstreamFailure,
            StreamOutcome::Cancelled,
        ] {
            let provider = provider("source::chat");
            let db = Arc::new(Database::memory().unwrap());
            db.save_provider("claude", &provider).unwrap();
            let router = Arc::new(ProviderRouter::new(db));
            let lease = router
                .acquire_attempt(&provider, "claude", "alias", None)
                .await
                .unwrap();
            let (_, circuit) = lease.into_response_parts();
            let affinity = Arc::new(RwLock::new(SessionAffinityStore::default()));
            affinity.write().await.bind("session", &provider.id);
            ResponseHealthGuard {
                router: router.clone(),
                provider_id: provider.id.clone(),
                app_type: "claude".into(),
                circuit,
                affinity_key: Some("session".into()),
                affinity: affinity.clone(),
            }
            .complete(outcome);
            if outcome != StreamOutcome::Cancelled {
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let stats = router
                            .get_circuit_breaker_stats(&provider.id, "claude")
                            .await
                            .unwrap();
                        let binding = affinity.write().await.get("session");
                        if stats.total_requests == 1
                            && (outcome != StreamOutcome::UpstreamFailure || binding.is_none())
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
            let stats = router
                .get_circuit_breaker_stats(&provider.id, "claude")
                .await
                .unwrap();
            assert_eq!(
                stats.total_requests,
                u32::from(outcome != StreamOutcome::Cancelled)
            );
            assert_eq!(
                stats.failed_requests,
                u32::from(outcome == StreamOutcome::UpstreamFailure)
            );
            assert_eq!(
                affinity.write().await.get("session").is_none(),
                outcome == StreamOutcome::UpstreamFailure
            );
        }
    }
}
