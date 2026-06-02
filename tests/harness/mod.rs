//! Shared test harness for the e2e suite.
//!
//! `acr` is spawned **once** per test run via [`process::shared`] and reused
//! across every test. Tests must run with `--test-threads=1` so that
//! state-sensitive scenarios (rate limits, DB row assertions) don't observe
//! interleaving from concurrent tests. `run_e2e_tests.sh` enforces this.

#![cfg(feature = "e2e")]

pub mod assertions;
pub mod client;
pub mod config_synth;
pub mod process;
pub mod sse;
