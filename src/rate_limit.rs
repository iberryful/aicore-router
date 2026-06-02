//! Rate limiting for authentication attempts.
//!
//! Tracks failed authentication attempts per IP address and enforces
//! a cooldown period after too many failures.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const MAX_FAILED_ATTEMPTS: u32 = 5;
const COOLDOWN_DURATION: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct FailedAttemptInfo {
    count: u32,
    last_failure: Instant,
}

/// Rate limiter that tracks failed auth attempts per IP.
#[derive(Debug, Clone)]
pub struct AuthRateLimiter {
    attempts: Arc<RwLock<HashMap<String, FailedAttemptInfo>>>,
}

impl Default for AuthRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthRateLimiter {
    pub fn new() -> Self {
        Self {
            attempts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if the given IP is rate-limited. Returns remaining cooldown if limited.
    pub async fn is_rate_limited(&self, ip: &str) -> Option<Duration> {
        let elapsed_info = {
            let attempts = self.attempts.read().await;
            attempts.get(ip).and_then(|info| {
                if info.count >= MAX_FAILED_ATTEMPTS {
                    Some(info.last_failure.elapsed())
                } else {
                    None
                }
            })
        };
        elapsed_info.and_then(|elapsed| {
            if elapsed < COOLDOWN_DURATION {
                Some(COOLDOWN_DURATION.saturating_sub(elapsed))
            } else {
                None
            }
        })
    }

    /// Record a failed authentication attempt for the given IP.
    pub async fn record_failure(&self, ip: &str) {
        let now = Instant::now();
        let mut attempts = self.attempts.write().await;
        let entry = attempts.entry(ip.to_string()).or_insert(FailedAttemptInfo {
            count: 0,
            last_failure: now,
        });

        // Reset counter if the previous cooldown has expired
        if entry.last_failure.elapsed() >= COOLDOWN_DURATION {
            entry.count = 0;
        }

        entry.count += 1;
        entry.last_failure = now;
    }

    /// Reset the failure counter on successful authentication.
    pub async fn record_success(&self, ip: &str) {
        self.attempts.write().await.remove(ip);
    }

    /// Remove expired entries to prevent unbounded memory growth.
    pub async fn cleanup(&self) {
        let mut attempts = self.attempts.write().await;
        attempts.retain(|_, info| info.last_failure.elapsed() < COOLDOWN_DURATION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_not_rate_limited_initially() {
        let limiter = AuthRateLimiter::new();
        assert!(limiter.is_rate_limited("1.2.3.4").await.is_none());
    }

    #[tokio::test]
    async fn test_rate_limited_after_max_failures() {
        let limiter = AuthRateLimiter::new();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            limiter.record_failure("1.2.3.4").await;
        }
        assert!(limiter.is_rate_limited("1.2.3.4").await.is_some());
    }

    #[tokio::test]
    async fn test_not_limited_below_threshold() {
        let limiter = AuthRateLimiter::new();
        for _ in 0..MAX_FAILED_ATTEMPTS - 1 {
            limiter.record_failure("1.2.3.4").await;
        }
        assert!(limiter.is_rate_limited("1.2.3.4").await.is_none());
    }

    #[tokio::test]
    async fn test_success_resets_counter() {
        let limiter = AuthRateLimiter::new();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            limiter.record_failure("1.2.3.4").await;
        }
        assert!(limiter.is_rate_limited("1.2.3.4").await.is_some());

        limiter.record_success("1.2.3.4").await;
        assert!(limiter.is_rate_limited("1.2.3.4").await.is_none());
    }

    #[tokio::test]
    async fn test_different_ips_independent() {
        let limiter = AuthRateLimiter::new();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            limiter.record_failure("1.2.3.4").await;
        }
        assert!(limiter.is_rate_limited("1.2.3.4").await.is_some());
        assert!(limiter.is_rate_limited("5.6.7.8").await.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired() {
        let limiter = AuthRateLimiter::new();
        // Manually insert an expired entry
        {
            let mut attempts = limiter.attempts.write().await;
            attempts.insert(
                "old-ip".to_string(),
                FailedAttemptInfo {
                    count: MAX_FAILED_ATTEMPTS,
                    last_failure: Instant::now() - COOLDOWN_DURATION - Duration::from_secs(1),
                },
            );
        }
        limiter.cleanup().await;
        assert!(limiter.is_rate_limited("old-ip").await.is_none());
        // Verify it was actually removed
        let attempts = limiter.attempts.read().await;
        assert!(!attempts.contains_key("old-ip"));
    }
}
