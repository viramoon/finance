#!/usr/bin/env bash
set -Eeuo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
# 不调用 uv run 自动重建源码；先按 build.sh 构建并验证本地 wheel。
python="$root/venv/bin/python"
"$python" "$repo/scripts/verify-source.py"
PYTHONPATH="$repo/python${PYTHONPATH:+:$PYTHONPATH}" "$python" -m fertile_finance.backtest_sample
