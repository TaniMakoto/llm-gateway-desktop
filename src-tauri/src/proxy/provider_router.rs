//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::gateway::GatewayRoutingPolicy;
use crate::provider::Provider;
use crate::proxy::circuit_breaker::{AllowResult, CircuitBreaker, CircuitBreakerConfig};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};

#[derive(Debug)]
struct ProviderCapacityState {
    limit: u32,
    in_use: u32,
    waiters: u32,
    notify: Arc<Notify>,
}

struct ProviderQueueWaiterGuard {
    source_provider_id: String,
    states: Arc<Mutex<HashMap<String, ProviderCapacityState>>>,
}

impl Drop for ProviderQueueWaiterGuard {
    fn drop(&mut self) {
        let mut states = self.states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = states.get_mut(&self.source_provider_id) {
            state.waiters = state.waiters.saturating_sub(1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityAcquireError {
    Full,
    QueueFull,
    Timeout,
}

#[derive(Debug)]
pub struct ProviderCapacityPermit {
    source_provider_id: String,
    states: Arc<Mutex<HashMap<String, ProviderCapacityState>>>,
}

impl Drop for ProviderCapacityPermit {
    fn drop(&mut self) {
        let notify = {
            let mut states = self.states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            states.get_mut(&self.source_provider_id).map(|state| {
                state.in_use = state.in_use.saturating_sub(1);
                state.notify.clone()
            })
        };
        if let Some(notify) = notify {
            notify.notify_one();
        }
    }
}

/// 供应商路由器
pub struct ProviderRouter {
    /// 数据库连接
    db: Arc<Database>,
    /// 熔断器管理器 - key 格式: "app_type:provider_id"
    circuit_breakers: Arc<RwLock<HashMap<String, Arc<CircuitBreaker>>>>,
    /// Provider 临时冷却表（典型来源：HTTP 429 / Retry-After）。
    /// 与熔断器分离：限流/配额耗尽不等价于服务故障。
    cooldowns: Arc<RwLock<HashMap<String, Instant>>>,
    /// Candidate capability rejections keyed by app/provider/model/request profile.
    /// These are deliberately narrower than provider health or rate-limit cooldowns.
    route_rejections: Arc<RwLock<HashMap<(String, String, String, String), Instant>>>,
    /// Last selected candidate keyed by app_type + public model alias.
    ///
    /// Keeping an identity instead of an array index makes round-robin stable when
    /// a config reload adds, removes, or reorders candidates between requests.
    route_last_picked: Arc<RwLock<HashMap<String, String>>>,
    /// Smooth weighted round-robin current scores keyed by app_type + alias.
    weighted_route_scores: Arc<RwLock<HashMap<String, HashMap<String, i64>>>>,
    /// Source-provider admission state shared across app types and protocol materializations.
    capacity_states: Arc<Mutex<HashMap<String, ProviderCapacityState>>>,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
            cooldowns: Arc::new(RwLock::new(HashMap::new())),
            route_rejections: Arc::new(RwLock::new(HashMap::new())),
            route_last_picked: Arc::new(RwLock::new(HashMap::new())),
            weighted_route_scores: Arc::new(RwLock::new(HashMap::new())),
            capacity_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn capacity_state_for_provider<'a>(
        states: &'a mut HashMap<String, ProviderCapacityState>,
        provider: &Provider,
    ) -> (&'a mut ProviderCapacityState, String) {
        let source_id = provider.gateway_source_provider_id().to_string();
        let configured_limit = provider.gateway_max_concurrent_requests();
        let state = states.entry(source_id.clone()).or_insert_with(|| ProviderCapacityState {
            limit: configured_limit,
            in_use: 0,
            waiters: 0,
            notify: Arc::new(Notify::new()),
        });
        state.limit = configured_limit;
        (state, source_id)
    }

    pub fn provider_capacity_available(&self, provider: &Provider) -> bool {
        let limit = provider.gateway_max_concurrent_requests();
        if limit == 0 {
            return true;
        }
        let mut states = self.capacity_states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (state, _) = Self::capacity_state_for_provider(&mut states, provider);
        state.in_use < state.limit
    }

    pub fn try_acquire_provider_capacity(
        &self,
        provider: &Provider,
    ) -> Result<Option<ProviderCapacityPermit>, CapacityAcquireError> {
        if provider.gateway_max_concurrent_requests() == 0 {
            return Ok(None);
        }
        let source_id = {
            let mut states = self.capacity_states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let (state, source_id) = Self::capacity_state_for_provider(&mut states, provider);
            if state.in_use >= state.limit {
                return Err(CapacityAcquireError::Full);
            }
            state.in_use = state.in_use.saturating_add(1);
            source_id
        };
        Ok(Some(ProviderCapacityPermit {
            source_provider_id: source_id,
            states: self.capacity_states.clone(),
        }))
    }

    pub async fn wait_acquire_provider_capacity(
        &self,
        provider: &Provider,
    ) -> Result<Option<ProviderCapacityPermit>, CapacityAcquireError> {
        if provider.gateway_max_concurrent_requests() == 0 {
            return Ok(None);
        }

        let queue_limit = provider.gateway_queue_limit();
        let timeout = Duration::from_millis(provider.gateway_queue_timeout_ms());
        let (source_id, notify) = {
            let mut states = self.capacity_states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let (state, source_id) = Self::capacity_state_for_provider(&mut states, provider);
            if state.in_use < state.limit {
                state.in_use = state.in_use.saturating_add(1);
                return Ok(Some(ProviderCapacityPermit {
                    source_provider_id: source_id,
                    states: self.capacity_states.clone(),
                }));
            }
            if queue_limit == 0 || state.waiters >= queue_limit {
                return Err(CapacityAcquireError::QueueFull);
            }
            state.waiters = state.waiters.saturating_add(1);
            (source_id, state.notify.clone())
        };
        // Cancellation-safe queue accounting: if this future is dropped because
        // the client disconnects or the task is aborted, waiter count is still released.
        let _waiter_guard = ProviderQueueWaiterGuard {
            source_provider_id: source_id.clone(),
            states: self.capacity_states.clone(),
        };

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if timeout.is_zero() || tokio::time::timeout_at(deadline, notify.notified()).await.is_err() {
                return Err(CapacityAcquireError::Timeout);
            }

            let acquired = {
                let mut states = self.capacity_states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let (state, _) = Self::capacity_state_for_provider(&mut states, provider);
                if state.in_use < state.limit {
                    state.in_use = state.in_use.saturating_add(1);
                    true
                } else {
                    false
                }
            };
            if acquired {
                return Ok(Some(ProviderCapacityPermit {
                    source_provider_id: source_id,
                    states: self.capacity_states.clone(),
                }));
            }
        }
    }

    pub fn provider_capacity_snapshot(&self, source_provider_id: &str) -> (u32, u32, u32) {
        let states = self.capacity_states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        states
            .get(source_provider_id)
            .map(|state| (state.limit, state.in_use, state.waiters))
            .unwrap_or((0, 0, 0))
    }

    pub async fn provider_cooldown_remaining_seconds(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<u64> {
        let key = Self::provider_key(app_type, provider_id);
        let now = Instant::now();
        let mut cooldowns = self.cooldowns.write().await;
        match cooldowns.get(&key).copied() {
            Some(until) if until > now => Some(until.duration_since(now).as_secs().max(1)),
            Some(_) => {
                cooldowns.remove(&key);
                None
            }
            None => None,
        }
    }

    fn provider_key(app_type: &str, provider_id: &str) -> String {
        format!("{app_type}:{provider_id}")
    }

    /// 暂时把 Provider 从调度候选中移除；重复冷却只延长、不缩短已有期限。
    pub async fn cooldown_provider(
        &self,
        provider_id: &str,
        app_type: &str,
        duration: Duration,
    ) {
        let until = Instant::now() + duration.max(Duration::from_secs(1));
        let key = Self::provider_key(app_type, provider_id);
        let mut cooldowns = self.cooldowns.write().await;
        cooldowns
            .entry(key)
            .and_modify(|existing| {
                if until > *existing {
                    *existing = until;
                }
            })
            .or_insert(until);
    }

    /// Provider 是否仍处于临时冷却；到期记录会惰性清理。
    pub async fn is_provider_cooled_down(&self, provider_id: &str, app_type: &str) -> bool {
        let key = Self::provider_key(app_type, provider_id);
        let now = Instant::now();
        let mut cooldowns = self.cooldowns.write().await;
        match cooldowns.get(&key).copied() {
            Some(until) if until > now => true,
            Some(_) => {
                cooldowns.remove(&key);
                false
            }
            None => false,
        }
    }

    pub async fn clear_provider_cooldown(&self, provider_id: &str, app_type: &str) {
        let key = Self::provider_key(app_type, provider_id);
        self.cooldowns.write().await.remove(&key);
    }

    fn route_rejection_key(
        provider_id: &str,
        app_type: &str,
        model: &str,
        request_profile: &str,
    ) -> (String, String, String, String) {
        (
            app_type.to_string(),
            provider_id.to_string(),
            model.to_string(),
            request_profile.to_string(),
        )
    }

    /// Temporarily suppress one candidate for one model/request capability shape.
    /// A schema rejection must not cool down unrelated models or simpler requests.
    pub async fn reject_route_candidate(
        &self,
        provider_id: &str,
        app_type: &str,
        model: &str,
        request_profile: &str,
        duration: Duration,
    ) {
        let key = Self::route_rejection_key(provider_id, app_type, model, request_profile);
        let until = Instant::now() + duration.max(Duration::from_secs(1));
        let mut rejections = self.route_rejections.write().await;
        rejections
            .entry(key)
            .and_modify(|existing| {
                if until > *existing {
                    *existing = until;
                }
            })
            .or_insert(until);
    }

    pub async fn is_route_candidate_rejected(
        &self,
        provider_id: &str,
        app_type: &str,
        model: &str,
        request_profile: &str,
    ) -> bool {
        let key = Self::route_rejection_key(provider_id, app_type, model, request_profile);
        let now = Instant::now();
        let mut rejections = self.route_rejections.write().await;
        match rejections.get(&key).copied() {
            Some(until) if until > now => true,
            Some(_) => {
                rejections.remove(&key);
                false
            }
            None => false,
        }
    }

    pub async fn clear_route_candidate_rejection(
        &self,
        provider_id: &str,
        app_type: &str,
        model: &str,
        request_profile: &str,
    ) {
        let key = Self::route_rejection_key(provider_id, app_type, model, request_profile);
        self.route_rejections.write().await.remove(&key);
    }

    /// Apply the configured scheduling policy to one gateway alias.
    /// Session affinity is layered later by RequestForwarder and can still
    /// promote an already-bound provider ahead of this order.
    pub async fn apply_gateway_routing_policy(
        &self,
        app_type: &str,
        alias: &str,
        policy: GatewayRoutingPolicy,
        weights: &HashMap<String, u32>,
        active_provider_counts: &HashMap<(String, String), (usize, String)>,
        mut providers: Vec<Provider>,
    ) -> Vec<Provider> {
        if providers.len() <= 1 || policy == GatewayRoutingPolicy::Priority {
            return providers;
        }

        match policy {
            GatewayRoutingPolicy::Priority => {}
            GatewayRoutingPolicy::RoundRobin => {
                let key = format!("{app_type}:{}", alias.trim());
                let start = {
                    let mut last_picked = self.route_last_picked.write().await;
                    let start = last_picked
                        .get(&key)
                        .and_then(|last_id| {
                            providers
                                .iter()
                                .position(|provider| &provider.id == last_id)
                                .map(|index| (index + 1) % providers.len())
                        })
                        .unwrap_or(0);
                    last_picked.insert(key, providers[start].id.clone());
                    start
                };
                providers.rotate_left(start);
            }
            GatewayRoutingPolicy::WeightedRoundRobin => {
                let key = format!("{app_type}:{}", alias.trim());
                let selected_id = {
                    let mut all_scores = self.weighted_route_scores.write().await;
                    let scores = all_scores.entry(key).or_default();
                    let active_ids: std::collections::HashSet<&str> =
                        providers.iter().map(|provider| provider.id.as_str()).collect();
                    scores.retain(|provider_id, _| active_ids.contains(provider_id.as_str()));

                    let mut total_weight = 0i64;
                    let mut selected: Option<(String, i64)> = None;
                    for provider in &providers {
                        let weight = weights
                            .get(provider.gateway_source_provider_id())
                            .copied()
                            .unwrap_or(1)
                            .max(1) as i64;
                        total_weight += weight;
                        let score = scores.entry(provider.id.clone()).or_insert(0);
                        *score += weight;
                        if selected
                            .as_ref()
                            .is_none_or(|(_, selected_score)| *score > *selected_score)
                        {
                            selected = Some((provider.id.clone(), *score));
                        }
                    }
                    if let Some((provider_id, _)) = selected {
                        if let Some(score) = scores.get_mut(&provider_id) {
                            *score -= total_weight;
                        }
                        Some(provider_id)
                    } else {
                        None
                    }
                };

                if let Some(selected_id) = selected_id {
                    if let Some(index) = providers.iter().position(|provider| provider.id == selected_id) {
                        providers.rotate_left(index);
                    }
                }
            }
            GatewayRoutingPolicy::LeastOutstanding => {
                providers.sort_by_key(|provider| {
                    let source_id = provider.gateway_source_provider_id();
                    active_provider_counts
                        .iter()
                        .filter(|((_, materialized_id), _)| {
                            materialized_id
                                .rsplit_once("::")
                                .map(|(source, _)| source == source_id)
                                .unwrap_or(materialized_id == source_id)
                        })
                        .map(|(_, (count, _))| *count)
                        .sum::<usize>()
                });
            }
        }
        providers
    }

    /// 选择可用的供应商（支持故障转移）
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：仅使用故障转移队列，按队列顺序依次尝试（P1 → P2 → ...）
    pub async fn select_providers(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled {
            // 故障转移开启：仅按队列顺序依次尝试（P1 → P2 → ...）
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致
            let ordered_ids: Vec<String> = self
                .db
                .get_failover_queue(app_type)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();

            total_providers = ordered_ids.len();

            for provider_id in ordered_ids {
                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };

                let circuit_key = format!("{app_type}:{}", provider.id);
                let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

                if breaker.is_available().await {
                    result.push(provider);
                } else {
                    circuit_open_count += 1;
                }
            }
        } else {
            // 故障转移关闭：仅使用当前供应商，跳过熔断器检查
            let current_id = AppType::from_str(app_type)
                .ok()
                .and_then(|app_enum| {
                    crate::settings::get_effective_current_provider(&self.db, &app_enum)
                        .ok()
                        .flatten()
                })
                .or_else(|| self.db.get_current_provider(app_type).ok().flatten());

            if let Some(current_id) = current_id {
                if let Some(current) = self.db.get_provider_by_id(&current_id, app_type)? {
                    total_providers = 1;
                    result.push(current);
                }
            }
        }

        if result.is_empty() {
            if total_providers > 0 && circuit_open_count == total_providers {
                log::warn!("[{app_type}] [FO-004] 所有供应商均已熔断");
                return Err(AppError::AllProvidersCircuitOpen);
            } else {
                log::warn!("[{app_type}] [FO-005] 未配置供应商");
                return Err(AppError::NoProvidersConfigured);
            }
        }

        Ok(result)
    }

    /// 请求执行前获取熔断器“放行许可”
    ///
    /// - Closed：直接放行
    /// - Open：超时到达后切到 HalfOpen 并放行一次探测
    /// - HalfOpen：按限流规则放行探测
    ///
    /// 注意：调用方必须在请求结束后通过 `record_result()` 释放 HalfOpen 名额，
    /// 否则会导致该 Provider 长时间无法进入探测状态。
    pub async fn allow_provider_request(&self, provider_id: &str, app_type: &str) -> AllowResult {
        if self.is_provider_cooled_down(provider_id, app_type).await {
            return AllowResult {
                allowed: false,
                used_half_open_permit: false,
            };
        }
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.allow_request().await
    }

    /// 记录供应商请求结果
    pub async fn record_result(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        // 1. 按应用独立获取熔断器配置
        let failure_threshold = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => app_config.circuit_failure_threshold,
            Err(_) => 5, // 默认值
        };

        // 2. 更新熔断器状态
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

        if success {
            breaker.record_success(used_half_open_permit).await;
        } else {
            breaker.record_failure(used_half_open_permit).await;
        }

        // 3. 更新数据库健康状态（使用配置的阈值）
        self.db
            .update_provider_health_with_threshold(
                provider_id,
                app_type,
                success,
                error_msg.clone(),
                failure_threshold,
            )
            .await?;

        Ok(())
    }

    /// 重置熔断器（手动恢复）
    pub async fn reset_circuit_breaker(&self, circuit_key: &str) {
        let breakers = self.circuit_breakers.read().await;
        if let Some(breaker) = breakers.get(circuit_key) {
            breaker.reset().await;
        }
    }

    /// 重置指定供应商的熔断器
    pub async fn reset_provider_breaker(&self, provider_id: &str, app_type: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.reset_circuit_breaker(&circuit_key).await;
        self.clear_provider_cooldown(provider_id, app_type).await;
    }

    /// 仅释放 HalfOpen permit，不影响健康统计（neutral 接口）
    ///
    /// 用于整流器等场景：请求结果不应计入 Provider 健康度，
    /// 但仍需释放占用的探测名额，避免 HalfOpen 状态卡死
    pub async fn release_permit_neutral(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
    ) {
        if !used_half_open_permit {
            return;
        }
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.release_half_open_permit();
    }

    /// 更新所有熔断器的配置（热更新）
    pub async fn update_all_configs(&self, config: CircuitBreakerConfig) {
        let breakers = self.circuit_breakers.read().await;
        for breaker in breakers.values() {
            breaker.update_config(config.clone()).await;
        }
    }

    /// 更新指定应用已创建熔断器的配置（热更新）
    pub async fn update_app_configs(&self, app_type: &str, config: CircuitBreakerConfig) {
        let prefix = format!("{app_type}:");
        let breakers = self.circuit_breakers.read().await;
        for (key, breaker) in breakers.iter() {
            if key.starts_with(&prefix) {
                breaker.update_config(config.clone()).await;
            }
        }
    }

    /// 获取熔断器状态
    #[allow(dead_code)]
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::circuit_breaker::CircuitBreakerStats> {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breakers = self.circuit_breakers.read().await;

        if let Some(breaker) = breakers.get(&circuit_key) {
            Some(breaker.get_stats().await)
        } else {
            None
        }
    }

    /// 获取或创建熔断器
    async fn get_or_create_circuit_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // 先尝试读锁获取
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(breaker) = breakers.get(key) {
                return breaker.clone();
            }
        }

        // 如果不存在，获取写锁创建
        let mut breakers = self.circuit_breakers.write().await;

        // 双重检查，防止竞争条件
        if let Some(breaker) = breakers.get(key) {
            return breaker.clone();
        }

        // 从 key 中提取 app_type (格式: "app_type:provider_id")
        let app_type = key.split(':').next().unwrap_or("claude");

        // 按应用独立读取熔断器配置
        let config = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => crate::proxy::circuit_breaker::CircuitBreakerConfig {
                failure_threshold: app_config.circuit_failure_threshold,
                success_threshold: app_config.circuit_success_threshold,
                timeout_seconds: app_config.circuit_timeout_seconds as u64,
                error_rate_threshold: app_config.circuit_error_rate_threshold,
                min_requests: app_config.circuit_min_requests,
            },
            Err(_) => crate::proxy::circuit_breaker::CircuitBreakerConfig::default(),
        };

        let breaker = Arc::new(CircuitBreaker::new(config));
        breakers.insert(key.to_string(), breaker.clone());

        breaker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("LLM_GATEWAY_TEST_HOME").ok();

            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("LLM_GATEWAY_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");

            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("LLM_GATEWAY_TEST_HOME", value),
                None => env::remove_var("LLM_GATEWAY_TEST_HOME"),
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_router_creation() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        let breaker = router.get_or_create_circuit_breaker("claude:test").await;
        assert!(breaker.allow_request().await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_rate_limit_cooldown_blocks_admission_until_cleared() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        router
            .cooldown_provider("p1", "claude", Duration::from_secs(30))
            .await;
        assert!(router.is_provider_cooled_down("p1", "claude").await);
        assert!(!router.allow_provider_request("p1", "claude").await.allowed);

        router.clear_provider_cooldown("p1", "claude").await;
        assert!(!router.is_provider_cooled_down("p1", "claude").await);
        assert!(router.allow_provider_request("p1", "claude").await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_route_rejection_is_scoped_to_model_and_request_profile() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        router
            .reject_route_candidate(
                "p1::chat",
                "codex",
                "agent-model",
                "tools-and-reasoning",
                Duration::from_secs(30),
            )
            .await;

        assert!(
            router
                .is_route_candidate_rejected(
                    "p1::chat",
                    "codex",
                    "agent-model",
                    "tools-and-reasoning",
                )
                .await
        );
        assert!(
            !router
                .is_route_candidate_rejected(
                    "p1::chat",
                    "codex",
                    "agent-model",
                    "plain-text",
                )
                .await
        );
        assert!(
            !router
                .is_route_candidate_rejected(
                    "p1::chat",
                    "codex",
                    "another-model",
                    "tools-and-reasoning",
                )
                .await
        );

        router
            .clear_route_candidate_rejection(
                "p1::chat",
                "codex",
                "agent-model",
                "tools-and-reasoning",
            )
            .await;
        assert!(
            !router
                .is_route_candidate_rejected(
                    "p1::chat",
                    "codex",
                    "agent-model",
                    "tools-and-reasoning",
                )
                .await
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_round_robin_rotates_alias_chain_and_priority_stays_stable() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        let providers = vec![
            Provider::with_id("a".to_string(), "A".to_string(), json!({}), None),
            Provider::with_id("b".to_string(), "B".to_string(), json!({}), None),
            Provider::with_id("c".to_string(), "C".to_string(), json!({}), None),
        ];
        let weights = HashMap::new();
        let active = HashMap::new();

        let priority = router
            .apply_gateway_routing_policy(
                "codex",
                "best-code",
                GatewayRoutingPolicy::Priority,
                &weights,
                &active,
                providers.clone(),
            )
            .await;
        assert_eq!(priority.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);

        let first = router
            .apply_gateway_routing_policy(
                "codex",
                "best-code",
                GatewayRoutingPolicy::RoundRobin,
                &weights,
                &active,
                providers.clone(),
            )
            .await;
        let second = router
            .apply_gateway_routing_policy(
                "codex",
                "best-code",
                GatewayRoutingPolicy::RoundRobin,
                &weights,
                &active,
                providers.clone(),
            )
            .await;
        let third = router
            .apply_gateway_routing_policy(
                "codex",
                "best-code",
                GatewayRoutingPolicy::RoundRobin,
                &weights,
                &active,
                providers,
            )
            .await;

        assert_eq!(first[0].id, "a");
        assert_eq!(second[0].id, "b");
        assert_eq!(third[0].id, "c");
    }

    #[tokio::test]
    #[serial]
    async fn test_round_robin_tracks_candidate_identity_across_reload() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        let provider = |id: &str| {
            Provider::with_id(id.to_string(), id.to_uppercase(), json!({}), None)
        };
        let weights = HashMap::new();
        let active = HashMap::new();

        let first = router
            .apply_gateway_routing_policy(
                "codex",
                "changing-chain",
                GatewayRoutingPolicy::RoundRobin,
                &weights,
                &active,
                vec![provider("a"), provider("b"), provider("c")],
            )
            .await;
        assert_eq!(first[0].id, "a");

        // Removing a candidate before the former numeric cursor must not skip b.
        let after_reload = router
            .apply_gateway_routing_policy(
                "codex",
                "changing-chain",
                GatewayRoutingPolicy::RoundRobin,
                &weights,
                &active,
                vec![provider("a"), provider("b")],
            )
            .await;
        assert_eq!(after_reload[0].id, "b");

        // If the remembered identity disappears, start from the current first
        // candidate rather than applying a stale index to the new vector.
        let after_second_reload = router
            .apply_gateway_routing_policy(
                "codex",
                "changing-chain",
                GatewayRoutingPolicy::RoundRobin,
                &weights,
                &active,
                vec![provider("c"), provider("a")],
            )
            .await;
        assert_eq!(after_second_reload[0].id, "c");
    }

    #[tokio::test]
    #[serial]
    async fn test_weighted_round_robin_uses_smooth_5_3_2_distribution() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        let providers = vec![
            Provider::with_id("a".to_string(), "A".to_string(), json!({}), None),
            Provider::with_id("b".to_string(), "B".to_string(), json!({}), None),
            Provider::with_id("c".to_string(), "C".to_string(), json!({}), None),
        ];
        let weights = HashMap::from([
            ("a".to_string(), 5),
            ("b".to_string(), 3),
            ("c".to_string(), 2),
        ]);
        let active = HashMap::new();
        let mut counts: HashMap<String, usize> = HashMap::new();

        for _ in 0..10 {
            let ordered = router
                .apply_gateway_routing_policy(
                    "codex",
                    "best-code",
                    GatewayRoutingPolicy::WeightedRoundRobin,
                    &weights,
                    &active,
                    providers.clone(),
                )
                .await;
            *counts.entry(ordered[0].id.clone()).or_insert(0) += 1;
        }

        assert_eq!(counts.get("a").copied(), Some(5));
        assert_eq!(counts.get("b").copied(), Some(3));
        assert_eq!(counts.get("c").copied(), Some(2));
    }

    #[tokio::test]
    #[serial]
    async fn test_least_outstanding_prefers_lowest_source_provider_load() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        let providers = vec![
            Provider::with_id("a".to_string(), "A".to_string(), json!({}), None),
            Provider::with_id("b".to_string(), "B".to_string(), json!({}), None),
            Provider::with_id("c".to_string(), "C".to_string(), json!({}), None),
        ];
        let active = HashMap::from([
            (("codex".to_string(), "a".to_string()), (3usize, "A".to_string())),
            (("claude".to_string(), "b".to_string()), (1usize, "B".to_string())),
            (("codex".to_string(), "c".to_string()), (2usize, "C".to_string())),
        ]);
        let ordered = router
            .apply_gateway_routing_policy(
                "codex",
                "best-code",
                GatewayRoutingPolicy::LeastOutstanding,
                &HashMap::new(),
                &active,
                providers,
            )
            .await;
        assert_eq!(ordered[0].id, "b");
        assert_eq!(ordered[1].id, "c");
        assert_eq!(ordered[2].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_capacity_waits_and_releases_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = Arc::new(ProviderRouter::new(db));
        let mut provider = Provider::with_id(
            "p1::responses".to_string(),
            "P1".to_string(),
            json!({}),
            None,
        );
        provider.meta = Some(crate::provider::ProviderMeta {
            gateway_source_provider_id: Some("p1".to_string()),
            gateway_max_concurrent_requests: Some(1),
            gateway_queue_limit: Some(1),
            gateway_queue_timeout_ms: Some(1_000),
            ..Default::default()
        });

        let first = router
            .try_acquire_provider_capacity(&provider)
            .unwrap()
            .expect("limited provider should return a permit");
        assert_eq!(
            router.try_acquire_provider_capacity(&provider).unwrap_err(),
            CapacityAcquireError::Full
        );

        let wait_router = router.clone();
        let wait_provider = provider.clone();
        let waiter = tokio::spawn(async move {
            wait_router.wait_acquire_provider_capacity(&wait_provider).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(router.provider_capacity_snapshot("p1"), (1, 1, 1));

        drop(first);
        let second = waiter
            .await
            .unwrap()
            .unwrap()
            .expect("queued request should receive the released slot");
        assert_eq!(router.provider_capacity_snapshot("p1"), (1, 1, 0));
        drop(second);
        assert_eq!(router.provider_capacity_snapshot("p1"), (1, 0, 0));
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_capacity_cancelled_waiter_does_not_leak_queue_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = Arc::new(ProviderRouter::new(db));
        let mut provider = Provider::with_id(
            "p1::chat".to_string(),
            "P1".to_string(),
            json!({}),
            None,
        );
        provider.meta = Some(crate::provider::ProviderMeta {
            gateway_source_provider_id: Some("p1".to_string()),
            gateway_max_concurrent_requests: Some(1),
            gateway_queue_limit: Some(1),
            gateway_queue_timeout_ms: Some(5_000),
            ..Default::default()
        });

        let first = router
            .try_acquire_provider_capacity(&provider)
            .unwrap()
            .expect("limited provider should return a permit");
        let wait_router = router.clone();
        let wait_provider = provider.clone();
        let waiter = tokio::spawn(async move {
            wait_router.wait_acquire_provider_capacity(&wait_provider).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(router.provider_capacity_snapshot("p1"), (1, 1, 1));

        waiter.abort();
        let _ = waiter.await;
        assert_eq!(router.provider_capacity_snapshot("p1"), (1, 1, 0));
        drop(first);
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_disabled_uses_current_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_order_ignoring_current() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 设置 sort_index 来控制顺序：b=1, a=2
        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(2);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        db.add_to_failover_queue("claude", "b").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 2);
        // 故障转移开启时：仅按队列顺序选择（忽略当前供应商）
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[1].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_only_even_if_current_not_in_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        // 只把 b 加入故障转移队列（模拟“当前供应商不在队列里”的常见配置）
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
    }

    #[tokio::test]
    #[serial]
    async fn test_select_providers_does_not_consume_half_open_permit() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();

        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        let providers = router.select_providers("claude").await.unwrap();
        assert_eq!(providers.len(), 2);

        assert!(router.allow_provider_request("b", "claude").await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_release_permit_neutral_frees_half_open_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 配置熔断器：1 次失败即熔断，0 秒超时立即进入 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 触发熔断：1 次失败
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 第一次请求：获取 HalfOpen 探测名额
        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed);
        assert!(first.used_half_open_permit);

        // 第二次请求应被拒绝（名额已被占用）
        let second = router.allow_provider_request("a", "claude").await;
        assert!(!second.allowed);

        // 使用 release_permit_neutral 释放名额（不影响健康统计）
        router
            .release_permit_neutral("a", "claude", first.used_half_open_permit)
            .await;

        // 第三次请求应被允许（名额已释放）
        let third = router.allow_provider_request("a", "claude").await;
        assert!(third.allowed);
        assert!(third.used_half_open_permit);
    }
}
