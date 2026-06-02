//! End-to-end integration tests for `acr`.
//!
//! Tests boot a single `acr` process (lazily, once per test run) bound to a
//! random loopback port, and assert wire-level behavior against the user's
//! real SAP AI Core providers. The harness synthesizes its own config so the
//! user's `~/.aicore/config.yaml` is never modified — only its `providers`,
//! `models`, and `fallback_models` are reused.
//!
//! Run with: `./run_e2e_tests.sh` (handles the `cargo build --bin acr` and
//! `--test-threads=1` plumbing). Tests are gated by `feature = "e2e"` so
//! they're skipped from `cargo test` and CI by default.
//!
//! See `tests/harness/` for the shared infrastructure and
//! `tests/scenarios/` for the case-by-case wire assertions.

#![cfg(feature = "e2e")]

mod harness;
mod scenarios;
