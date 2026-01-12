//! Load balancer for distributing requests across multiple providers.
//!
//! Supports multiple strategies:
//! - Round-robin: Distribute requests evenly across providers
//! - Fallback: Always try the first provider, only switch on 429

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{LoadBalancingStrategy, Provider};

/// Load balancer that distributes requests across multiple providers.
#[derive(Debug, Clone)]
pub struct LoadBalancer {
    providers: Arc<Vec<Provider>>,
    current_index: Arc<AtomicUsize>,
    strategy: LoadBalancingStrategy,
}

impl LoadBalancer {
    /// Create a new load balancer with the given providers and strategy.
    /// Only enabled providers are included.
    pub fn new(providers: Vec<Provider>, strategy: LoadBalancingStrategy) -> Self {
        let enabled_providers: Vec<Provider> =
            providers.into_iter().filter(|p| p.enabled).collect();

        Self {
            providers: Arc::new(enabled_providers),
            current_index: Arc::new(AtomicUsize::new(0)),
            strategy,
        }
    }

    /// Get the load balancing strategy.
    pub fn strategy(&self) -> &LoadBalancingStrategy {
        &self.strategy
    }

    /// Get the next provider using round-robin selection.
    /// Returns None if no providers are available.
    pub fn next(&self) -> Option<&Provider> {
        if self.providers.is_empty() {
            return None;
        }

        let index = self.current_index.fetch_add(1, Ordering::SeqCst) % self.providers.len();
        self.providers.get(index)
    }

    /// Get all providers in order, starting from a specific index.
    /// This is used for fallback - try providers in order until one succeeds.
    pub fn get_providers_from(&self, start_index: usize) -> Vec<&Provider> {
        if self.providers.is_empty() {
            return Vec::new();
        }

        let len = self.providers.len();
        (0..len)
            .map(|i| &self.providers[(start_index + i) % len])
            .collect()
    }

    /// Get the next index without incrementing (peek).
    pub fn current_index(&self) -> usize {
        self.current_index.load(Ordering::SeqCst) % self.providers.len().max(1)
    }

    /// Get providers ordered according to the configured strategy.
    ///
    /// - `RoundRobin`: Returns providers starting from the current round-robin position,
    ///   then advances the index for the next request.
    /// - `Fallback`: Always returns providers in their original order (first provider first),
    ///   does not advance any index.
    pub fn get_ordered_providers(&self) -> Vec<&Provider> {
        match self.strategy {
            LoadBalancingStrategy::RoundRobin => {
                let start = self.current_index.fetch_add(1, Ordering::SeqCst);
                self.get_providers_from(start)
            }
            LoadBalancingStrategy::Fallback => {
                // Always start from the first provider
                self.providers.iter().collect()
            }
        }
    }

    /// Get the number of enabled providers.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Check if there are no enabled providers.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Get a provider by name.
    pub fn get_by_name(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// Get all enabled providers.
    pub fn providers(&self) -> &[Provider] {
        &self.providers
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
    fn test_round_robin() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin);

        // Should cycle through providers in order
        assert_eq!(balancer.next().unwrap().name, "provider1");
        assert_eq!(balancer.next().unwrap().name, "provider2");
        assert_eq!(balancer.next().unwrap().name, "provider3");
        assert_eq!(balancer.next().unwrap().name, "provider1"); // wrap around
    }

    #[test]
    fn test_disabled_providers_excluded() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", false), // disabled
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin);

        assert_eq!(balancer.len(), 2);
        assert_eq!(balancer.next().unwrap().name, "provider1");
        assert_eq!(balancer.next().unwrap().name, "provider3");
        assert_eq!(balancer.next().unwrap().name, "provider1");
    }

    #[test]
    fn test_get_ordered_providers_round_robin() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin);

        // First call starts at index 0
        let ordered = balancer.get_ordered_providers();
        let names: Vec<&str> = ordered.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);

        // Second call starts at index 1
        let ordered = balancer.get_ordered_providers();
        let names: Vec<&str> = ordered.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["provider2", "provider3", "provider1"]);
    }

    #[test]
    fn test_get_ordered_providers_fallback() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::Fallback);

        // First call always starts from provider1
        let ordered = balancer.get_ordered_providers();
        let names: Vec<&str> = ordered.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);

        // Second call also starts from provider1 (no rotation)
        let ordered = balancer.get_ordered_providers();
        let names: Vec<&str> = ordered.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);

        // Third call - still from provider1
        let ordered = balancer.get_ordered_providers();
        let names: Vec<&str> = ordered.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["provider1", "provider2", "provider3"]);
    }

    #[test]
    fn test_empty_providers() {
        let balancer = LoadBalancer::new(vec![], LoadBalancingStrategy::RoundRobin);

        assert!(balancer.is_empty());
        assert!(balancer.next().is_none());
        assert!(balancer.get_ordered_providers().is_empty());
    }

    #[test]
    fn test_get_by_name() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin);

        assert!(balancer.get_by_name("provider1").is_some());
        assert!(balancer.get_by_name("provider2").is_some());
        assert!(balancer.get_by_name("nonexistent").is_none());
    }

    #[test]
    fn test_strategy_getter() {
        let providers = vec![create_test_provider("provider1", true)];

        let rr_balancer = LoadBalancer::new(providers.clone(), LoadBalancingStrategy::RoundRobin);
        assert_eq!(rr_balancer.strategy(), &LoadBalancingStrategy::RoundRobin);

        let fb_balancer = LoadBalancer::new(providers, LoadBalancingStrategy::Fallback);
        assert_eq!(fb_balancer.strategy(), &LoadBalancingStrategy::Fallback);
    }
}
