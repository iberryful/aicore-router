//! Load balancer for distributing requests across multiple providers.
//!
//! Supports multiple strategies:
//! - Round-robin: Distribute requests evenly across providers
//! - Fallback: Always try the first provider, only switch on 429

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Result, bail};

use crate::config::{LoadBalancingStrategy, Provider};

/// An iterator over providers in load-balanced order.
pub struct OrderedProviders<'a> {
    inner: std::vec::IntoIter<&'a Provider>,
}

impl<'a> Iterator for OrderedProviders<'a> {
    type Item = &'a Provider;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for OrderedProviders<'_> {}

/// Load balancer that distributes requests across multiple providers.
#[derive(Debug, Clone)]
pub struct LoadBalancer {
    providers: Arc<Vec<Provider>>,
    round_robin_indices: Arc<Mutex<std::collections::HashMap<Vec<String>, usize>>>,
    strategy: LoadBalancingStrategy,
}

impl LoadBalancer {
    /// Create a new load balancer with the given providers and strategy.
    /// Only enabled providers are retained; zero-weight providers remain
    /// available for explicit per-model routing but are excluded by default.
    /// so the binary refuses to start in a non-functional state.
    pub fn new(providers: Vec<Provider>, strategy: LoadBalancingStrategy) -> Result<Self> {
        let enabled_providers: Vec<Provider> = providers
            .into_iter()
            .filter(|provider| provider.enabled)
            .collect();

        if enabled_providers.is_empty() {
            bail!(
                "No enabled providers configured. Set at least one provider's `enabled: true` in config."
            );
        }

        Ok(Self {
            providers: Arc::new(enabled_providers),
            round_robin_indices: Arc::new(Mutex::new(std::collections::HashMap::new())),
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
        self.order_providers(None)
    }

    /// Get only the named providers, preserving the model's configured order
    /// before applying the global load-balancing strategy. An empty list means
    /// all enabled providers for backward compatibility.
    pub fn get_ordered_providers_for(&self, provider_names: &[String]) -> OrderedProviders<'_> {
        if provider_names.is_empty() {
            self.order_providers(None)
        } else {
            self.order_providers(Some(provider_names))
        }
    }

    fn order_providers(&self, provider_names: Option<&[String]>) -> OrderedProviders<'_> {
        let mut providers: Vec<&Provider> = match provider_names {
            Some(names) => names
                .iter()
                .filter_map(|name| {
                    self.providers
                        .iter()
                        .find(|provider| provider.name == name.as_str())
                })
                .collect(),
            None => self
                .providers
                .iter()
                .filter(|provider| provider.weight > 0)
                .collect(),
        };

        if matches!(self.strategy, LoadBalancingStrategy::RoundRobin) && providers.len() > 1 {
            let key: Vec<String> = providers
                .iter()
                .map(|provider| provider.name.clone())
                .collect();
            let mut indices = self
                .round_robin_indices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let index = indices.entry(key).or_default();
            let start = *index % providers.len();
            *index = (*index).wrapping_add(1);
            providers.rotate_left(start);
        }

        OrderedProviders {
            inner: providers.into_iter(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_provider(name: &str, enabled: bool) -> Provider {
        let mut p = crate::config::test_aicore_provider(name);
        p.enabled = enabled;
        p
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
    fn test_zero_weight_providers_excluded() {
        let mut skipped = create_test_provider("provider2", true);
        skipped.weight = 0;
        let providers = vec![
            create_test_provider("provider1", true),
            skipped,
            create_test_provider("provider3", true),
        ];

        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();
        let names: Vec<&str> = balancer
            .get_ordered_providers()
            .map(|provider| provider.name.as_str())
            .collect();

        assert_eq!(names, vec!["provider1", "provider3"]);
    }

    #[test]
    fn test_zero_weight_provider_is_available_when_explicitly_selected() {
        let mut opt_in = create_test_provider("provider2", true);
        opt_in.weight = 0;
        let providers = vec![create_test_provider("provider1", true), opt_in];
        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();
        let allowed = vec!["provider2".to_string()];

        let names: Vec<&str> = balancer
            .get_ordered_providers_for(&allowed)
            .map(|provider| provider.name.as_str())
            .collect();

        assert_eq!(names, vec!["provider2"]);
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
    fn test_round_robin_is_limited_to_model_providers_in_model_order() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];
        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();
        let allowed = vec!["provider3".to_string(), "provider1".to_string()];

        let first: Vec<&str> = balancer
            .get_ordered_providers_for(&allowed)
            .map(|provider| provider.name.as_str())
            .collect();
        let second: Vec<&str> = balancer
            .get_ordered_providers_for(&allowed)
            .map(|provider| provider.name.as_str())
            .collect();

        assert_eq!(first, vec!["provider3", "provider1"]);
        assert_eq!(second, vec!["provider1", "provider3"]);
    }

    #[test]
    fn test_single_model_provider_never_falls_through() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];
        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();
        let allowed = vec!["provider2".to_string()];

        for _ in 0..3 {
            let names: Vec<&str> = balancer
                .get_ordered_providers_for(&allowed)
                .map(|provider| provider.name.as_str())
                .collect();
            assert_eq!(names, vec!["provider2"]);
        }
    }

    #[test]
    fn test_fallback_is_limited_to_model_providers_in_model_order() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];
        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::Fallback).unwrap();
        let allowed = vec!["provider3".to_string(), "provider1".to_string()];

        for _ in 0..2 {
            let names: Vec<&str> = balancer
                .get_ordered_providers_for(&allowed)
                .map(|provider| provider.name.as_str())
                .collect();
            assert_eq!(names, vec!["provider3", "provider1"]);
        }
    }

    #[test]
    fn test_round_robin_counters_are_independent_per_provider_set() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", true),
            create_test_provider("provider3", true),
        ];
        let balancer = LoadBalancer::new(providers, LoadBalancingStrategy::RoundRobin).unwrap();
        let restricted = vec!["provider1".to_string(), "provider3".to_string()];

        let first_restricted: Vec<&str> = balancer
            .get_ordered_providers_for(&restricted)
            .map(|provider| provider.name.as_str())
            .collect();
        let _: Vec<&Provider> = balancer.get_ordered_providers().collect();
        let second_restricted: Vec<&str> = balancer
            .get_ordered_providers_for(&restricted)
            .map(|provider| provider.name.as_str())
            .collect();

        assert_eq!(first_restricted, vec!["provider1", "provider3"]);
        assert_eq!(second_restricted, vec!["provider3", "provider1"]);
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

        let mut zero_weight = create_test_provider("provider1", true);
        zero_weight.weight = 0;
        let balancer = LoadBalancer::new(vec![zero_weight], LoadBalancingStrategy::RoundRobin)
            .expect("enabled zero-weight provider remains available for explicit routing");
        assert_eq!(balancer.get_ordered_providers().count(), 0);
    }
}
