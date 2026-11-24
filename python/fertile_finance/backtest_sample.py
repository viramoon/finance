"""运行锁定上游 CSV 示例，保存精确摘要；不编辑 vendor 或实现策略。"""
import json
import os
from pathlib import Path
import runpy
import sys
import tempfile
from datetime import datetime, timezone
from unittest.mock import patch


def atomic_write(path: Path, summary: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         prefix=".latest-", suffix=".tmp", delete=False) as output:
            temporary = Path(output.name)
            json.dump(summary, output, ensure_ascii=False, allow_nan=False)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def timestamp(nanoseconds: int) -> str:
    # 不把纳秒时间戳转成 float；示例数据为整秒。
    seconds, nanos = divmod(nanoseconds, 1_000_000_000)
    return datetime.fromtimestamp(seconds, timezone.utc).strftime("%Y-%m-%dT%H:%M:%S") + f".{nanos:09d}Z"


def run_sample(repo: Path, *, start: str | None = None, end: str | None = None) -> dict:
    from nautilus_trader.backtest import engine as engine_module
    from nautilus_trader.model.currencies import USD
    from nautilus_trader.model.identifiers import Venue

    captured = {}
    original = engine_module.BacktestEngine

    class CapturingEngine(original):
        def run(self):
            return super().run(start=start, end=end)

        def dispose(self):
            try:
                result = self.get_result()
                account = self.cache.account_for_venue(Venue("XCME"))
                # 直接使用 Money 的 Decimal，避免上游统计接口的 float PnL。
                pnl = account.balance_total(USD).as_decimal() - account.starting_balances()[USD].as_decimal()
                captured.update(run_id=result.run_id, total_orders=result.total_orders,
                                total_positions=result.total_positions,
                                start=timestamp(result.backtest_start), end=timestamp(result.backtest_end),
                                pnl=format(pnl, "f"), currency="USD")
            finally:
                super().dispose()

    example = repo / "vendor/nautilus_trader/examples/backtest/example_01_load_bars_from_custom_csv"
    previous = Path.cwd()
    sys.path.insert(0, str(example))
    try:
        os.chdir(example)
        with patch.object(engine_module, "BacktestEngine", CapturingEngine):
            namespace = runpy.run_path(str(example / "run_example.py"), run_name="__main__")
        captured.update(strategy=namespace["DemoStrategy"].__name__,
                        instrument=str(namespace["EURUSD_FUTURES_INSTRUMENT"].id),
                        generated_at=datetime.now(timezone.utc).isoformat())
        return captured
    finally:
        os.chdir(previous)
        sys.path.remove(str(example))


def main():
    root = Path(os.environ.get("FERTILE_DATA_ROOT", "/home/box/fertile/data"))
    if not root.is_absolute():
        raise ValueError("FERTILE_DATA_ROOT must be absolute")
    summary = run_sample(Path(__file__).resolve().parents[2])
    target = root / "finance-state/backtests/sample/latest.json"
    atomic_write(target, summary)
    print(f"SUMMARY_PATH={target}")


if __name__ == "__main__":
    main()
