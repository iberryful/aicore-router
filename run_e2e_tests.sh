#!/bin/bash

# E2E Test Runner
# This script runs end-to-end tests locally only
# These tests are excluded from CI and commit hooks

echo "Running E2E tests locally..."
echo "Make sure your config.yaml is properly configured!"

# Run tests with e2e feature enabled
cargo test --features e2e --test e2e_tests

echo "E2E tests completed!"
