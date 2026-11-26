#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} != --help && ${IB_LIVE_TEST:-} != 1 ]]; then
  printf '%s\n' 'The private paper probe requires IB_LIVE_TEST=1.' >&2
  exit 1
fi

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
exec cargo run --quiet --locked --offline -p finance-ib --bin finance-ib -- entitlements "$@"
