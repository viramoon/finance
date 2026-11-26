from contextlib import redirect_stderr, redirect_stdout
from datetime import date, datetime, timezone
from functools import partial
from io import StringIO
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, call, patch
from urllib.parse import urlsplit

from fertile_finance import download_yahoo_daily as downloader
from fertile_finance.yahoo_daily import load_yahoo_daily, yahoo_chart_url


FIXTURE = Path(__file__).parent / "fixtures" / "historical" / "yahoo_ibm.json"
START = date(2024, 1, 1)
END = date(2024, 12, 31)
QUERY1 = "query1.finance.yahoo.com"
QUERY2 = "query2.finance.yahoo.com"


def response(status=200, payload=None, **headers):
    reply = Mock(status=status)
    reply.getheader.side_effect = headers.get
    reply.read.return_value = FIXTURE.read_bytes() if payload is None else payload
    return reply


class DownloadYahooDailyTests(unittest.TestCase):
    def setUp(self):
        directory = self.enterContext(tempfile.TemporaryDirectory(dir="/tmp"))
        self.output = Path(directory) / "yahoo-IBM.json"
        self.sleep = Mock()
        self.network = self.enterContext(patch.object(downloader, "HTTPSConnection"))
        self.now = lambda: datetime(2024, 12, 31, 12, 0, 0, tzinfo=timezone.utc)

    def plan(self, *results):
        connections = []
        for result in results:
            connection = Mock()
            if isinstance(result, Exception):
                connection.getresponse.side_effect = result
            else:
                connection.getresponse.return_value = result
            connections.append(connection)
        self.network.side_effect = connections
        return connections

    def fetch(self, **kwargs):
        return downloader.download_yahoo_daily(
            "IBM", START, END, self.output, sleep=self.sleep, now=self.now, **kwargs,
        )

    def cli(self):
        stdout, stderr = StringIO(), StringIO()
        download = partial(downloader.download_yahoo_daily, sleep=self.sleep, now=self.now)
        args = ["--symbol", "IBM", "--start", str(START), "--end", str(END), "--output", str(self.output)]
        with patch.object(downloader, "download_yahoo_daily", download), redirect_stdout(stdout), redirect_stderr(stderr):
            try:
                downloader.main(args)
            except SystemExit as exc:
                return exc.code, stdout.getvalue(), stderr.getvalue()
        return 0, stdout.getvalue(), stderr.getvalue()

    def test_success_cli_preserves_raw_bytes_and_reports_primary_url(self):
        reply = response()
        (connection,) = self.plan(reply)
        code, stdout, stderr = self.cli()
        self.assertEqual((code, stderr), (0, ""))
        report = json.loads(stdout)
        url = yahoo_chart_url("IBM", START, END)
        self.assertEqual(report["url"], url)
        self.assertEqual(report["output"], str(self.output))
        self.assertEqual(self.output.read_bytes(), FIXTURE.read_bytes())
        self.assertEqual(report["bytes"], self.output.stat().st_size)
        self.assertEqual(load_yahoo_daily(self.output, adjustment_as_of=END).symbol, "IBM")
        endpoint = urlsplit(url)
        self.network.assert_called_once_with(QUERY1, timeout=15)
        connection.request.assert_called_once_with("GET", f"{endpoint.path}?{endpoint.query}", headers={
            "User-Agent": "fertile-finance/0.0.0 (historical daily archive)", "Accept": "application/json",
        })
        connection.close.assert_called_once_with()
        self.sleep.assert_not_called()

    def test_429_seconds_waits_before_trying_backup_with_user_agent_override(self):
        limited = response(429, **{"Retry-After": "5", "Date": "Tue, 31 Dec 2024 12:00:00 GMT"})
        first, second = self.plan(limited, response())
        self.sleep.side_effect = lambda _: self.network.assert_called_once_with(QUERY1, timeout=15)
        report = self.fetch(user_agent="Approved archive operator/1.0")
        self.sleep.assert_called_once_with(5)
        self.assertEqual(self.network.call_args_list, [call(QUERY1, timeout=15), call(QUERY2, timeout=15)])
        self.assertEqual(urlsplit(report["url"]).hostname, QUERY2)
        self.assertEqual(first.request.call_args.args, second.request.call_args.args)
        for connection in (first, second):
            self.assertEqual(connection.request.call_args.kwargs["headers"]["User-Agent"],
                             "Approved archive operator/1.0")
            connection.close.assert_called_once_with()
        self.assertIn("HTTP 429 [Retry-After: 5] [Date: Tue, 31 Dec 2024 12:00:00 GMT]", report["attempts"][0])
        limited.read.assert_not_called()

    def test_429_respects_retry_after_http_date(self):
        retry_after = "Tue, 31 Dec 2024 12:00:09 GMT"
        self.plan(response(429, **{"Retry-After": retry_after}), response())
        report = self.fetch()
        self.sleep.assert_called_once_with(9)
        self.assertIn(retry_after, report["attempts"][0])

    def test_429_exhaustion_exits_nonzero_with_sanitized_attempted_urls(self):
        limited = response(429, b"PRIVATE ERROR PAGE", **{
            "Retry-After": "PRIVATE INVALID HEADER", "Date": "PRIVATE INVALID DATE", "Set-Cookie": "PRIVATE COOKIE",
        })
        connections = self.plan(limited, limited, limited, limited)
        code, stdout, stderr = self.cli()
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("attempts exhausted", stderr)
        self.assertIn(QUERY1, stderr)
        self.assertIn(QUERY2, stderr)
        self.assertEqual(stderr.count("HTTP 429"), 4)
        self.assertNotIn("PRIVATE", stderr)
        self.assertEqual(self.sleep.call_args_list, [call(1), call(2), call(4)])
        self.assertEqual([item.args[0] for item in self.network.call_args_list], [QUERY1, QUERY2, QUERY1, QUERY2])
        limited.read.assert_not_called()
        for connection in connections:
            connection.close.assert_called_once_with()
        self.assertFalse(self.output.exists())

    def test_retry_after_beyond_remaining_wait_budget_never_retries_early(self):
        self.plan(response(429, **{"Retry-After": "20"}), response(429, **{"Retry-After": "11"}))
        with self.assertRaisesRegex(downloader.DownloadError, "30-second budget") as failed:
            self.fetch()
        self.assertIn("HTTP 429 [Retry-After: 11]", str(failed.exception))
        self.sleep.assert_called_once_with(20)
        self.assertEqual(self.network.call_count, 2)
        self.assertFalse(self.output.exists())

    def test_primary_transport_failure_retries_backup(self):
        first, second = self.plan(TimeoutError("private exception details"), response())
        report = self.fetch()
        self.assertEqual(urlsplit(report["url"]).hostname, QUERY2)
        self.assertIn("transport error (TimeoutError)", report["attempts"][0])
        self.assertNotIn("private exception details", report["attempts"][0])
        self.sleep.assert_called_once_with(1)
        first.close.assert_called_once_with()
        second.close.assert_called_once_with()

    def test_redirect_is_terminal_and_does_not_send_a_second_request(self):
        redirected = response(302, b"private page", Location="https://unlisted.example/private")
        self.plan(redirected)
        code, stdout, stderr = self.cli()
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("HTTP 302", stderr)
        self.assertNotIn("unlisted.example", stderr)
        self.network.assert_called_once_with(QUERY1, timeout=15)
        self.sleep.assert_not_called()
        redirected.read.assert_not_called()
        self.assertFalse(self.output.exists())

    def test_invalid_http_200_never_publishes_error_page(self):
        self.plan(response(200, b"<html>PRIVATE ERROR PAGE</html>"))
        code, stdout, stderr = self.cli()
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("HTTP 200", stderr)
        self.assertIn("expected a nonempty Yahoo daily chart", stderr)
        self.assertNotIn("PRIVATE ERROR PAGE", stderr)
        self.assertFalse(self.output.exists())

    def test_refuses_existing_archive_without_network_access(self):
        self.output.write_bytes(b"original")
        code, stdout, stderr = self.cli()
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("Archive already exists", stderr)
        self.assertEqual(self.output.read_bytes(), b"original")
        self.network.assert_not_called()


if __name__ == "__main__":
    unittest.main()
