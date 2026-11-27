#!/bin/sh
# Reproduce the source files registered as table-format false positives.
# Export the committed bytes from the current checkout.
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
exec git -C "$root" archive --format=tar HEAD -- \
    crates/finance-collector/src/option_flow/observation/tests.rs \
    crates/finance-ib/src/market/replay_tests.rs \
    crates/finance-ib/tests/entitlements.rs \
    third_party/ibapi/src/messages.rs
