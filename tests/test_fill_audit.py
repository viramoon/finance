from datetime import date
from decimal import Decimal
import unittest

from nautilus_trader.backtest.engine import BacktestEngine
from nautilus_trader.backtest.models import FixedFeeModel
from nautilus_trader.config import BacktestEngineConfig, LoggingConfig
from nautilus_trader.model.currencies import USD
from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.enums import AccountType, OmsType, OrderSide
from nautilus_trader.model.events import OrderFilled
from nautilus_trader.model.objects import Money, Price, Quantity
from nautilus_trader.trading.strategy import Strategy

from fertile_finance.epoch import MAX_UNIX_NS
from fertile_finance.fill_audit import FillSignal, audit_logical_fills, merge_order_fills
from fertile_finance.portfolio_backtest import _performance
from fertile_finance.research_catalog import cash_equity


SIGNAL_TS = 1732568400000000000
FILL_TS = 1732654800000000000
STARTING_CASH = Decimal("10000.00")
ORDER_SIZE = Quantity.from_int(10)


class SubmitOneMarketOrder(Strategy):
    def __init__(self, bar_type):
        super().__init__()
        self.bar_type = bar_type
        self.signal_ts = None
        self.fills = []

    def on_start(self):
        self.subscribe_bars(self.bar_type)

    def on_bar(self, bar):
        if self.signal_ts is None:
            self.signal_ts = bar.ts_event
        else:
            order = self.order_factory.market(self.bar_type.instrument_id, OrderSide.BUY, ORDER_SIZE)
            self.submit_order(order)

    def on_order_filled(self, event):
        self.fills.append(event)


def run_fixture(volume):
    instrument = cash_equity("INDEX-FIXTURE.SIM")
    bar_type = BarType.from_str(f"{instrument.id}-1-DAY-LAST-EXTERNAL")
    prices = (
        (SIGNAL_TS, ("99.50", "100.00", "99.00", "99.75")),
        (FILL_TS, ("100.00", "101.00", "99.50", "100.50")),
    )
    bars = [
        Bar(bar_type, *(Price.from_str(price) for price in ohlc), Quantity.from_int(volume), ts, ts)
        for ts, ohlc in prices
    ]
    engine = BacktestEngine(BacktestEngineConfig(logging=LoggingConfig(bypass_logging=True)))
    try:
        engine.add_venue(
            venue=instrument.id.venue, oms_type=OmsType.NETTING, account_type=AccountType.CASH,
            base_currency=USD, starting_balances=[Money(STARTING_CASH, USD)],
            fee_model=FixedFeeModel(Money.from_str("1.00 USD")),
        )
        strategy = SubmitOneMarketOrder(bar_type)
        engine.add_instrument(instrument)
        engine.add_strategy(strategy)
        engine.add_data(bars)
        engine.run()
        orders = engine.cache.orders()
        account = engine.portfolio.account(instrument.id.venue)
        cash = account.balance_total(USD).as_decimal()
        position = engine.portfolio.net_position(instrument.id)
        return strategy, orders, cash, position
    finally:
        engine.dispose()


def edit_fill(event, **changes):
    return OrderFilled.from_dict({**OrderFilled.to_dict(event), **changes})


class FillAuditTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.runs = {volume: run_fixture(volume) for volume in (0, 100)}

    def test_zero_and_normal_volume_count_one_logical_trade(self):
        cases = (
            (0, [("1", "100.50", "1.00 USD"), ("9", "100.51", "0.00 USD")], "1005.09", "8993.91"),
            (100, [("10", "100.50", "1.00 USD")], "1005.00", "8994.00"),
        )
        for volume, expected_events, notional, ending_cash in cases:
            with self.subTest(volume=volume):
                strategy, orders, cash, position = self.runs[volume]
                fills = strategy.fills
                raw = [(str(fill.last_qty), str(fill.last_px), str(fill.commission)) for fill in fills]
                print(f"volume={volume} orders={len(orders)} raw_fills={raw} "
                      f"fill_ts={[fill.ts_event for fill in fills]} cash={cash} position={position}")
                self.assertEqual(len(orders), 1)
                self.assertEqual(raw, expected_events)
                self.assertTrue(all(fill.ts_event == FILL_TS for fill in fills))
                (logical,) = merge_order_fills(fills)
                self.assertEqual(logical.order_id, orders[0].client_order_id)
                self.assertEqual(logical.raw_fills, tuple(fills))
                self.assertEqual(logical.fill_ts, FILL_TS)
                self.assertLess(strategy.signal_ts, logical.fill_ts)
                self.assertEqual(logical.quantity, orders[0].filled_qty.as_decimal())
                self.assertEqual(logical.quantity, ORDER_SIZE.as_decimal())
                self.assertEqual(logical.notional, Decimal(notional))
                self.assertEqual(logical.average_price, Decimal(notional) / ORDER_SIZE.as_decimal())
                self.assertEqual(logical.commission, Decimal("1.00"))
                self.assertEqual(STARTING_CASH + logical.cash_delta, cash)
                self.assertEqual(cash, Decimal(ending_cash))
                self.assertEqual(logical.position_delta, position)
                self.assertEqual(position, ORDER_SIZE.as_decimal())

    def test_keeps_different_orders_and_timestamps_separate(self):
        first, second = self.runs[0][0].fills
        other_order = edit_fill(first, client_order_id="OTHER", order_side="SELL")
        later = edit_fill(second, ts_event=FILL_TS + 1)
        fills = merge_order_fills([first, other_order, later])

        self.assertEqual([fill.fill_ts for fill in fills], [FILL_TS, FILL_TS, FILL_TS + 1])
        self.assertEqual([fill.quantity for fill in fills], [Decimal(1), Decimal(1), Decimal(9)])
        self.assertEqual(fills[1].position_delta, Decimal(-1))
        self.assertEqual(fills[1].cash_delta, Decimal("99.50"))

    def test_rejects_duplicate_and_inconsistent_fills(self):
        first, second = self.runs[0][0].fills
        with self.assertRaisesRegex(ValueError, "Duplicate trade"):
            merge_order_fills([first, edit_fill(first, ts_event=FILL_TS + 1)])
        with self.assertRaisesRegex(ValueError, "Inconsistent fill"):
            merge_order_fills([first, edit_fill(second, order_side="SELL")])
        with self.assertRaisesRegex(ValueError, "Inconsistent fill"):
            merge_order_fills([first, edit_fill(second, commission="0.00 EUR")])

    def test_research_result_audit_and_turnover_use_one_native_logical_fill(self):
        start, end = date(2024, 11, 25), date(2024, 11, 26)
        closes = {start: SIGNAL_TS, end: FILL_TS}
        for volume, expected in ((0, "10.0509"), (100, "10.0500")):
            with self.subTest(volume=volume):
                strategy, orders, cash, position = self.runs[volume]
                signals = {str(orders[0].client_order_id): FillSignal(start, strategy.signal_ts, end)}
                audit = audit_logical_fills(strategy.fills, signals)
                (trade,) = audit
                self.assertLess(trade.signal_ts, trade.fill_ts)
                self.assertEqual(trade.fill.quantity, position)
                equity = {start: STARTING_CASH, end: cash + position * Decimal("100.50")}
                windows, _ = _performance(equity, closes, audit, None, carried_valuation_dates=set())
                self.assertEqual(windows[-1]["metrics"]["turnover_pct"], expected)

    def test_fill_timestamp_obeys_epoch_range(self):
        first = self.runs[0][0].fills[0]
        (at_epoch,) = merge_order_fills([edit_fill(first, ts_event=0)])
        self.assertEqual(at_epoch.fill_ts, 0)
        with self.assertRaisesRegex(ValueError, "within int64"):
            merge_order_fills([edit_fill(first, ts_event=MAX_UNIX_NS + 1)])


if __name__ == "__main__":
    unittest.main()
