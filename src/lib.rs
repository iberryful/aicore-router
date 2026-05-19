pub mod balancer;
pub mod cli;
pub mod client;
pub mod commands;
pub mod config;
pub mod constants;
#[cfg(feature = "db")]
pub mod database;
pub mod metrics;
pub mod proxy;
pub mod quota;
pub mod rate_limit;
pub mod registry;
pub mod routes;
pub mod table;
pub mod token;
#[cfg(feature = "tui")]
pub mod tui;

/// Format a cost value with adaptive precision: 4 decimal places below $1, 2 above.
pub(crate) fn format_cost_value(cost: f64) -> String {
    if cost < 1.0 {
        format!("${:.4}", cost)
    } else {
        format!("${:.2}", cost)
    }
}
