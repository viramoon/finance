#!/bin/sh
# Export the committed Rust sources containing the fixed TotalView replay cases.
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
exec git -C "$root" archive --format=tar HEAD -- \
    crates/finance-collector/tests/nasdaq_totalview.rs \
    crates/finance-ib/src/depth/focus/tests.rs \
    crates/finance-ib/src/depth/transport_tests.rs
