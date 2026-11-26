#!/bin/sh
# Reproduce the three source files registered as table-format false positives.
# The input is the reviewed F9a commit. Only these fixed Git paths are exported.
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
exec git -C "$root" archive --format=tar 1bef03873295d75ae47c854db4c01a389238dc1a -- \
    crates/finance-ib/src/market/replay_tests.rs \
    crates/finance-ib/tests/entitlements.rs \
    third_party/ibapi/src/messages.rs
