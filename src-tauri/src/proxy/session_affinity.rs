//! 会话亲和（Session Affinity）
//!
//! 把一段对话绑定到固定的上游 provider，让同一会话的后续请求继续落到同一家：
//! prompt / KV cache 命中率显著提升，不同上游对同一段历史的处理差异也不会再造成行为抖动。
//!
//! 语义移植自 CPA 的 `SessionAffinitySelector`（`sdk/cliproxy/auth/selector.go`），
//! 但只保留我们需要的部分——我们的模型是「一行 provider = 一个凭证」，没有凭证池层级，
//! 所以没有 auth / model 两级调度，只做「会话 → provider」一层映射。
//!
//! ## 语义要点
//!
//! - **绑定优先于配置顺序**：命中绑定时先试被绑定的 provider；它不可用则回退重选并重新绑定。
//!   这不计入「故障转移」统计——亲和是偏好层，不是故障转移。
//! - **`get` 不续期**：只有 [`SessionAffinityStore::touch`] / `get_and_refresh` 才刷新 TTL。
//!   否则打开一个陈旧标签页就能让早已过期的绑定永远活下去。
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
    /// 最近一次续期 / 访问（仅用于容量淘汰的取舍）
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
/// 续期要靠 [`SessionAffinityStore::touch`]（成功时调用）或
/// [`SessionAffinityStore::get_and_refresh`]。
#[derive(Debug)]
pub struct SessionAffinityStore {
    bindings: HashMap<String, Binding>,
    ttl: Duration,
    capacity: usize,
    hits: u64,
    misses: u64,
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
            hits: 0,
            misses: 0,
        }
    }

    /// 查询绑定，**不**续期
    ///
    /// 过期项会被就地丢弃并视为未命中。
    pub fn get(&mut self, session_id: &str) -> Option<String> {
        self.get_at(session_id, Instant::now())
    }

    /// 查询绑定并续期（命中后延长 TTL）
    pub fn get_and_refresh(&mut self, session_id: &str) -> Option<String> {
        self.get_and_refresh_at(session_id, Instant::now())
    }

    /// 建立 / 覆盖绑定
    pub fn bind(&mut self, session_id: &str, provider_id: &str) {
        self.bind_at(session_id, provider_id, Instant::now());
    }

    /// 成功转发后调用：仅当映射仍指向该 provider 时续期
    ///
    /// 返回是否续期成功。`false` 说明这条绑定已经被别的请求改写或已过期，
    /// 此时不应该把这次成功当成「亲和命中」记账。
    pub fn touch(&mut self, session_id: &str, provider_id: &str) -> bool {
        self.touch_at(session_id, provider_id, Instant::now())
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

    /// 显式解除绑定（忽略当前指向哪一家）
    pub fn remove(&mut self, session_id: &str) -> Option<String> {
        self.bindings.remove(session_id).map(|b| b.provider_id)
    }

    /// 清空
    pub fn clear(&mut self) {
        self.bindings.clear();
    }

    /// 当前绑定条数（含尚未被清理的过期项）
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// 命中次数（`get` / `get_and_refresh` 命中即计）
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// 未命中次数
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// 清掉所有已过期绑定，返回清理条数
    pub fn sweep_expired(&mut self) -> usize {
        self.sweep_expired_at(Instant::now())
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

        let found = self
            .bindings
            .get(session_id)
            .map(|binding| binding.provider_id.clone());

        match found {
            Some(provider_id) => {
                self.hits += 1;
                Some(provider_id)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    fn get_and_refresh_at(&mut self, session_id: &str, now: Instant) -> Option<String> {
        let ttl = self.ttl;

        let expired = self
            .bindings
            .get(session_id)
            .map(|binding| binding.is_expired(now))
            .unwrap_or(false);

        if expired {
            self.bindings.remove(session_id);
        }

        let found = match self.bindings.get_mut(session_id) {
            Some(binding) => {
                binding.refresh(now, ttl);
                Some(binding.provider_id.clone())
            }
            None => None,
        };

        match found {
            Some(provider_id) => {
                self.hits += 1;
                Some(provider_id)
            }
            None => {
                self.misses += 1;
                None
            }
        }
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

    fn touch_at(&mut self, session_id: &str, provider_id: &str, now: Instant) -> bool {
        let ttl = self.ttl;

        match self.bindings.get_mut(session_id) {
            Some(binding) if binding.provider_id == provider_id && !binding.is_expired(now) => {
                binding.refresh(now, ttl);
                true
            }
            _ => false,
        }
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
        assert_eq!(store.hits(), 0);
        assert_eq!(store.misses(), 1);
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
        assert!(store.is_empty());
    }

    #[test]
    fn test_get_and_refresh_extends_ttl() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        let later = now + TTL - Duration::from_secs(1);
        assert!(store.get_and_refresh_at("s1", later).is_some());

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
    fn test_touch_only_refreshes_matching_provider() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);

        assert!(!store.touch_at("s1", "provider-b", now));
        // 不匹配的 touch 不该续期
        assert_eq!(store.get_at("s1", now + TTL + Duration::from_secs(1)), None);
    }

    #[test]
    fn test_touch_refreshes_ttl() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        let later = now + TTL - Duration::from_secs(1);
        assert!(store.touch_at("s1", "provider-a", later));
        assert!(store
            .get_at("s1", now + TTL + Duration::from_secs(1))
            .is_some());
    }

    #[test]
    fn test_touch_ignores_expired_binding() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        assert!(!store.touch_at("s1", "provider-a", now + TTL + Duration::from_secs(1)));
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
        // 续期 s1 使其成为最近访问的一条 —— 注意只有 refresh 会更新 last_access，
        // 普通的 `get` 不会，所以这里必须用 get_and_refresh_at 来表达「s1 更活跃」
        assert!(store
            .get_and_refresh_at("s1", now + Duration::from_secs(2))
            .is_some());

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

        assert_eq!(store.len(), 1);
        assert_eq!(store.get("s1").as_deref(), Some("provider-b"));
    }

    #[test]
    fn test_zero_capacity_is_clamped_to_one() {
        let mut store = SessionAffinityStore::new(TTL, 0);

        store.bind("s1", "provider-a");

        assert_eq!(store.get("s1").as_deref(), Some("provider-a"));
    }

    #[test]
    fn test_sweep_expired_drops_old_entries() {
        let mut store = store();
        let now = Instant::now();

        store.bind_at("s1", "provider-a", now);
        store.bind_at("s2", "provider-b", now + TTL);

        assert_eq!(
            store.sweep_expired_at(now + TTL + Duration::from_secs(1)),
            1
        );
        assert_eq!(store.len(), 1);
        assert_eq!(
            store
                .get_at("s2", now + TTL + Duration::from_secs(1))
                .as_deref(),
            Some("provider-b")
        );
    }

    #[test]
    fn test_remove_returns_previous_provider() {
        let mut store = store();

        store.bind("s1", "provider-a");

        assert_eq!(store.remove("s1").as_deref(), Some("provider-a"));
        assert_eq!(store.remove("s1"), None);
    }

    #[test]
    fn test_hits_and_misses_are_counted() {
        let mut store = store();

        store.bind("s1", "provider-a");
        let _ = store.get("s1");
        let _ = store.get("missing");

        assert_eq!(store.hits(), 1);
        assert_eq!(store.misses(), 1);
    }
}
