//! Load balancer for distributing requests across providers.
//!
//! Two levels:
//! - **Cross-pool** (a model's provider allowlist, or the default pool): ordered
//!   fallback — pools are offered in configured order, distinct backends so the
//!   request loop only advances on exhaustion.
//! - **Intra-pool** (credentials of one backend): stateless weighted-random
//!   shuffle, skipping credentials cooling down after a recent 429. The backend
//!   is shared, so spreading across credentials does not fragment the cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rand::seq::SliceRandom;

use crate::config::Provider;

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

/// Load balancer that distributes requests across provider pools.
#[derive(Debug, Clone)]
pub struct LoadBalancer {
    providers: Arc<Vec<Provider>>,
    /// Member name -> instant until which it is cooling down after a 429.
    cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
}

impl LoadBalancer {
    /// Create a new load balancer. Only enabled providers (expanded pool members)
    /// are retained; the binary refuses to start with nothing to route to.
    pub fn new(providers: Vec<Provider>) -> Result<Self> {
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
            cooldowns: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Get providers for the default pool (every enabled provider), ordered for
    /// dispatch: pools in first-appearance order, credentials weighted-shuffled.
    pub fn get_ordered_providers(&self) -> OrderedProviders<'_> {
        self.order_providers(None)
    }

    /// Get providers restricted to the named pools, in the given pool order. An
    /// empty list falls back to the default pool for backward compatibility.
    pub fn get_ordered_providers_for(&self, pool_names: &[String]) -> OrderedProviders<'_> {
        if pool_names.is_empty() {
            self.order_providers(None)
        } else {
            self.order_providers(Some(pool_names))
        }
    }

    /// Cool `member_name` down for `secs` seconds so the weighted shuffle skips it
    /// until the window elapses (unless its whole pool is cooling down).
    pub fn mark_cooldown(&self, member_name: &str, secs: u64) {
        if secs == 0 {
            return;
        }
        let until = Instant::now() + Duration::from_secs(secs);
        let mut cooldowns = self
            .cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cooldowns.insert(member_name.to_string(), until);
    }

    fn order_providers(&self, pool_names: Option<&[String]>) -> OrderedProviders<'_> {
        let pool_order: Vec<&str> = match pool_names {
            Some(names) => names.iter().map(String::as_str).collect(),
            None => {
                let mut seen: Vec<&str> = Vec::new();
                for provider in self.providers.iter() {
                    if !seen.contains(&provider.pool.as_str()) {
                        seen.push(provider.pool.as_str());
                    }
                }
                seen
            }
        };

        let now = Instant::now();
        let cooldowns = self
            .cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut ordered: Vec<&Provider> = Vec::new();
        for pool in pool_order {
            // Only credentials with weight > 0 ever receive load-balanced traffic.
            let candidates: Vec<&Provider> = self
                .providers
                .iter()
                .filter(|provider| provider.pool == pool && provider.weight > 0)
                .collect();
            if candidates.is_empty() {
                continue;
            }
            // Prefer credentials not cooling down; if every one is cooling down,
            // still offer the pool rather than dropping the request.
            let live: Vec<&Provider> = candidates
                .iter()
                .copied()
                .filter(|provider| {
                    cooldowns
                        .get(&provider.name)
                        .is_none_or(|until| now >= *until)
                })
                .collect();
            let pick_from = if live.is_empty() { candidates } else { live };
            ordered.extend(weighted_shuffle(pick_from));
        }

        OrderedProviders {
            inner: ordered.into_iter(),
        }
    }
}

/// Produce a full weighted-random permutation of `candidates` (weight is
/// proportional to selection probability at each position). All callers pass
/// weight > 0.
fn weighted_shuffle(candidates: Vec<&Provider>) -> Vec<&Provider> {
    if candidates.len() <= 1 {
        return candidates;
    }
    let mut rng = rand::thread_rng();
    match candidates.choose_multiple_weighted(&mut rng, candidates.len(), |p| p.weight as f64) {
        Ok(iter) => iter.copied().collect(),
        // Only errors on invalid/zero total weight, which the weight > 0 filter
        // already rules out; fall back to insertion order.
        Err(_) => candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool member with explicit pool, weight, and enabled flag.
    fn member(name: &str, pool: &str, weight: u32, enabled: bool) -> Provider {
        let mut p = crate::config::test_aicore_provider(name);
        p.pool = pool.to_string();
        p.weight = weight;
        p.enabled = enabled;
        p
    }

    /// A single-credential provider whose pool name equals its own name.
    fn create_test_provider(name: &str, enabled: bool) -> Provider {
        member(name, name, 1, enabled)
    }

    fn names(balancer: &LoadBalancer) -> Vec<String> {
        balancer
            .get_ordered_providers()
            .map(|p| p.name.clone())
            .collect()
    }

    #[test]
    fn test_disabled_providers_excluded() {
        let providers = vec![
            create_test_provider("provider1", true),
            create_test_provider("provider2", false), // disabled
            create_test_provider("provider3", true),
        ];
        let balancer = LoadBalancer::new(providers).unwrap();

        let mut got = names(&balancer);
        got.sort();
        assert_eq!(got, vec!["provider1", "provider3"]);
    }

    #[test]
    fn test_zero_weight_credential_excluded_from_shuffle() {
        let providers = vec![
            member("pool/a", "pool", 1, true),
            member("pool/b", "pool", 0, true), // excluded from LB
        ];
        let balancer = LoadBalancer::new(providers).unwrap();

        for _ in 0..20 {
            assert_eq!(names(&balancer), vec!["pool/a"]);
        }
    }

    #[test]
    fn test_default_pool_grouped_by_pool_in_config_order() {
        let providers = vec![
            member("A/1", "A", 1, true),
            member("A/2", "A", 1, true),
            member("B/1", "B", 1, true),
        ];
        let balancer = LoadBalancer::new(providers).unwrap();

        // Every credential of pool A is offered before pool B (cross-pool order
        // follows config order), regardless of intra-pool shuffle.
        for _ in 0..20 {
            let pools: Vec<String> = balancer
                .get_ordered_providers()
                .map(|p| p.pool.clone())
                .collect();
            assert_eq!(pools, vec!["A", "A", "B"]);
        }
    }

    #[test]
    fn test_allowlist_restricts_and_orders_pools() {
        let providers = vec![
            member("A/1", "A", 1, true),
            member("B/1", "B", 1, true),
            member("C/1", "C", 1, true),
        ];
        let balancer = LoadBalancer::new(providers).unwrap();
        let allowed = vec!["C".to_string(), "A".to_string()];

        let pools: Vec<String> = balancer
            .get_ordered_providers_for(&allowed)
            .map(|p| p.pool.clone())
            .collect();
        assert_eq!(pools, vec!["C", "A"]);
    }

    #[test]
    fn test_cooldown_skips_credential_but_keeps_pool_when_all_cooled() {
        let providers = vec![
            member("pool/a", "pool", 1, true),
            member("pool/b", "pool", 1, true),
        ];
        let balancer = LoadBalancer::new(providers).unwrap();

        // Cool one credential: only the other is offered.
        balancer.mark_cooldown("pool/a", 60);
        for _ in 0..20 {
            assert_eq!(names(&balancer), vec!["pool/b"]);
        }

        // Cool both: the pool still offers its credentials (last resort).
        balancer.mark_cooldown("pool/b", 60);
        let mut got = names(&balancer);
        got.sort();
        assert_eq!(got, vec!["pool/a", "pool/b"]);
    }

    #[test]
    fn test_weighted_shuffle_favors_higher_weight() {
        let providers = vec![
            member("pool/light", "pool", 1, true),
            member("pool/heavy", "pool", 99, true),
        ];
        let balancer = LoadBalancer::new(providers).unwrap();

        let mut heavy_first = 0;
        let draws = 1000;
        for _ in 0..draws {
            if balancer.get_ordered_providers().next().unwrap().name == "pool/heavy" {
                heavy_first += 1;
            }
        }
        // ~99% expected; a very loose bound keeps the test non-flaky.
        assert!(
            heavy_first > draws * 3 / 4,
            "heavy credential won first only {heavy_first}/{draws} times"
        );
    }

    #[test]
    fn test_new_rejects_when_no_enabled_providers() {
        assert!(LoadBalancer::new(vec![]).is_err());

        let all_disabled = vec![
            create_test_provider("provider1", false),
            create_test_provider("provider2", false),
        ];
        assert!(LoadBalancer::new(all_disabled).is_err());
    }
}
