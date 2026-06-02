//! Load balancer for distributing requests across multiple providers.
//!
//! Supports multiple strategies:
//! - Round-robin: Distribute requests evenly across providers
//! - Fallback: Always try the first provider, only switch on 429

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};

use crate::config::{LoadBalancingStrategy, Provider};

/// An iterator over providers in load-balanced order (zero-allocation).
pub struct OrderedProviders<'a> {
    providers: &'a [Provider],
    start: usize,
    index: usize,
    len: usize,
}

impl<'a> Iterator for OrderedProviders<'a> {
    type Item = &'a Provider;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.len {
            return None;
        }
        let item = &self.providers[(self.start + self.index) % self.len];
        self.index += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for OrderedProviders<'_> {}

/// Load balancer that distributes requests across multiple providers.
#[derive(Debug, Clone)]
pub struct LoadBalancer {
    providers: Arc<Vec<Provider>>,
    current_index: Arc<AtomicUsize>,
    strategy: LoadBalancingStrategy,
}

impl LoadBalancer {
    /// Create a new load balancer with the given providers and strategy.
    /// Only enabled providers are included; returns an error when none remain
    /// so the binary refuses to start in a non-functional state.
    pub fn new(providers: Vec<Provider>, strategy: LoadBalancingStrategy) -> Result<Self> {
        let enabled_providers: Vec<Provider> =
            providers.into_iter().filter(|p| p.enabled).collect();

        if enabled_providers.is_empty() {
            bail!(
                "No enabled providers configured. Set at least one provider's `enabled: true` in config."
            );
        }

        Ok(Self {
            providers: Arc::new(enabled_providers),
            current_index: Arc::new(AtomicUsize::new(0)),
            strategy,
        })
    }

    /// Get providers ordered according to the configured strategy.
    ///
    /// - `RoundRobin`: Returns providers starting from the current round-robin position,
    ///   then advances the index for the next request.
    /// - `Fallback`: Always returns providers in their original order (first provider first),
    ///   does not advance any index.
    pub fn get_ordered_providers(&self) -> OrderedProviders<'_> {
        let len = self.providers.len();
        let start = match self.strategy {
            LoadBalancingStrategy::RoundRobin => {
                self.current_index.fetch_add(1, Ordering::Relaxed) % len
            }
            LoadBalancingStrategy::Fallback => 0,
        };
        OrderedProviders {
            providers: &self.providers,
            start,
            index: 0,
            len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_provider(name: &str, enabled: bool) -> Provider {
        Provider {
            name: name.to_string(),
            uaa_token_url: format!("https://{}.example.com/oauth/token", name),
            uaa_client_id: format!("{}-client", name),
            uaa_client_secret: format!("{}-secret", name),
            genai_api_url: format!("https://api.{}.example.com", name),
            resource_group: "default".to_string(),
            weight: 1,
            enabled,
        }
    }

    #[test]
    fn test_disabled_providers_excluded() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", false), // disabled
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();

        let ordered: Vec<&Provider> = balancer.get_ordered_providers().collect();
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].name, "provider1");
        assert_eq!(ordered[1].name, "provider3");
    }

    #[test]
    fn test_get_ordered_providers_round_robin() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();

        // First call starts at index 0
        let names: Vec<&str> = balancer
            .get_ordered_providers()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);

        // Second call starts at index 1
        let names: Vec<&str> = balancer
            .get_ordered_providers()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["provider2", "provider3", "provider1"]);
    }

    #[test]
    fn test_get_ordered_providers_fallback() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::Fallback).unwrap();

        // Always starts from provider1
        let names: Vec<&str> = balancer
            .get_ordered_providers()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);

        // Second call also starts from provider1 (no rotation)
        let names: Vec<&str> = balancer
            .get_ordered_providers()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);
    }

    #[test]
    fn test_new_rejects_when_no_enabled_providers() {
        // Empty list rejected outright.
        assert!(LoadBalancer::new(vec![], LoadBalancingStrategy::RoundRobin).is_err());

        // A list of disabled providers also rejected — there's nothing to route to.
        let all_disabled = vec![
            create_test_provider("provider1", false),
            create_test_provider("provider2", false),
        ];
        assert!(LoadBalancer::new(all_disabled, LoadBalancingStrategy::RoundRobin).is_err());
    }
}
