//! 会话亲和（Session Affinity）
//!
//! 把一段对话绑定到固定的上游 provider，让同一会话的后续请求继续落到同一家：
//! prompt / KV cache 命中率显著提升，不同上游对同一段历史处理方式的差异也不会再造成行为抖动。
//!
//! 语义移植自 CPA 的会话亲和选择器（`sdk/cliproxy/auth/selector.go`），
//! 但只保留我们需要的部分——我们的模型是「一行 provider = 一个凭证」，没有凭证池层级，
//! 所以没有 auth / model 两级调度，只做「会话 → provider」一层映射。
//!
//! ## 语义要点
//!
//! - **绑定优先于配置顺序**：命中绑定时先试被绑定的 provider；它不可用则回退重选并重新绑定。
//!   这不计入「故障转移」统计——亲和是偏好层，不是故障转移。
//! - **`get` 不续期**：查询本身延长绑定寿命的话，一个反复打开陈旧标签页的客户端
//!   就能让早已不该存在的绑定一直活下去。绑定寿命只由成功请求延长。
//! - **成功时用 `bind` 而不是「仅续期」**：故障转移后会话必须能改绑到新的健康上游，
//!   「仅当仍指向同一家才续期」的操作在这里会变成空操作，会话再也绑不回去。
//!   这与 CPA `Touch`（「刷新已有序列，或在它是新扩展时绑定」）在单层模型下的行为一致。
//! - **`compare_and_delete`**：失败时仅当当前映射仍指向那个 provider 才解除，
//!   避免删掉另一个请求刚重新建立的绑定。
//! - **容量有界**：写入时先清过期项，仍然满则淘汰最久未访问的一条。
//!
//! 所有方法都是同步的（`&mut self`），由持有方决定用哪种锁包起来；
//! 这样这个模块可以脱离 tokio 单独做单元测试。参见 `ProxyState::session_affinity`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 默认绑定 TTL（1 小时，与 CPA `session-affinity-ttl` 默认值一致）
pub const DEFAULT_AFFINITY_TTL: Duration = Duration::from_secs(60 * 60);

/// 默认容量上限
///
/// CPA 用 65,536 条。桌面本地网关的并发会话量远低于控制面，4,096 条足够，
/// 也让「写满才触发」的线性淘汰扫描保持在微秒级。
pub const DEFAULT_AFFINITY_CAPACITY: usize = 4096;

/// 一条会话绑定
#[derive(Debug, Clone)]
struct Binding {
    provider_id: String,
    /// 过期时刻
    expires_at: Instant,
    /// 最近一次绑定 / 续期（仅用于容量淘汰的取舍）
    last_access: Instant,
}

impl Binding {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at <= now
    }

    fn refresh(&mut self, now: Instant, ttl: Duration) {
        self.expires_at = now + ttl;
        self.last_access = now;
    }
}

/// 会话 → provider 绑定表
///
/// 见模块文档。[`SessionAffinityStore::get`] 只读不续期，
/// 寿命由 [`SessionAffinityStore::bind`]（成功转发时调用）延长。
#[derive(Debug)]
pub struct SessionAffinityStore {
    bindings: HashMap<String, Binding>,
    ttl: Duration,
    capacity: usize,
}

impl Default for SessionAffinityStore {
    fn default() -> Self {
        Self::new(DEFAULT_AFFINITY_TTL, DEFAULT_AFFINITY_CAPACITY)
    }
}

impl SessionAffinityStore {
    /// 创建绑定表；`capacity` 为 0 时按 1 处理，避免永久拒绝写入
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            bindings: HashMap::new(),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// 查询绑定，**不**续期
    ///
    /// 过期项会被就地丢弃并视为未命中。
    pub fn get(&mut self, session_id: &str) -> Option<String> {
        self.get_at(session_id, Instant::now())
    }

    /// 建立 / 覆盖绑定，并刷新 TTL
    ///
    /// 成功转发后调用。同时承担「首次建立」「故障转移后改绑」「续期」三种职责。
    pub fn bind(&mut self, session_id: &str, provider_id: &str) {
        self.bind_at(session_id, provider_id, Instant::now());
    }

    /// 失败时调用：仅当映射仍指向该 provider 才解除绑定
    ///
    /// 返回是否真的删除了。请求级错误（用户输入导致的 400 等）不应该走这里——
    /// 那类错误换 provider 也没用，不该打断亲和。
    pub fn compare_and_delete(&mut self, session_id: &str, provider_id: &str) -> bool {
        let matched = self
            .bindings
            .get(session_id)
            .map(|binding| binding.provider_id == provider_id)
            .unwrap_or(false);

        if matched {
            self.bindings.remove(session_id);
        }

        matched
    }

    /// 绑定条数（含尚未被清理的过期项）
    ///
    /// 只给测试用：生产代码从不读它，放出来会让 release 构建多一个永远为 0 的调用点。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// 是否为空（与 [`Self::len`] 一样，仅测试内省用）
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    // ------------------------------------------------------------------
    // 显式时钟版本：公开方法只是 `Instant::now()` 的薄封装，测试用这些
    // 函数直接推进逻辑时间，避免 sleep 带来的不确定性。
    // ------------------------------------------------------------------

    fn get_at(&mut self, session_id: &str, now: Instant) -> Option<String> {
        let expired = self
            .bindings
            .get(session_id)
            .map(|binding| binding.is_expired(now))
            .unwrap_or(false);

        if expired {
            self.bindings.remove(session_id);
        }

        self.bindings
            .get(session_id)
            .map(|binding| binding.provider_id.clone())
    }

    fn bind_at(&mut self, session_id: &str, provider_id: &str, now: Instant) {
        // 新键且已满 —— 先腾位置，避免表无限增长
        if !self.bindings.contains_key(session_id) && self.bindings.len() >= self.capacity {
            self.make_room_at(now);
        }

        let ttl = self.ttl;
        self.bindings
            .entry(session_id.to_string())
            .and_modify(|binding| {
                binding.provider_id = provider_id.to_string();
                binding.refresh(now, ttl);
            })
            .or_insert_with(|| {
                let mut binding = Binding {
                    provider_id: provider_id.to_string(),
                    expires_at: now,
                    last_access: now,
                };
                binding.refresh(now, ttl);
                binding
            });
    }

    fn sweep_expired_at(&mut self, now: Instant) -> usize {
        let before = self.bindings.len();
        self.bindings.retain(|_, binding| !binding.is_expired(now));
        before - self.bindings.len()
    }

    /// 腾出一个位置：先清过期项，仍然满就淘汰最久未访问的一条
    fn make_room_at(&mut self, now: Instant) {
        if self.sweep_expired_at(now) > 0 {
            return;
        }

        let oldest = self
            .bindings
            .iter()
            .min_by_key(|(_session_id, binding)| binding.last_access)
            .map(|(session_id, _)| session_id.clone());

        if let Some(session_id) = oldest {
            self.bindings.remove(&session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(3600);

    fn store() -> SessionAffinityStore {
        SessionAffinityStore::new(TTL, 8)
    }

    #[test]
    fn test_bind_then_get_returns_provider() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);

        assert_eq!(store.get_at("s1", now).as_deref(), Some("provider-a"));
    }

    #[test]
    fn test_get_misses_without_binding() {
        let mut store = store();

        assert_eq!(store.get("nope"), None);
        assert!(store.is_empty());
    }

    #[test]
    fn test_get_does_not_refresh_ttl() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        // 到期前一刻查询：命中，但不该续期
        assert!(store
            .get_at("s1", now + TTL - Duration::from_secs(1))
            .is_some());
        // 原本的到期时刻之后：必须已经过期
        assert_eq!(store.get_at("s1", now + TTL + Duration::from_secs(1)), None);
        // 过期项在查询时被就地丢弃
        assert!(store.is_empty());
    }

    #[test]
    fn test_bind_refreshes_ttl() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        let later = now + TTL - Duration::from_secs(1);
        store.bind_at("s1", "provider-a", later);

        // 从续期时刻起重新计时，所以原到期时刻之后仍然有效
        assert!(store
            .get_at("s1", now + TTL + Duration::from_secs(1))
            .is_some());
        // 但超过续期后的完整 TTL 就过期了
        assert_eq!(
            store.get_at("s1", later + TTL + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn test_compare_and_delete_only_removes_matching() {
        let mut store = store();

        store.bind("s1", "provider-a");

        // 已被别的请求改绑到 b —— 此时用 a 去删必须失败，否则会误删新绑定
        assert!(!store.compare_and_delete("s1", "provider-b"));
        assert_eq!(store.get("s1").as_deref(), Some("provider-a"));

        assert!(store.compare_and_delete("s1", "provider-a"));
        assert_eq!(store.get("s1"), None);
    }

    #[test]
    fn test_compare_and_delete_unknown_session() {
        let mut store = store();

        assert!(!store.compare_and_delete("never-bound", "provider-a"));
    }

    #[test]
    fn test_rebind_overwrites_provider() {
        let mut store = store();

        store.bind("s1", "provider-a");
        store.bind("s1", "provider-b");

        assert_eq!(store.get("s1").as_deref(), Some("provider-b"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_capacity_evicts_least_recently_accessed() {
        let mut store = SessionAffinityStore::new(TTL, 2);
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        store.bind_at("s2", "provider-b", now + Duration::from_secs(1));
        // 重新绑定 s1 使其成为最近访问的一条 —— 只有 bind 会更新 last_access，
        // 普通的 `get` 不会，所以这里用 bind_at 来表达「s1 更活跃」
        store.bind_at("s1", "provider-a", now + Duration::from_secs(2));

        store.bind_at("s3", "provider-c", now + Duration::from_secs(3));

        // s2 是最久未访问的，被淘汰
        assert_eq!(store.get_at("s2", now + Duration::from_secs(3)), None);
        assert_eq!(
            store.get_at("s1", now + Duration::from_secs(3)).as_deref(),
            Some("provider-a")
        );
        assert_eq!(
            store.get_at("s3", now + Duration::from_secs(3)).as_deref(),
            Some("provider-c")
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_capacity_clears_expired_first() {
        let mut store = SessionAffinityStore::new(TTL, 2);
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        store.bind_at("s2", "provider-b", now + Duration::from_secs(1));

        // 两个都过期了再写第三条：应该清过期而不是淘汰活跃项
        let much_later = now + TTL + Duration::from_secs(10);
        store.bind_at("s3", "provider-c", much_later);

        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get_at("s3", much_later).as_deref(),
            Some("provider-c")
        );
    }

    #[test]
    fn test_rebinding_existing_key_does_not_trigger_eviction() {
        let mut store = SessionAffinityStore::new(TTL, 1);

        store.bind("s1", "provider-a");
        store.bind("s1", "provider-b");

        // 容量为 1：若改绑先去「腾位置」，被淘汰的会是 s1 自己，改绑就白做了
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("s1").as_deref(), Some("provider-b"));
    }

    #[test]
    fn test_zero_capacity_is_clamped_to_one() {
        let mut store = SessionAffinityStore::new(TTL, 0);

        store.bind("s1", "provider-a");

        assert_eq!(store.get("s1").as_deref(), Some("provider-a"));
    }
}
