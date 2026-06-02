#!/bin/bash
# E2E Test Runner — local only; excluded from CI and commit hooks.
#
# The harness spawns a single shared `acr` process via OnceCell and reuses
# it across every test. To keep state-sensitive scenarios (rate limits, DB
# rows) deterministic, tests run sequentially via `--test-threads=1`.
#
# We pre-build the `acr` binary so the test harness can spawn it directly
# (avoiding cargo lock contention from a `cargo build` invocation inside a
# running `cargo test`).

set -euo pipefail

echo "Building acr binary…"
cargo build --bin acr

echo
echo "Running e2e tests against your real ~/.aicore/config.yaml providers."
echo "Tests run serially; this can take a few minutes."
echo
cargo test --features e2e --test e2e_tests -- --test-threads=1 --nocapture "$@"

echo
echo "E2E tests completed."
