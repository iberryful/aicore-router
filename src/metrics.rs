//! Real-time usage metrics aggregation service.
//!
//! Tracks request counts (active, total, successful, failed) and
//! token usage (input, output, cache_read, cache_write) with
//! thread-safe atomic counters and a broadcast pub/sub channel.
//! Also tracks per-model token usage for cost estimation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, broadcast};

/// Accumulated token usage across all requests.
#[derive(Debug, Clone, Default)]
pub struct UsageMetrics {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
}

/// Per-model token counts for cost estimation.
#[derive(Debug, Clone, Default)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// A point-in-time snapshot of all metrics.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub total_requests: u64,
    pub active_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub usage: UsageMetrics,
}

/// Events broadcast to subscribers when metrics change.
#[derive(Debug, Clone)]
pub enum MetricsEvent {
    RequestStarted,
    RequestCompleted {
        success: bool,
        tokens: TokenCounts,
    },
}

struct MetricsInner {
    active_requests: AtomicU64,
    total_requests: AtomicU64,
    successful_requests: AtomicU64,
    failed_requests: AtomicU64,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
    total_cache_read_tokens: AtomicU64,
    total_cache_write_tokens: AtomicU64,
    model_usage: RwLock<HashMap<String, TokenCounts>>,
    sender: broadcast::Sender<MetricsEvent>,
}

/// Thread-safe metrics service with pub/sub support.
#[derive(Debug, Clone)]
pub struct MetricsService {
    inner: Arc<MetricsInner>,
}

impl std::fmt::Debug for MetricsInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsInner")
            .field(
                "active_requests",
                &self.active_requests.load(Ordering::Relaxed),
            )
            .field(
                "total_requests",
                &self.total_requests.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl Default for MetricsService {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsService {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(MetricsInner {
                active_requests: AtomicU64::new(0),
                total_requests: AtomicU64::new(0),
                successful_requests: AtomicU64::new(0),
                failed_requests: AtomicU64::new(0),
                total_input_tokens: AtomicU64::new(0),
                total_output_tokens: AtomicU64::new(0),
                total_cache_read_tokens: AtomicU64::new(0),
                total_cache_write_tokens: AtomicU64::new(0),
                model_usage: RwLock::new(HashMap::new()),
                sender,
            }),
        }
    }

    /// Increment active request count. Call when a request begins.
    pub fn increment_active(&self) {
        self.inner.active_requests.fetch_add(1, Ordering::Relaxed);
        self.inner.total_requests.fetch_add(1, Ordering::Relaxed);
        let _ = self.inner.sender.send(MetricsEvent::RequestStarted);
    }

    /// Decrement active request count. Call when a request finishes.
    pub fn decrement_active(&self) {
        // CAS loop to prevent underflow
        loop {
            let current = self.inner.active_requests.load(Ordering::Relaxed);
            if current == 0 {
                return;
            }
            if self
                .inner
                .active_requests
                .compare_exchange(current, current - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Record a completed request with optional token usage and model name.
    pub async fn record_completion(
        &self,
        success: bool,
        model: Option<&str>,
        tokens: &TokenCounts,
    ) {
        if success {
            self.inner
                .successful_requests
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.inner.failed_requests.fetch_add(1, Ordering::Relaxed);
        }

        self.inner.total_input_tokens.fetch_add(tokens.input, Ordering::Relaxed);
        self.inner.total_output_tokens.fetch_add(tokens.output, Ordering::Relaxed);
        self.inner.total_cache_read_tokens.fetch_add(tokens.cache_read, Ordering::Relaxed);
        self.inner.total_cache_write_tokens.fetch_add(tokens.cache_write, Ordering::Relaxed);

        // Update per-model tracking
        if let Some(model_name) = model {
            let mut model_map = self.inner.model_usage.write().await;
            let counts = model_map.entry(model_name.to_string()).or_default();
            counts.input = counts.input.saturating_add(tokens.input);
            counts.output = counts.output.saturating_add(tokens.output);
            counts.cache_read = counts.cache_read.saturating_add(tokens.cache_read);
            counts.cache_write = counts.cache_write.saturating_add(tokens.cache_write);
            drop(model_map);
        }

        let _ = self.inner.sender.send(MetricsEvent::RequestCompleted {
            success,
            tokens: tokens.clone(),
        });
    }

    /// Non-blocking snapshot for synchronous contexts (e.g. TUI rendering).
    pub fn snapshot_sync(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            total_requests: self.inner.total_requests.load(Ordering::Relaxed),
            active_requests: self.inner.active_requests.load(Ordering::Relaxed),
            successful_requests: self.inner.successful_requests.load(Ordering::Relaxed),
            failed_requests: self.inner.failed_requests.load(Ordering::Relaxed),
            usage: UsageMetrics {
                total_input_tokens: self.inner.total_input_tokens.load(Ordering::Relaxed),
                total_output_tokens: self.inner.total_output_tokens.load(Ordering::Relaxed),
                total_cache_read_tokens: self.inner.total_cache_read_tokens.load(Ordering::Relaxed),
                total_cache_write_tokens: self.inner.total_cache_write_tokens.load(Ordering::Relaxed),
            },
        }
    }

    /// Get per-model token usage for cost estimation.
    pub async fn session_usage_by_model(&self) -> HashMap<String, TokenCounts> {
        self.inner.model_usage.read().await.clone()
    }

    /// Non-blocking per-model usage for synchronous contexts.
    /// Returns None if the lock is contended.
    pub fn session_usage_by_model_sync(&self) -> Option<HashMap<String, TokenCounts>> {
        self.inner
            .model_usage
            .try_read()
            .ok()
            .map(|m| m.clone())
    }

    /// Subscribe to real-time metrics events.
    pub fn subscribe(&self) -> broadcast::Receiver<MetricsEvent> {
        self.inner.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_increment_decrement_active() {
        let ms = MetricsService::new();
        ms.increment_active();
        ms.increment_active();
        let snap = ms.snapshot_sync();
        assert_eq!(snap.active_requests, 2);
        assert_eq!(snap.total_requests, 2);

        ms.decrement_active();
        let snap = ms.snapshot_sync();
        assert_eq!(snap.active_requests, 1);
    }

    #[tokio::test]
    async fn test_decrement_no_underflow() {
        let ms = MetricsService::new();
        ms.decrement_active(); // should not panic or underflow
        let snap = ms.snapshot_sync();
        assert_eq!(snap.active_requests, 0);
    }

    #[tokio::test]
    async fn test_record_completion_success() {
        let ms = MetricsService::new();
        ms.record_completion(true, Some("test-model"), &TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 5 })
            .await;
        let snap = ms.snapshot_sync();
        assert_eq!(snap.successful_requests, 1);
        assert_eq!(snap.failed_requests, 0);
        assert_eq!(snap.usage.total_input_tokens, 100);
        assert_eq!(snap.usage.total_output_tokens, 50);
        assert_eq!(snap.usage.total_cache_read_tokens, 10);
        assert_eq!(snap.usage.total_cache_write_tokens, 5);
    }

    #[tokio::test]
    async fn test_record_completion_failure() {
        let ms = MetricsService::new();
        ms.record_completion(false, None, &TokenCounts::default()).await;
        let snap = ms.snapshot_sync();
        assert_eq!(snap.successful_requests, 0);
        assert_eq!(snap.failed_requests, 1);
    }

    #[tokio::test]
    async fn test_accumulation() {
        let ms = MetricsService::new();
        ms.record_completion(true, Some("gpt-4o"), &TokenCounts { input: 100, output: 50, cache_read: 0, cache_write: 0 })
            .await;
        ms.record_completion(true, Some("gpt-4o"), &TokenCounts { input: 200, output: 100, cache_read: 30, cache_write: 0 })
            .await;
        let snap = ms.snapshot_sync();
        assert_eq!(snap.usage.total_input_tokens, 300);
        assert_eq!(snap.usage.total_output_tokens, 150);
        assert_eq!(snap.usage.total_cache_read_tokens, 30);
    }

    #[tokio::test]
    async fn test_per_model_tracking() {
        let ms = MetricsService::new();
        ms.record_completion(true, Some("claude-sonnet-4-5"), &TokenCounts { input: 100, output: 50, cache_read: 10, cache_write: 5 })
            .await;
        ms.record_completion(true, Some("gpt-4o"), &TokenCounts { input: 200, output: 100, cache_read: 0, cache_write: 0 })
            .await;
        ms.record_completion(true, Some("claude-sonnet-4-5"), &TokenCounts { input: 300, output: 150, cache_read: 20, cache_write: 0 })
            .await;

        let model_usage = ms.session_usage_by_model().await;

        let claude = model_usage.get("claude-sonnet-4-5").unwrap();
        assert_eq!(claude.input, 400);
        assert_eq!(claude.output, 200);
        assert_eq!(claude.cache_read, 30);
        assert_eq!(claude.cache_write, 5);

        let gpt = model_usage.get("gpt-4o").unwrap();
        assert_eq!(gpt.input, 200);
        assert_eq!(gpt.output, 100);
        assert_eq!(gpt.cache_read, 0);
        assert_eq!(gpt.cache_write, 0);
    }

    #[tokio::test]
    async fn test_no_model_not_tracked() {
        let ms = MetricsService::new();
        ms.record_completion(true, None, &TokenCounts { input: 100, output: 50, cache_read: 0, cache_write: 0 })
            .await;

        let model_usage = ms.session_usage_by_model().await;
        assert!(model_usage.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_receives_events() {
        let ms = MetricsService::new();
        let mut rx = ms.subscribe();
        ms.increment_active();
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, MetricsEvent::RequestStarted));
    }
}
