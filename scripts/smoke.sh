#!/usr/bin/env bash
set -Eeuo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
python="$root/venv/bin/python"
"$python" "$repo/scripts/verify-source.py"
example="$repo/vendor/nautilus_trader/examples/backtest/example_01_load_bars_from_custom_csv"
mkdir -p "$root/logs"
# 上游自带的 CSV 和计数策略，9 根 bar，无网络、无账户、无自研策略。
(cd "$example"; "$python" run_example.py) 2>&1 | tee "$root/logs/smoke.log"
grep -q 'Total bars processed: 9' "$root/logs/smoke.log"
echo "PASS upstream backtest example: 9 bars"
