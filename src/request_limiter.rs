//! Per-API-key requests-per-minute rate limiter.
//!
//! Rejects burst traffic from a single key with HTTP 429 + `Retry-After`,
//! complementing the daily/monthly **token** quotas in `quota.rs`. The two
//! concerns deliberately live in separate modules:
//!
//! * `quota.rs` tracks cumulative *budgets* against calendar windows
//!   (midnight / month-end UTC), backed by the requests DB on startup.
//! * This module tracks instantaneous *rate* via `governor`'s GCRA token
//!   bucket, in memory only.
//!
//! Keying is the same SHA-256 hash used by `quota::hash_api_key`, so a single
//! request lookup costs at most one DashMap probe per check.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{
    Quota, RateLimiter,
    clock::{Clock, DefaultClock},
    middleware::NoOpMiddleware,
    state::keyed::DefaultKeyedStateStore,
};

use crate::config::{ApiKeyConfig, QuotaConfig};

type Limiter = RateLimiter<
    String,
    DefaultKeyedStateStore<String>,
    DefaultClock,
    NoOpMiddleware<<DefaultClock as governor::clock::Clock>::Instant>,
>;

/// Result of a request-rate check.
pub enum RequestLimitResult {
    Allowed,
    Exceeded { retry_after_secs: u64 },
}

/// Per-API-key requests-per-minute limiter.
///
/// Internally maintains one `governor::RateLimiter` per distinct configured
/// limit value. A key whose configured limit is `Some(rpm)` is checked
/// against the limiter for that rpm; a key with `None` (unlimited) is
/// always allowed.
pub struct RequestLimiter {
    /// rpm value → limiter for that rate. Built once at startup.
    by_rpm: HashMap<NonZeroU32, Arc<Limiter>>,
    /// key_hash → resolved rpm (None = unlimited).
    key_rpm: HashMap<String, Option<NonZeroU32>>,
    /// Default rpm applied to keys without a per-key override (None = unlimited).
    default_rpm: Option<NonZeroU32>,
}

impl RequestLimiter {
    /// Build a limiter from configured per-key overrides + global default.
    /// Returns `None` if no rpm is configured anywhere — saves the cost of
    /// even constructing limiter state.
    pub fn from_config(api_keys: &[ApiKeyConfig], quotas: &QuotaConfig) -> Option<Self> {
        let default_rpm = nonzero_rpm(quotas.requests_per_minute);

        let key_rpm: HashMap<String, Option<NonZeroU32>> = api_keys
            .iter()
            .map(|k| {
                let resolved = match k.requests_per_minute {
                    Some(0) => None, // explicit unlimited override
                    Some(n) => NonZeroU32::new(n),
                    None => default_rpm,
                };
                (crate::quota::hash_api_key(&k.key), resolved)
            })
            .collect();

        let any_limited = default_rpm.is_some() || key_rpm.values().any(|v| v.is_some());
        if !any_limited {
            return None;
        }

        // Build one shared limiter per distinct rpm value used.
        let mut by_rpm: HashMap<NonZeroU32, Arc<Limiter>> = HashMap::new();
        let distinct_rpms = key_rpm.values().filter_map(|v| *v).chain(default_rpm);
        for rpm in distinct_rpms {
            by_rpm
                .entry(rpm)
                .or_insert_with(|| Arc::new(RateLimiter::keyed(Quota::per_minute(rpm))));
        }

        Some(Self {
            by_rpm,
            key_rpm,
            default_rpm,
        })
    }

    /// Check whether a request from this key is allowed right now.
    /// On `Exceeded`, returns the wall-clock seconds until the next request would
    /// succeed (rounded up, minimum 1) for use as `Retry-After`.
    pub fn check(&self, key_hash: &str) -> RequestLimitResult {
        let rpm = self
            .key_rpm
            .get(key_hash)
            .copied()
            .unwrap_or(self.default_rpm);

        let Some(rpm) = rpm else {
            return RequestLimitResult::Allowed;
        };

        // The limiter for `rpm` is guaranteed present because `from_config`
        // pre-registers one for every distinct value reachable here.
        let limiter = self
            .by_rpm
            .get(&rpm)
            .expect("limiter for configured rpm must exist");

        match limiter.check_key(&key_hash.to_string()) {
            Ok(()) => RequestLimitResult::Allowed,
            Err(not_until) => {
                let wait = not_until.wait_time_from(DefaultClock::default().now());
                let secs = wait.as_secs().max(1);
                RequestLimitResult::Exceeded {
                    retry_after_secs: secs,
                }
            }
        }
    }
}

fn nonzero_rpm(opt: Option<u32>) -> Option<NonZeroU32> {
    match opt {
        Some(0) | None => None,
        Some(n) => NonZeroU32::new(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use governor::clock::Clock;

    fn key_cfg(name: &str, rpm: Option<u32>) -> ApiKeyConfig {
        ApiKeyConfig {
            key: name.to_string(),
            daily_token_limit: None,
            monthly_token_limit: None,
            requests_per_minute: rpm,
        }
    }

    fn quotas(rpm: Option<u32>) -> QuotaConfig {
        QuotaConfig {
            enabled: true,
            daily_token_limit: None,
            monthly_token_limit: None,
            requests_per_minute: rpm,
            unknown: Default::default(),
        }
    }

    #[test]
    fn from_config_returns_none_when_no_limits_configured() {
        let keys = vec![key_cfg("a", None)];
        assert!(RequestLimiter::from_config(&keys, &quotas(None)).is_none());
    }

    #[test]
    fn rejects_burst_above_limit() {
        let keys = vec![key_cfg("burst", Some(2))];
        let limiter = RequestLimiter::from_config(&keys, &quotas(None)).unwrap();
        let h = crate::quota::hash_api_key("burst");

        // Two should pass (burst capacity = rpm at minute granularity).
        assert!(matches!(limiter.check(&h), RequestLimitResult::Allowed));
        assert!(matches!(limiter.check(&h), RequestLimitResult::Allowed));

        // Third should be rejected with a positive Retry-After.
        match limiter.check(&h) {
            RequestLimitResult::Exceeded { retry_after_secs } => {
                assert!(retry_after_secs >= 1);
                assert!(retry_after_secs <= 60);
            }
            RequestLimitResult::Allowed => panic!("expected rate-limit"),
        }
    }

    #[test]
    fn per_key_zero_means_unlimited_even_when_global_set() {
        let keys = vec![key_cfg("admin", Some(0))];
        let limiter = RequestLimiter::from_config(&keys, &quotas(Some(1))).unwrap();
        let h = crate::quota::hash_api_key("admin");

        // Many requests in a row succeed — admin is opted out of the global limit.
        for _ in 0..20 {
            assert!(matches!(limiter.check(&h), RequestLimitResult::Allowed));
        }
    }

    #[test]
    fn unknown_key_falls_back_to_default_global() {
        let keys = vec![key_cfg("known", None)];
        let limiter = RequestLimiter::from_config(&keys, &quotas(Some(1))).unwrap();

        // An unknown key still gets the global default.
        let h = crate::quota::hash_api_key("never-configured");
        assert!(matches!(limiter.check(&h), RequestLimitResult::Allowed));
        assert!(matches!(
            limiter.check(&h),
            RequestLimitResult::Exceeded { .. }
        ));
    }

    #[test]
    fn per_key_override_beats_global() {
        let keys = vec![key_cfg("vip", Some(5)), key_cfg("plebe", None)];
        let limiter = RequestLimiter::from_config(&keys, &quotas(Some(1))).unwrap();

        let vip = crate::quota::hash_api_key("vip");
        let plebe = crate::quota::hash_api_key("plebe");

        // VIP has a private rpm=5 limiter, plebe shares the global rpm=1.
        for _ in 0..5 {
            assert!(matches!(limiter.check(&vip), RequestLimitResult::Allowed));
        }
        assert!(matches!(limiter.check(&plebe), RequestLimitResult::Allowed));
        assert!(matches!(
            limiter.check(&plebe),
            RequestLimitResult::Exceeded { .. }
        ));
    }

    #[test]
    fn governor_clock_is_constructible() {
        // Sanity check: ensure the API we depend on stays exercised.
        let _ = DefaultClock::default().now();
    }
}
