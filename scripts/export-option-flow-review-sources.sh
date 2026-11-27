#!/bin/sh
# Export the committed source of the fixed synthetic option-flow callbacks.
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
exec git -C "$root" archive --format=tar HEAD -- \
    crates/finance-collector/src/option_flow/observation/tests.rs
