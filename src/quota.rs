//! Per-API-key token usage quota enforcement.
//!
//! Tracks daily and monthly token usage per API key in memory,
//! with baselines derived from the requests table on startup and
//! on day/month rollover. No separate persistence needed — the
//! requests table (written per-request) is the source of truth.

use chrono::{Datelike, Local, NaiveDate};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{ApiKeyConfig, QuotaConfig};
#[cfg(feature = "db")]
use crate::database::Database;
use crate::metrics::TokenCounts;

/// Which quota period was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitType {
    Daily,
    Monthly,
}

impl std::fmt::Display for LimitType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Daily => f.write_str("daily"),
            Self::Monthly => f.write_str("monthly"),
        }
    }
}

/// Result of a quota check.
#[derive(Debug, Clone)]
pub enum QuotaCheckResult {
    /// Request is allowed to proceed.
    Allowed {
        daily_remaining: Option<u64>,
        monthly_remaining: Option<u64>,
        daily_limit: Option<u64>,
        monthly_limit: Option<u64>,
        daily_reset: i64,
        monthly_reset: i64,
    },
    /// Quota exceeded — request should be rejected with 429.
    Exceeded {
        retry_after_secs: u64,
        limit_type: LimitType,
    },
}

/// Resolved limits for a specific API key (merged from per-key and global defaults).
#[derive(Debug, Clone)]
struct ResolvedLimits {
    daily: Option<u64>,
    monthly: Option<u64>,
}

/// Token usage for a single time period.
#[derive(Debug, Clone, Copy)]
struct PeriodUsage {
    total_tokens: u64,
    period_start: NaiveDate,
}

/// Per-key usage accumulator (daily + monthly).
#[derive(Debug, Clone)]
struct KeyUsage {
    daily: PeriodUsage,
    monthly: PeriodUsage,
}

/// Manages per-API-key token quotas.
#[derive(Debug, Clone)]
pub struct QuotaManager {
    inner: Arc<QuotaManagerInner>,
}

#[derive(Debug)]
struct QuotaManagerInner {
    usage: RwLock<HashMap<String, KeyUsage>>,
    limits: HashMap<String, ResolvedLimits>,
    global_daily: Option<u64>,
    global_monthly: Option<u64>,
    #[cfg(feature = "db")]
    database: Option<Database>,
}

/// Compute a short hash of an API key for storage/lookup.
pub fn hash_api_key(api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    let result = hasher.finalize();
    // Take first 8 bytes → 16 hex chars
    result[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Resolve a per-key limit against the global default.
/// - `Some(0)` = explicitly unlimited (overrides global)
/// - `Some(n)` = use per-key limit
/// - `None` = inherit global default
fn resolve_limit(per_key: Option<u64>, global: Option<u64>) -> Option<u64> {
    match per_key {
        Some(0) => None,    // explicitly unlimited
        Some(n) => Some(n), // per-key override
        None => global,     // inherit global
    }
}

/// Build the per-key limits map from config (shared between feature-gated constructors).
fn build_limits(
    api_keys: &[ApiKeyConfig],
    quotas: &QuotaConfig,
) -> HashMap<String, ResolvedLimits> {
    api_keys
        .iter()
        .map(|key_config| {
            let key_hash = hash_api_key(&key_config.key);
            let limits = ResolvedLimits {
                daily: resolve_limit(key_config.daily_token_limit, quotas.daily_token_limit),
                monthly: resolve_limit(key_config.monthly_token_limit, quotas.monthly_token_limit),
            };
            (key_hash, limits)
        })
        .collect()
}

impl QuotaManager {
    /// Create a new QuotaManager from configuration.
    /// Per-key limits of `0` mean explicitly unlimited (overrides global default).
    #[cfg(feature = "db")]
    pub fn new(
        api_keys: &[ApiKeyConfig],
        quotas: &QuotaConfig,
        database: Option<Database>,
    ) -> Self {
        Self {
            inner: Arc::new(QuotaManagerInner {
                usage: RwLock::new(HashMap::new()),
                limits: build_limits(api_keys, quotas),
                global_daily: quotas.daily_token_limit,
                global_monthly: quotas.monthly_token_limit,
                database,
            }),
        }
    }

    /// Create a new QuotaManager (without database persistence).
    #[cfg(not(feature = "db"))]
    pub fn new(api_keys: &[ApiKeyConfig], quotas: &QuotaConfig) -> Self {
        Self {
            inner: Arc::new(QuotaManagerInner {
                usage: RwLock::new(HashMap::new()),
                limits: build_limits(api_keys, quotas),
                global_daily: quotas.daily_token_limit,
                global_monthly: quotas.monthly_token_limit,
            }),
        }
    }

    /// Check whether the given API key is within quota limits.
    pub async fn check_quota(&self, api_key: &str) -> QuotaCheckResult {
        self.check_quota_hashed(&hash_api_key(api_key)).await
    }

    /// Check quota using a pre-computed key hash (avoids redundant SHA-256).
    pub async fn check_quota_hashed(&self, key_hash: &str) -> QuotaCheckResult {
        let today = Local::now().date_naive();
        let this_month_start = start_of_month(today);

        let limits = self
            .inner
            .limits
            .get(key_hash)
            .cloned()
            .unwrap_or(ResolvedLimits {
                daily: self.inner.global_daily,
                monthly: self.inner.global_monthly,
            });

        // Fast path: try read lock first — avoids write contention on every request
        {
            let usage_map = self.inner.usage.read().await;
            if let Some(usage) = usage_map.get(key_hash) {
                // If periods are current, we can check without a write lock
                if usage.daily.period_start == today
                    && usage.monthly.period_start == this_month_start
                {
                    return Self::evaluate_limits(&limits, usage);
                }
            }
        }

        // Slow path: pre-fetch DB baseline (lock-free) before acquiring the
        // write lock, mirroring `record_usage_hashed`. See its docs for the
        // anti-pattern this avoids.
        #[cfg(feature = "db")]
        let prefetched_baseline = self
            .prefetch_baseline_if_rollover(key_hash, today, this_month_start)
            .await;

        // Snapshot the post-reset usage under the write lock, then evaluate
        // limits lock-free. Cloning ~40 bytes (KeyUsage = 2 × PeriodUsage) is
        // cheaper than holding a write lock across the comparison logic.
        let snapshot = {
            let mut usage_map = self.inner.usage.write().await;
            let usage = usage_map
                .entry(key_hash.to_string())
                .or_insert_with(|| KeyUsage {
                    daily: PeriodUsage {
                        total_tokens: 0,
                        period_start: today,
                    },
                    monthly: PeriodUsage {
                        total_tokens: 0,
                        period_start: this_month_start,
                    },
                });

            #[cfg(feature = "db")]
            Self::apply_period_reset(usage, today, this_month_start, prefetched_baseline);
            #[cfg(not(feature = "db"))]
            Self::reset_periods_if_needed(usage, today, this_month_start);

            usage.clone()
        };

        Self::evaluate_limits(&limits, &snapshot)
    }

    /// Non-blocking quota check for TUI render path.
    /// Returns None only if the lock is contended.
    pub fn check_quota_sync(&self, api_key: &str) -> Option<QuotaCheckResult> {
        let key_hash = hash_api_key(api_key);
        let limits = self
            .inner
            .limits
            .get(&key_hash)
            .cloned()
            .unwrap_or(ResolvedLimits {
                daily: self.inner.global_daily,
                monthly: self.inner.global_monthly,
            });

        let usage_map = self.inner.usage.try_read().ok()?;
        let today = Local::now().date_naive();
        let this_month_start = start_of_month(today);
        let zero_usage = KeyUsage {
            daily: PeriodUsage {
                total_tokens: 0,
                period_start: today,
            },
            monthly: PeriodUsage {
                total_tokens: 0,
                period_start: this_month_start,
            },
        };
        let usage = usage_map.get(&key_hash).unwrap_or(&zero_usage);
        // If a period is stale (rolled over), show zero for that period only
        let effective_usage = KeyUsage {
            daily: if usage.daily.period_start < today {
                PeriodUsage {
                    total_tokens: 0,
                    period_start: today,
                }
            } else {
                usage.daily
            },
            monthly: if usage.monthly.period_start < this_month_start {
                PeriodUsage {
                    total_tokens: 0,
                    period_start: this_month_start,
                }
            } else {
                usage.monthly
            },
        };
        Some(Self::evaluate_limits(&limits, &effective_usage))
    }

    /// Reset usage periods if they've rolled over, applying a pre-fetched DB
    /// baseline if provided (otherwise resets the rolled-over period(s) to
    /// zero). The DB query is performed by the caller outside any write lock
    /// — see `prefetch_baseline_if_rollover`. This split keeps the global
    /// usage write lock from being held across a database round-trip.
    ///
    /// No-op if periods are already current (TOCTOU re-check after lock
    /// re-acquire — another writer may have rolled them over already).
    #[cfg(feature = "db")]
    fn apply_period_reset(
        usage: &mut KeyUsage,
        today: NaiveDate,
        this_month_start: NaiveDate,
        prefetched_baseline: Option<(u64, u64)>,
    ) {
        if usage.daily.period_start >= today && usage.monthly.period_start >= this_month_start {
            return;
        }
        match prefetched_baseline {
            Some((daily_total, monthly_total)) => {
                usage.daily.total_tokens = daily_total;
                usage.monthly.total_tokens = monthly_total;
            }
            None => {
                if usage.daily.period_start < today {
                    usage.daily.total_tokens = 0;
                }
                if usage.monthly.period_start < this_month_start {
                    usage.monthly.total_tokens = 0;
                }
            }
        }
        usage.daily.period_start = today;
        usage.monthly.period_start = this_month_start;
    }

    /// Peek under a read lock to predict whether a period rollover will be
    /// needed for `key_hash`; if so, fetch the DB baseline for the new
    /// period. The DB query happens with no usage-map lock held.
    ///
    /// Returns `None` when no rollover is predicted, the entry is missing
    /// (a fresh insert starts at the current period — no DB needed), the
    /// DB feature is unavailable, or the DB query fails. Callers fall back
    /// to a zero-reset in those cases.
    #[cfg(feature = "db")]
    async fn prefetch_baseline_if_rollover(
        &self,
        key_hash: &str,
        today: NaiveDate,
        this_month_start: NaiveDate,
    ) -> Option<(u64, u64)> {
        let needs_reset = {
            let usage_map = self.inner.usage.read().await;
            match usage_map.get(key_hash) {
                Some(u) => {
                    u.daily.period_start < today || u.monthly.period_start < this_month_start
                }
                None => false,
            }
        };
        if !needs_reset {
            return None;
        }
        let db = self.inner.database.as_ref()?;
        db.load_quota_baseline_for_key(key_hash).await.ok()
    }

    /// Reset usage periods if they've rolled over (no database — always resets to zero).
    #[cfg(not(feature = "db"))]
    fn reset_periods_if_needed(
        usage: &mut KeyUsage,
        today: NaiveDate,
        this_month_start: NaiveDate,
    ) {
        if usage.daily.period_start >= today && usage.monthly.period_start >= this_month_start {
            return;
        }
        if usage.daily.period_start < today {
            usage.daily.total_tokens = 0;
        }
        if usage.monthly.period_start < this_month_start {
            usage.monthly.total_tokens = 0;
        }
        usage.daily.period_start = today;
        usage.monthly.period_start = this_month_start;
    }

    /// Evaluate quota limits against current usage (shared logic for read/write paths).
    fn evaluate_limits(limits: &ResolvedLimits, usage: &KeyUsage) -> QuotaCheckResult {
        // Check daily limit
        if let Some(daily_limit) = limits.daily
            && usage.daily.total_tokens >= daily_limit
        {
            let retry_after = seconds_until_next_day();
            return QuotaCheckResult::Exceeded {
                retry_after_secs: retry_after,
                limit_type: LimitType::Daily,
            };
        }

        // Check monthly limit
        if let Some(monthly_limit) = limits.monthly
            && usage.monthly.total_tokens >= monthly_limit
        {
            let retry_after = seconds_until_next_month();
            return QuotaCheckResult::Exceeded {
                retry_after_secs: retry_after,
                limit_type: LimitType::Monthly,
            };
        }

        QuotaCheckResult::Allowed {
            daily_remaining: limits
                .daily
                .map(|l| l.saturating_sub(usage.daily.total_tokens)),
            monthly_remaining: limits
                .monthly
                .map(|l| l.saturating_sub(usage.monthly.total_tokens)),
            daily_limit: limits.daily,
            monthly_limit: limits.monthly,
            daily_reset: next_day_timestamp(),
            monthly_reset: next_month_timestamp(),
        }
    }

    /// Record token usage for a given API key (called after response).
    /// Counts all token categories: input + output + cache_read + cache_write.
    pub async fn record_usage(&self, api_key: &str, tokens: &TokenCounts) {
        self.record_usage_hashed(&hash_api_key(api_key), tokens)
            .await;
    }

    /// Record usage using a pre-computed key hash (avoids redundant SHA-256).
    pub async fn record_usage_hashed(&self, key_hash: &str, tokens: &TokenCounts) {
        let total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
        if total == 0 {
            return;
        }

        let today = Local::now().date_naive();
        let this_month_start = start_of_month(today);

        // Pre-fetch DB baseline outside the write lock if a period rollover is
        // likely. Holding the global usage write lock across a DB round-trip
        // would serialize every concurrent quota update behind it; the peek
        // uses a read lock and the DB query runs lock-free. The write-lock
        // section below re-checks rollover state to handle TOCTOU.
        #[cfg(feature = "db")]
        let prefetched_baseline = self
            .prefetch_baseline_if_rollover(key_hash, today, this_month_start)
            .await;

        let mut usage_map = self.inner.usage.write().await;
        let usage = usage_map
            .entry(key_hash.to_string())
            .or_insert_with(|| KeyUsage {
                daily: PeriodUsage {
                    total_tokens: 0,
                    period_start: today,
                },
                monthly: PeriodUsage {
                    total_tokens: 0,
                    period_start: this_month_start,
                },
            });

        #[cfg(feature = "db")]
        Self::apply_period_reset(usage, today, this_month_start, prefetched_baseline);
        #[cfg(not(feature = "db"))]
        Self::reset_periods_if_needed(usage, today, this_month_start);

        usage.daily.total_tokens += total;
        usage.monthly.total_tokens += total;
    }

    /// Load baseline quota usage from the requests table.
    /// Queries daily and monthly totals per api_key_hash and populates in-memory usage.
    #[cfg(feature = "db")]
    pub async fn load_baselines(&self, db: &Database) -> anyhow::Result<()> {
        let rows = db.load_quota_baselines().await?;
        let today = Local::now().date_naive();
        let this_month_start = start_of_month(today);

        let mut usage_map = self.inner.usage.write().await;

        for (key_hash, daily_tokens, monthly_tokens) in rows {
            let usage = usage_map.entry(key_hash).or_insert_with(|| KeyUsage {
                daily: PeriodUsage {
                    total_tokens: 0,
                    period_start: today,
                },
                monthly: PeriodUsage {
                    total_tokens: 0,
                    period_start: this_month_start,
                },
            });

            usage.daily.total_tokens = daily_tokens;
            usage.daily.period_start = today;
            usage.monthly.total_tokens = monthly_tokens;
            usage.monthly.period_start = this_month_start;
        }

        Ok(())
    }
}

/// Returns the first day of the month containing `date`.
pub(crate) fn start_of_month(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap()
}

/// Returns the first day of the month following `date`'s month.
fn next_month_start(date: NaiveDate) -> NaiveDate {
    if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1).unwrap()
    }
}

/// Seconds until next midnight local time.
fn seconds_until_next_day() -> u64 {
    let now = Local::now();
    let tomorrow = (now.date_naive() + chrono::Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let tomorrow_local = tomorrow.and_local_timezone(now.timezone()).unwrap();
    let diff = tomorrow_local - now;
    diff.num_seconds().max(1) as u64
}

/// Seconds until the first of next month local time.
fn seconds_until_next_month() -> u64 {
    let now = Local::now();
    let next_month = next_month_start(now.date_naive());
    let next_month_local = next_month
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(now.timezone())
        .unwrap();
    let diff = next_month_local - now;
    diff.num_seconds().max(1) as u64
}

/// Unix timestamp of next midnight local time.
fn next_day_timestamp() -> i64 {
    let now = Local::now();
    let tomorrow = (now.date_naive() + chrono::Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .unwrap();
    tomorrow
        .and_local_timezone(now.timezone())
        .unwrap()
        .timestamp()
}

/// Unix timestamp of first of next month local time.
fn next_month_timestamp() -> i64 {
    let now = Local::now();
    let next_month = next_month_start(now.date_naive());
    next_month
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(now.timezone())
        .unwrap()
        .timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(daily: Option<u64>, monthly: Option<u64>) -> (Vec<ApiKeyConfig>, QuotaConfig) {
        let keys = vec![ApiKeyConfig {
            key: "test-key".to_string(),
            daily_token_limit: None,
            monthly_token_limit: None,
            requests_per_minute: None,
        }];
        let quotas = QuotaConfig {
            enabled: true,
            daily_token_limit: daily,
            monthly_token_limit: monthly,
            ..Default::default()
        };
        (keys, quotas)
    }

    /// Helper to construct QuotaManager in tests regardless of feature flags.
    fn make_qm(keys: &[ApiKeyConfig], quotas: &QuotaConfig) -> QuotaManager {
        #[cfg(feature = "db")]
        {
            QuotaManager::new(keys, quotas, None)
        }
        #[cfg(not(feature = "db"))]
        {
            QuotaManager::new(keys, quotas)
        }
    }

    #[tokio::test]
    async fn test_quota_allowed_when_under_limit() {
        let (keys, quotas) = make_config(Some(1000), Some(10000));
        let qm = make_qm(&keys, &quotas);

        qm.record_usage(
            "test-key",
            &TokenCounts {
                input: 100,
                output: 100,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await;

        match qm.check_quota("test-key").await {
            QuotaCheckResult::Allowed {
                daily_remaining, ..
            } => {
                assert_eq!(daily_remaining, Some(800)); // 1000 - 200
            }
            QuotaCheckResult::Exceeded { .. } => panic!("Should be allowed"),
        }
    }

    #[tokio::test]
    async fn test_quota_exceeded_daily() {
        let (keys, quotas) = make_config(Some(500), None);
        let qm = make_qm(&keys, &quotas);

        qm.record_usage(
            "test-key",
            &TokenCounts {
                input: 300,
                output: 200,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await; // 500 total = at limit

        match qm.check_quota("test-key").await {
            QuotaCheckResult::Exceeded { limit_type, .. } => {
                assert_eq!(limit_type, LimitType::Daily);
            }
            QuotaCheckResult::Allowed { .. } => panic!("Should be exceeded"),
        }
    }

    #[tokio::test]
    async fn test_quota_exceeded_monthly() {
        let (keys, quotas) = make_config(None, Some(1000));
        let qm = make_qm(&keys, &quotas);

        qm.record_usage(
            "test-key",
            &TokenCounts {
                input: 600,
                output: 500,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await; // 1100 > 1000

        match qm.check_quota("test-key").await {
            QuotaCheckResult::Exceeded { limit_type, .. } => {
                assert_eq!(limit_type, LimitType::Monthly);
            }
            QuotaCheckResult::Allowed { .. } => panic!("Should be exceeded"),
        }
    }

    #[tokio::test]
    async fn test_quota_unlimited_when_no_limits() {
        let (keys, quotas) = make_config(None, None);
        let qm = make_qm(&keys, &quotas);

        qm.record_usage(
            "test-key",
            &TokenCounts {
                input: 999999,
                output: 999999,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await;

        match qm.check_quota("test-key").await {
            QuotaCheckResult::Allowed {
                daily_remaining,
                monthly_remaining,
                ..
            } => {
                assert_eq!(daily_remaining, None);
                assert_eq!(monthly_remaining, None);
            }
            QuotaCheckResult::Exceeded { .. } => panic!("Should be allowed (unlimited)"),
        }
    }

    #[tokio::test]
    async fn test_per_key_override() {
        let keys = vec![
            ApiKeyConfig {
                key: "limited-key".to_string(),
                daily_token_limit: Some(100),
                monthly_token_limit: None,
                requests_per_minute: None,
            },
            ApiKeyConfig {
                key: "unlimited-key".to_string(),
                daily_token_limit: None,
                monthly_token_limit: None,
                requests_per_minute: None,
            },
        ];
        let quotas = QuotaConfig {
            enabled: true,
            daily_token_limit: Some(1000), // global default
            monthly_token_limit: None,
            ..Default::default()
        };
        let qm = make_qm(&keys, &quotas);

        qm.record_usage(
            "limited-key",
            &TokenCounts {
                input: 50,
                output: 60,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await; // 110 > 100

        match qm.check_quota("limited-key").await {
            QuotaCheckResult::Exceeded { limit_type, .. } => {
                assert_eq!(limit_type, LimitType::Daily);
            }
            _ => panic!("limited-key should be exceeded"),
        }

        // unlimited-key uses global 1000, so 110 is fine
        qm.record_usage(
            "unlimited-key",
            &TokenCounts {
                input: 50,
                output: 60,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await;
        match qm.check_quota("unlimited-key").await {
            QuotaCheckResult::Allowed {
                daily_remaining, ..
            } => {
                assert_eq!(daily_remaining, Some(890)); // 1000 - 110
            }
            _ => panic!("unlimited-key should be allowed"),
        }
    }

    #[tokio::test]
    async fn test_cache_tokens_counted() {
        let (keys, quotas) = make_config(Some(500), None);
        let qm = make_qm(&keys, &quotas);

        // 100 input + 100 output + 200 cache_read + 100 cache_write = 500
        qm.record_usage(
            "test-key",
            &TokenCounts {
                input: 100,
                output: 100,
                cache_read: 200,
                cache_write: 100,
            },
        )
        .await;

        match qm.check_quota("test-key").await {
            QuotaCheckResult::Exceeded { limit_type, .. } => {
                assert_eq!(limit_type, LimitType::Daily);
            }
            QuotaCheckResult::Allowed { .. } => panic!("Should be exceeded (cache tokens count)"),
        }
    }

    #[tokio::test]
    async fn test_zero_means_unlimited() {
        let keys = vec![ApiKeyConfig {
            key: "admin-key".to_string(),
            daily_token_limit: Some(0),   // explicitly unlimited
            monthly_token_limit: Some(0), // explicitly unlimited
            requests_per_minute: None,
        }];
        let quotas = QuotaConfig {
            enabled: true,
            daily_token_limit: Some(100), // global limit
            monthly_token_limit: Some(1000),
            ..Default::default()
        };
        let qm = make_qm(&keys, &quotas);

        // Use way more than global limit
        qm.record_usage(
            "admin-key",
            &TokenCounts {
                input: 5000,
                output: 5000,
                cache_read: 0,
                cache_write: 0,
            },
        )
        .await;

        match qm.check_quota("admin-key").await {
            QuotaCheckResult::Allowed {
                daily_remaining,
                monthly_remaining,
                ..
            } => {
                assert_eq!(daily_remaining, None);
                assert_eq!(monthly_remaining, None);
            }
            QuotaCheckResult::Exceeded { .. } => panic!("Should be unlimited"),
        }
    }

    #[test]
    fn test_resolve_limit() {
        // 0 means unlimited regardless of global
        assert_eq!(resolve_limit(Some(0), Some(1000)), None);
        // Explicit per-key value overrides global
        assert_eq!(resolve_limit(Some(500), Some(1000)), Some(500));
        // None inherits global
        assert_eq!(resolve_limit(None, Some(1000)), Some(1000));
        // None + no global = unlimited
        assert_eq!(resolve_limit(None, None), None);
    }

    #[test]
    fn test_hash_api_key() {
        let hash = hash_api_key("test-key");
        assert_eq!(hash.len(), 16); // 8 bytes = 16 hex chars
        // Should be deterministic
        assert_eq!(hash, hash_api_key("test-key"));
        // Different keys produce different hashes
        assert_ne!(hash, hash_api_key("other-key"));
    }

    #[test]
    fn test_seconds_until_next_day() {
        let secs = seconds_until_next_day();
        assert!(secs > 0);
        assert!(secs <= 86400);
    }

    #[test]
    fn test_seconds_until_next_month() {
        let secs = seconds_until_next_month();
        assert!(secs > 0);
        assert!(secs <= 31 * 86400);
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn test_load_baselines_from_requests() {
        use crate::database::{Database, RequestRecord};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(db_path).await.unwrap();

        let (keys, quotas) = make_config(Some(10000), Some(100000));
        let qm = make_qm(&keys, &quotas);

        // Insert request rows that will form the baseline
        let key_hash = hash_api_key("test-key");
        for i in 0..3 {
            let record = RequestRecord {
                correlation_id: format!("req-{i}"),
                method: "POST".to_string(),
                path: "/v1/messages".to_string(),
                model: "claude-opus-4-7".to_string(),
                provider: "default".to_string(),
                duration_ms: 100.0,
                response_status: 200,
                streaming: false,
                input_tokens: Some(100),
                output_tokens: Some(50),
                cache_read_tokens: Some(200),
                cache_write_tokens: Some(10),
                api_key_hash: Some(key_hash.clone()),
            };
            db.insert_request(record).await.unwrap();
        }

        // Before loading, should be at full quota
        match qm.check_quota("test-key").await {
            QuotaCheckResult::Allowed {
                daily_remaining, ..
            } => {
                assert_eq!(daily_remaining, Some(10000));
            }
            _ => panic!("Should be allowed with full quota"),
        }

        // Load baselines from requests table
        qm.load_baselines(&db).await.unwrap();

        // After loading, should reflect usage from requests table
        // Each request: 100 + 50 + 200 + 10 = 360 tokens
        // 3 requests: 1080 tokens total
        match qm.check_quota("test-key").await {
            QuotaCheckResult::Allowed {
                daily_remaining, ..
            } => {
                assert_eq!(daily_remaining, Some(10000 - 1080));
            }
            _ => panic!("Should be allowed"),
        }
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn test_query_usage_with_data() {
        use crate::database::{Database, RequestRecord};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(db_path).await.unwrap();

        // Insert some test requests
        let key_hash = hash_api_key("test-key");
        for i in 0..3 {
            let record = RequestRecord {
                correlation_id: format!("req-{i}"),
                method: "POST".to_string(),
                path: "/v1/messages".to_string(),
                model: "claude-opus-4-7".to_string(),
                provider: "default".to_string(),
                duration_ms: 100.0,
                response_status: 200,
                streaming: false,
                input_tokens: Some(100),
                output_tokens: Some(50),
                cache_read_tokens: Some(200),
                cache_write_tokens: Some(10),
                api_key_hash: Some(key_hash.clone()),
            };
            db.insert_request(record).await.unwrap();
        }

        // Query by day
        let today = format!("{} 00:00:00", Local::now().date_naive());
        let rows = db
            .query_usage(Some(&key_hash), &today, crate::database::GroupBy::Day)
            .await
            .unwrap();

        assert_eq!(rows.len(), 1); // All same model + same day = 1 row
        assert_eq!(rows[0].model, "claude-opus-4-7");
        assert_eq!(rows[0].input_tokens, 300); // 3 * 100
        assert_eq!(rows[0].output_tokens, 150); // 3 * 50
        assert_eq!(rows[0].cache_read_tokens, 600); // 3 * 200
        assert_eq!(rows[0].cache_write_tokens, 30); // 3 * 10
        assert_eq!(rows[0].request_count, 3);
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn test_cleanup_old_requests() {
        use crate::database::{Database, RequestRecord};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(db_path).await.unwrap();

        // Insert a request (created_at defaults to 'now')
        let record = RequestRecord {
            correlation_id: "recent".to_string(),
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            model: "claude-opus-4-7".to_string(),
            provider: "default".to_string(),
            duration_ms: 100.0,
            response_status: 200,
            streaming: false,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_read_tokens: None,
            cache_write_tokens: None,
            api_key_hash: Some("abc123".to_string()),
        };
        db.insert_request(record).await.unwrap();

        // Cleanup with 7 days retention should NOT delete the just-inserted record
        let deleted = db.cleanup_old_requests(7).await.unwrap();
        assert_eq!(deleted, 0);

        // Verify record still exists
        let today = format!("{} 00:00:00", Local::now().date_naive());
        let rows = db
            .query_usage(None, &today, crate::database::GroupBy::Day)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }
}
