"""Offline HTTP replay with deterministic, hand-authored candles and trade rows.

No market samples or randomness are used. The synthetic ZIP generator below uses
fixed OHLCV/sign flags, consecutive IDs starting at 2**53+1, and UTC day offsets.
"""

from contextlib import contextmanager
import csv
from datetime import date
from decimal import Decimal
import doctest
import fcntl
from functools import partial
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog
import pyarrow.parquet as pq

from fertile_finance.epoch import EPOCH
from fertile_finance.historical_crypto import (
    BinanceDisabled,
    CRYPTO_INTERVAL_MS,
    CryptoBars,
    CryptoHTTPError,
    TradeImportReport,
    import_binance_trades,
)
from fertile_finance.historical_pull import CryptoHistory, main, pull_bars


MINUTE = CRYPTO_INTERVAL_MS["1m"]
DAY = CRYPTO_INTERVAL_MS["1d"]
EXPECTED_COINS = ("BTC", "ETH", "SOL")
OLD_DAY = date(2024, 12, 31)
NEW_DAY = date(2025, 1, 1)
NEXT_DAY = date(2025, 1, 2)
START = (OLD_DAY - EPOCH).days * DAY
OHLCV = ["41.1234567890123456", "42.000", "40.0100", "41.500", "0.0000012500"]


class ReplayResponse(io.BytesIO):
    def __init__(self, status, payload):
        super().__init__(payload)
        self.status = status


class HTTPReplay:
    def __init__(self, replies=()):
        self.replies = list(replies)
        self.calls = []

    @contextmanager
    def __call__(self, method, url, body=None):
        self.calls.append((method, url, body))
        if not self.replies:
            raise AssertionError("unexpected HTTP request in replay")
        status, payload = self.replies.pop(0)
        with ReplayResponse(status, payload) as response:
            yield response


def candle_hl(symbol, opened, interval="1m", *, end_inclusive=True):
    return dict(zip(
        ("t", "T", "s", "i", "o", "h", "l", "c", "v"),
        (opened, opened + CRYPTO_INTERVAL_MS[interval] - int(end_inclusive), symbol, interval, *OHLCV),
        strict=True,
    ))


def candle_binance(opened, interval="1m"):
    return [opened, *OHLCV, opened + CRYPTO_INTERVAL_MS[interval] - 1, "0.000051875", 1, "0", "0", "0"]


def json_reply(rows):
    return 200, json.dumps(rows).encode("ascii")


def fixture_first_id(day):
    return 2**53 + 1 + 2 * (day - OLD_DAY).days


def archive_fixture(symbol, day, *, fault=None):
    stem = f"{symbol}USDT-trades-{day}"
    timestamp = (day - EPOCH).days * DAY + 7
    if day >= NEW_DAY:
        timestamp = timestamp * 1000 + 123
    first_id = fixture_first_id(day)
    second_id = first_id if fault == "duplicate_id" else first_id + 1
    rows = [
        [first_id, "11.1234567890123456", "0.12345678", "1.370824560000", timestamp, "True", "True"],
        [second_id, "11.000", "0.10000", "1.100000", timestamp, "False", "True"],
    ]
    if fault == "out_of_day":
        rows[-1][4] -= DAY * (1000 if day >= NEW_DAY else 1)
    content = io.StringIO()
    csv.writer(content).writerows(rows)
    archive = io.BytesIO()
    with ZipFile(archive, "w") as zipped:
        zipped.writestr(ZipInfo(stem + ".csv"), content.getvalue(), compress_type=ZIP_DEFLATED)
    raw = archive.getvalue()
    checksum = "0" * 64 if fault == "checksum" else hashlib.sha256(raw).hexdigest()
    return [(200, f"{checksum}  {stem}.zip\n".encode("ascii")), (200, raw)]


class CryptoBarsTest(unittest.TestCase):
    def test_hyperliquid_all_perpetuals_exact_sorted_closed_only(self):
        for symbol in EXPECTED_COINS:
            with self.subTest(symbol=symbol):
                first = candle_hl(symbol, START)
                second = candle_hl(symbol, START + MINUTE, end_inclusive=False)
                still_open = candle_hl(symbol, START + 2 * MINUTE)
                http = HTTPReplay([json_reply([still_open, second, first, second])])
                source = CryptoBars("hyperliquid", http=http)
                rows = source.fetch(symbol, "1m", START, START + 3 * MINUTE, now_ms=START + 2 * MINUTE + 5)
                self.assertEqual(source.page_limit, 5000)
                self.assertEqual(
                    [row["ts_ns"] for row in rows],
                    [(START + step * MINUTE) * 1_000_000 for step in (1, 2)],
                )
                self.assertEqual([rows[0][field] for field in ("open", "high", "low", "close", "volume")], OHLCV)
                self.assertEqual(rows[0]["instrument"], f"{symbol}-USD-PERP.HYPERLIQUID")
                method, url, request = http.calls[0]
                self.assertEqual((method, url), ("POST", "https://api.hyperliquid.xyz/info"))
                self.assertEqual(json.loads(request), {
                    "type": "candleSnapshot",
                    "req": {"coin": symbol, "interval": "1m", "startTime": START, "endTime": START + 2 * MINUTE - 1},
                })

    def test_binance_all_spot_daily_bars_exact_and_utc(self):
        for symbol in EXPECTED_COINS:
            with self.subTest(symbol=symbol):
                http = HTTPReplay([json_reply([candle_binance(START, "1d")])])
                source = CryptoBars("binance", binance_enabled=True, http=http)
                rows = source.fetch(symbol, "1d", START, START + 2 * DAY, now_ms=START + DAY)
                self.assertEqual(rows, [{
                    "instrument": f"{symbol}USDT.BINANCE", "ts_ns": (START + DAY) * 1_000_000,
                    **dict(zip(("open", "high", "low", "close", "volume"), OHLCV, strict=True)),
                }])
                method, url, body = http.calls[0]
                endpoint = urlsplit(url)
                self.assertEqual((method, endpoint.hostname, endpoint.path, body),
                                 ("GET", "data-api.binance.vision", "/api/v3/klines", None))
                self.assertEqual(parse_qs(endpoint.query), {
                    "symbol": [f"{symbol}USDT"], "interval": ["1d"], "startTime": [str(START)],
                    "endTime": [str(START + DAY - 1)], "limit": ["1"],
                })

    def test_binance_pages_cover_explicit_ranges_even_when_payload_has_gaps(self):
        second_page = START + 1000 * MINUTE
        end = second_page + 2 * MINUTE
        http = HTTPReplay([
            json_reply([candle_binance(START + MINUTE)]),
            json_reply([candle_binance(second_page + MINUTE)]),
        ])
        rows = CryptoBars("binance", binance_enabled=True, http=http).fetch(
            "BTC", "1m", START, end, now_ms=end,
        )
        self.assertEqual([row["ts_ns"] for row in rows], [(START + 2 * MINUTE) * 1_000_000, end * 1_000_000])
        queries = [parse_qs(urlsplit(url).query) for _, url, _ in http.calls]
        self.assertEqual(
            [(q["startTime"][0], q["endTime"][0], q["limit"][0]) for q in queries],
            [(str(START), str(second_page - 1), "1000"), (str(second_page), str(end - 1), "2")],
        )

    def test_current_only_range_makes_no_http_request(self):
        http = HTTPReplay()
        rows = CryptoBars("hyperliquid", http=http).fetch("BTC", "1m", START, START + MINUTE, now_ms=START)
        self.assertEqual(rows, [])
        self.assertEqual(http.calls, [])

    def test_conflicting_duplicates_or_floating_json_are_rejected(self):
        exact = candle_hl("BTC", START)
        conflicting = {**exact, "o": "41.3"}
        numeric = {**exact, "o": 41}
        for rows, message in (([exact, conflicting], "conflicting"), ([numeric], "expected a string")):
            with self.subTest(message=message):
                http = HTTPReplay([json_reply(rows)])
                with self.assertRaisesRegex(ValueError, message):
                    CryptoBars("hyperliquid", http=http).fetch(
                        "BTC", "1m", START, START + MINUTE, now_ms=START + MINUTE,
                    )

    def test_binance_disabled_before_any_http_or_archive_io(self):
        http = HTTPReplay()
        with self.assertRaisesRegex(BinanceDisabled, "disabled_by_profile"):
            CryptoBars("binance", http=http).fetch("BTC", "1m", START, START + MINUTE, now_ms=START)
        with self.assertRaisesRegex(BinanceDisabled, "disabled_by_profile"):
            import_binance_trades(Path("would-fail-root-validation"), "BTC", OLD_DAY, OLD_DAY, http=http)
        self.assertEqual(http.calls, [])

    def test_binance_451_reported_without_retries(self):
        http = HTTPReplay([(451, b"discarded synthetic response")])
        with self.assertRaises(CryptoHTTPError) as caught:
            CryptoBars("binance", binance_enabled=True, http=http).fetch(
                "BTC", "1m", START, START + MINUTE, now_ms=START + MINUTE,
            )
        self.assertEqual(caught.exception.status, "region_restricted")
        self.assertEqual(len(http.calls), 1)

    def test_crypto_http_replay_round_trips_through_collector_parquet(self):
        now_ns = (START + 2 * MINUTE + 5) * 1_000_000
        expected_closes = [(START + step * MINUTE) * 1_000_000 for step in (1, 2)]
        fixtures = {
            "hyperliquid": [candle_hl("BTC", START + step * MINUTE) for step in range(3)],
            "binance": [candle_binance(START + step * MINUTE) for step in range(3)],
        }
        with tempfile.TemporaryDirectory(prefix="f11-crypto-bars-") as temporary:
            root = Path(temporary)
            for venue, response in fixtures.items():
                with self.subTest(venue=venue):
                    http = HTTPReplay([json_reply(response)])
                    provider = CryptoBars(venue, binance_enabled=True, http=http)
                    source = CryptoHistory(venue, "1m", provider, now_ns)
                    report = pull_bars(
                        root, source, ("BTC",), START * 1_000_000 + 1, (START + DAY) * 1_000_000 - 1,
                        now_ns=now_ns,
                    )
                    self.assertEqual(report["status"], "complete")
                    instrument_report = report["instruments"][provider.instrument("BTC")]
                    self.assertEqual(instrument_report["watermark_ns"], expected_closes[-1])
                    catalog = ParquetDataCatalog(root / "catalog" / "historical" / venue / "1-MINUTE-LAST")
                    bars = catalog.bars()
                    self.assertEqual([bar.ts_event for bar in bars], expected_closes)
                    self.assertEqual([str(bar.bar_type.instrument_id) for bar in bars], [provider.instrument("BTC")] * 2)
                    self.assertEqual([bar.open.as_decimal() for bar in bars], [Decimal(OHLCV[0])] * 2)
                    self.assertEqual([bar.volume.as_decimal() for bar in bars], [Decimal(OHLCV[-1])] * 2)
                    self.assertEqual(len(http.calls), 1)

    def test_cli_defaults_pull_all_three_coins_for_each_crypto_source(self):
        bar_fixtures = {
            "hyperliquid": [json_reply([candle_hl(coin, START - DAY, "1d")]) for coin in EXPECTED_COINS],
            "binance": [json_reply([candle_binance(START - DAY, "1d")]) for _ in EXPECTED_COINS],
        }
        for source in ("hyperliquid", "binance", "binance-trades"):
            with self.subTest(source=source), tempfile.TemporaryDirectory(prefix="f11-crypto-cli-") as root:
                replies = bar_fixtures.get(source)
                if source == "binance-trades":
                    replies = [reply for coin in EXPECTED_COINS for reply in archive_fixture(coin, OLD_DAY)]
                http = HTTPReplay(replies)
                args = [source, "--allow-download", "--data-root", root,
                        "--start", str(OLD_DAY), "--end", str(OLD_DAY)]
                if source != "binance-trades":
                    args += ["--interval", "1d"]
                if source != "hyperliquid":
                    args += ["--enable-binance"]
                with (
                    patch("fertile_finance.historical_pull.CryptoBars", partial(CryptoBars, http=http)),
                    patch("fertile_finance.historical_pull.import_binance_trades", partial(import_binance_trades, http=http)),
                    patch("sys.stdout", new_callable=io.StringIO) as output,
                ):
                    self.assertEqual(main(args), 0)
                    report = json.loads(output.getvalue())
                expected_instruments = {
                    "hyperliquid": {f"{coin}-USD-PERP.HYPERLIQUID" for coin in EXPECTED_COINS},
                    "binance": {f"{coin}USDT.BINANCE" for coin in EXPECTED_COINS},
                    "binance-trades": set(EXPECTED_COINS),
                }
                self.assertEqual(report["status"], "complete")
                self.assertEqual(set(report["instruments"]), expected_instruments[source])
                self.assertEqual(http.replies, [])


class TradeArchiveTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="f11-crypto-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / "data"

    def partition(self, symbol, day):
        return self.root / "catalog" / "historical" / "binance-spot-trades" / f"{symbol}USDT.BINANCE" / f"{day}.parquet"

    def run_import(self, http, first=OLD_DAY, last=OLD_DAY, symbol="BTC"):
        return import_binance_trades(self.root, symbol, first, last, binance_enabled=True, http=http)

    def test_archive_exact_decimal_native_ids_and_ms_us_transition(self):
        for symbol in EXPECTED_COINS:
            with self.subTest(symbol=symbol):
                http = HTTPReplay([*archive_fixture(symbol, OLD_DAY), *archive_fixture(symbol, NEW_DAY)])
                with patch("fertile_finance.historical_crypto._TRADE_BATCH_ROWS", 1):
                    self.assertEqual(
                        self.run_import(http, last=NEW_DAY, symbol=symbol),
                        TradeImportReport((str(OLD_DAY), str(NEW_DAY)), (), (), 4),
                    )
                for day, subsecond_ns in ((OLD_DAY, 7_000_000), (NEW_DAY, 7_123_000)):
                    with pq.ParquetFile(self.partition(symbol, day)) as persisted:
                        self.assertEqual(persisted.metadata.num_row_groups, 2)
                        rows = persisted.read().to_pylist()
                        metadata = persisted.schema_arrow.metadata
                    self.assertEqual(
                        [row["trade_id"] for row in rows], [str(fixture_first_id(day)), str(fixture_first_id(day) + 1)],
                    )
                    self.assertEqual([row["ts_ns"] for row in rows], [(day - EPOCH).days * DAY * 1_000_000 + subsecond_ns] * 2)
                    self.assertEqual(rows[0]["price"], "11.1234567890123456")
                    self.assertEqual(rows[0]["quantity"], "0.12345678")
                    self.assertEqual(rows[0]["quote_quantity"], "1.370824560000")
                    self.assertEqual([row["is_buyer_maker"] for row in rows], [True, False])
                    self.assertEqual(metadata[b"instrument_id"], f"{symbol}USDT.BINANCE".encode("ascii"))
                    self.assertEqual(metadata[b"utc_day"], str(day).encode("ascii"))
                    self.assertEqual(len(metadata[b"archive_sha256"]), 64)
                self.assertEqual([url.rsplit("/", 1)[1] for method, url, body in http.calls], [
                    f"{symbol}USDT-trades-{day}.zip{suffix}" for day in (OLD_DAY, NEW_DAY) for suffix in (".CHECKSUM", "")
                ])
                empty = HTTPReplay()
                self.assertEqual(
                    self.run_import(empty, last=NEW_DAY, symbol=symbol),
                    TradeImportReport((), (str(OLD_DAY), str(NEW_DAY)), (), 0),
                )
                self.assertEqual(empty.calls, [])

    def test_documented_archive_readback_uses_local_parquet_without_f3_wal(self):
        http = HTTPReplay(archive_fixture("BTC", OLD_DAY))
        self.assertEqual(self.run_import(http).trades_written, 2)
        guide = Path(__file__).resolve().parents[1] / "docs/historical-pull.md"
        _, marker, fenced = guide.read_text(encoding="utf-8").partition("```pycon\n")
        self.assertTrue(marker, "archive guide must include a runnable PyArrow example")
        session = doctest.DocTestParser().get_doctest(
            fenced.split("```", 1)[0], {"archive_path": self.partition("BTC", OLD_DAY)},
            "archive_readback", str(guide), 0,
        )
        result = doctest.DocTestRunner(verbose=False).run(session, clear_globs=False)
        self.assertGreater(result.attempted, 0)
        self.assertEqual(result.failed, 0)
        self.assertEqual(session.globs["instrument_id"], "BTCUSDT.BINANCE")
        rows = session.globs["trades"].to_pylist()
        self.assertEqual([row["trade_id"] for row in rows], [str(2**53 + 1), str(2**53 + 2)])
        self.assertEqual(rows[0]["ts_ns"], rows[1]["ts_ns"])
        for path in ("catalog/market-events", "finance-state/market-wal", "finance-state/market-projection"):
            self.assertFalse((self.root / path).exists(), path)

    def test_archive_fills_missing_middle_on_resume_without_refetching_neighbors(self):
        http = HTTPReplay([*archive_fixture("BTC", OLD_DAY), (404, b""), *archive_fixture("BTC", NEXT_DAY)])
        self.assertEqual(
            self.run_import(http, last=NEXT_DAY),
            TradeImportReport((str(OLD_DAY), str(NEXT_DAY)), (), (str(NEW_DAY),), 4),
        )
        self.assertFalse(self.partition("BTC", NEW_DAY).exists())
        resume = HTTPReplay(archive_fixture("BTC", NEW_DAY))
        self.assertEqual(
            self.run_import(resume, last=NEXT_DAY),
            TradeImportReport((str(NEW_DAY),), (str(OLD_DAY), str(NEXT_DAY)), (), 2),
        )
        self.assertEqual(len(resume.calls), 2)

    def test_archive_interruption_preserves_prior_day_and_never_publishes_partial_partition(self):
        http = HTTPReplay([*archive_fixture("BTC", OLD_DAY), *archive_fixture("BTC", NEW_DAY, fault="duplicate_id")])
        with patch("fertile_finance.historical_crypto._TRADE_BATCH_ROWS", 1):
            with self.assertRaisesRegex(ValueError, "trade IDs must strictly increase"):
                self.run_import(http, last=NEW_DAY)
        self.assertTrue(self.partition("BTC", OLD_DAY).exists())
        self.assertFalse(self.partition("BTC", NEW_DAY).exists())
        self.assertFalse((self.partition("BTC", NEW_DAY).parent / ".staging").exists())
        self.assertEqual(
            self.run_import(HTTPReplay(archive_fixture("BTC", NEW_DAY)), last=NEW_DAY),
            TradeImportReport((str(NEW_DAY),), (str(OLD_DAY),), (), 2),
        )

    def test_archive_checksum_and_wrong_timestamp_prevent_publication(self):
        for fault, message in (("checksum", "SHA256 mismatch"), ("out_of_day", "outside archive day")):
            with self.subTest(fault=fault):
                with self.assertRaisesRegex(ValueError, message):
                    self.run_import(HTTPReplay(archive_fixture("BTC", OLD_DAY, fault=fault)))
                self.assertFalse(self.partition("BTC", OLD_DAY).exists())

    def test_archive_451_is_not_treated_as_missing(self):
        http = HTTPReplay([(451, b"")])
        with self.assertRaises(CryptoHTTPError) as caught:
            self.run_import(http, last=NEW_DAY)
        self.assertEqual(caught.exception.status, "region_restricted")
        self.assertEqual(len(http.calls), 1)

    def test_archive_rejects_checkout_root_and_symlink_before_http(self):
        http = HTTPReplay()
        checkout = Path(__file__).resolve().parents[1]
        with self.assertRaisesRegex(ValueError, "outside a Git working tree"):
            import_binance_trades(checkout, "BTC", OLD_DAY, OLD_DAY, binance_enabled=True, http=http)
        self.root.mkdir()
        (self.root / "catalog").symlink_to(checkout, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "inside the configured data root"):
            self.run_import(http)
        self.assertEqual(http.calls, [])

    def test_archive_concurrent_writer_lock_precedes_http(self):
        lock_path = self.partition("BTC", OLD_DAY).parent / ".writer.lock"
        lock_path.parent.mkdir(parents=True)
        http = HTTPReplay()
        with lock_path.open("ab") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(BlockingIOError):
                self.run_import(http)
        self.assertEqual(http.calls, [])
