"""Download one Yahoo Finance daily chart response to a local archive."""

import argparse
from datetime import date, datetime, timezone
from decimal import Decimal
from email.utils import format_datetime, parsedate_to_datetime
from http.client import HTTPException, HTTPSConnection
import json
from math import ceil
import os
from pathlib import Path
import re
from tempfile import NamedTemporaryFile
import time
from urllib.parse import urlsplit, urlunsplit

from .yahoo_daily import _invalid_constant, _unique_object, yahoo_chart_url


DEFAULT_USER_AGENT = "fertile-finance/0.0.0 (historical daily archive)"
MAX_ATTEMPTS = 4
MAX_WAIT_SECONDS = 30
HTTP_TIMEOUT_SECONDS = 15
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_NOT_FOUND_BYTES = 4 * 1024
_QUOTE_FIELDS = ("open", "high", "low", "close", "volume")


class DownloadError(ValueError):
    """A download failed before an archive could be published."""


def _utc_now():
    return datetime.now(timezone.utc)


def _http_date(value):
    if not value:
        return None
    try:
        parsed = parsedate_to_datetime(value)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.astimezone(timezone.utc)
    except (ValueError, OverflowError):
        return None


def _retry_after(value, now):
    value = (value or "").strip()
    if re.fullmatch(r"[0-9]+", value):
        # Normalize the header before logging it or converting it to an integer.
        digits = value.lstrip("0") or "0"
        if len(digits) > 9:
            return MAX_WAIT_SECONDS + 1, "above wait budget"
        return int(digits), digits
    if (when := _http_date(value)) is not None:
        return max(0, ceil((when - now()).total_seconds())), format_datetime(when, usegmt=True)
    return 0, None


def _fetch(url, user_agent, *, read_not_found=False):
    endpoint = urlsplit(url)
    connection = HTTPSConnection(endpoint.hostname, timeout=HTTP_TIMEOUT_SECONDS)
    try:
        connection.request(
            "GET",
            f"{endpoint.path}?{endpoint.query}",
            headers={"User-Agent": user_agent, "Accept": "application/json"},
        )
        response = connection.getresponse()
        # HTTPSConnection does not follow redirects. History inspects only bounded 404 bodies.
        data = None
        if response.status == 200:
            data = response.read(MAX_RESPONSE_BYTES + 1)
        elif read_not_found and response.status == 404:
            data = response.read(MAX_NOT_FOUND_BYTES + 1)
        return response.status, response.getheader("Retry-After"), response.getheader("Date"), data
    finally:
        connection.close()


def _validate_chart(raw, symbol):
    if len(raw) > MAX_RESPONSE_BYTES:
        raise DownloadError("Yahoo chart response exceeds 16 MiB")
    try:
        chart = json.loads(
            raw, parse_float=Decimal, parse_constant=_invalid_constant, object_pairs_hook=_unique_object,
        )["chart"]
        (result,) = chart["result"]
        meta = result["meta"]
        timestamps = result["timestamp"]
        (quote,) = result["indicators"]["quote"]
        if (
            chart.get("error") is None and meta["symbol"] == symbol and meta["dataGranularity"] == "1d"
            and isinstance(timestamps, list) and timestamps and isinstance(quote, dict)
            and all(isinstance(quote.get(field), list) and len(quote[field]) == len(timestamps)
                    for field in _QUOTE_FIELDS)
        ):
            return
    except (AttributeError, KeyError, TypeError, ValueError):
        pass
    raise DownloadError("expected a nonempty Yahoo daily chart for the requested symbol")


def _is_empty_chart(raw, status, symbol):
    limit = MAX_NOT_FOUND_BYTES if status == 404 else MAX_RESPONSE_BYTES
    if len(raw) > limit:
        return False
    try:
        chart = json.loads(raw, parse_float=Decimal, parse_constant=_invalid_constant,
                           object_pairs_hook=_unique_object)["chart"]
        error = chart["error"]
        if chart["result"] is None and error["code"] == "Not Found":
            detail = error["description"]
            return detail == "No data found, symbol may be delisted" or bool(re.fullmatch(
                r"Data doesn't exist for startDate = [0-9]{1,12}, endDate = [0-9]{1,12}", detail,
            ))
        if status == 200 and error is None:
            (result,) = chart["result"]
            meta = result["meta"]
            (quote,) = result["indicators"]["quote"]
            return (meta["symbol"] == symbol and meta["dataGranularity"] == "1d"
                    and meta["exchangeTimezoneName"] == "America/New_York" and result["timestamp"] == []
                    and (quote == {} or quote == {field: [] for field in _QUOTE_FIELDS}))
    except (KeyError, TypeError, ValueError):
        pass
    return False


def _publish(output, raw):
    with NamedTemporaryFile("wb", dir=output.parent, prefix=".yahoo-", delete=False) as handle:
        staged = Path(handle.name)
        try:
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
            # A concurrent download must not replace an existing archive.
            os.link(staged, output)
        finally:
            staged.unlink()
    directory = os.open(output.parent, os.O_DIRECTORY | os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def fetch_yahoo_daily(
    symbol, start, end, *, user_agent=DEFAULT_USER_AGENT, sleep=time.sleep, now=_utc_now, allow_empty=False,
):
    """Return response bytes and diagnostics, or None for a recognized empty history window."""
    primary = urlsplit(yahoo_chart_url(symbol, start, end))
    urls = (primary.geturl(), urlunsplit(primary._replace(netloc="query2.finance.yahoo.com")))
    if not re.fullmatch(r"[ -~]{1,256}", user_agent) or not user_agent.strip():
        raise ValueError("User-Agent must contain 1–256 printable ASCII characters")

    attempts = []
    waited = 0
    for attempt in range(MAX_ATTEMPTS):
        url = urls[attempt % len(urls)]
        delay = 0
        try:
            status, retry_after, response_date, raw = _fetch(url, user_agent, read_not_found=allow_empty)
        except (OSError, HTTPException) as exc:
            attempts.append(f"{url}: transport error ({type(exc).__name__})")
        else:
            if status == 200:
                attempts.append(f"{url}: HTTP 200")
                try:
                    _validate_chart(raw, symbol)
                except DownloadError as exc:
                    if allow_empty and _is_empty_chart(raw, status, symbol):
                        return None, {"url": url, "bytes": len(raw), "attempts": attempts}
                    raise DownloadError(f"{exc}\n" + "\n".join(attempts)) from None
                return raw, {"url": url, "bytes": len(raw), "attempts": attempts}

            delay, safe_retry_after = _retry_after(retry_after, now)
            diagnostic = f"{url}: HTTP {status}"
            if safe_retry_after is not None:
                diagnostic += f" [Retry-After: {safe_retry_after}]"
            if (received := _http_date(response_date)) is not None:
                diagnostic += f" [Date: {format_datetime(received, usegmt=True)}]"
            attempts.append(diagnostic)
            if allow_empty and status == 404 and _is_empty_chart(raw, status, symbol):
                return None, {"url": url, "bytes": len(raw), "attempts": attempts}
            if status not in (408, 429) and not 500 <= status <= 599:
                raise DownloadError("Yahoo download rejected\n" + "\n".join(attempts))

        if attempt + 1 == MAX_ATTEMPTS:
            break
        wait = max(2**attempt, delay)
        if wait > MAX_WAIT_SECONDS - waited:
            raise DownloadError("Yahoo retry wait would exceed the 30-second budget\n" + "\n".join(attempts))
        sleep(wait)
        waited += wait
    raise DownloadError("Yahoo download attempts exhausted\n" + "\n".join(attempts))


def download_yahoo_daily(
    symbol, start, end, output, *, user_agent=DEFAULT_USER_AGENT, sleep=time.sleep, now=_utc_now,
):
    """Validate and publish a new raw archive, returning the attempted endpoints."""
    if output.exists() or output.is_symlink():
        raise FileExistsError(f"Archive already exists: {output}")
    if not output.parent.is_dir():
        raise FileNotFoundError(f"Archive directory does not exist: {output.parent}")
    raw, report = fetch_yahoo_daily(symbol, start, end, user_agent=user_agent, sleep=sleep, now=now)
    _publish(output, raw)
    return {**report, "output": str(output)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--symbol", required=True, help="Yahoo symbol, such as IBM or SPY")
    parser.add_argument("--start", type=date.fromisoformat, required=True, metavar="YYYY-MM-DD",
                        help="First New York date, inclusive")
    parser.add_argument("--end", type=date.fromisoformat, required=True, metavar="YYYY-MM-DD",
                        help="Last New York date, inclusive")
    parser.add_argument("--output", type=Path, required=True, help="New archive file in an existing directory")
    parser.add_argument("--user-agent", default=DEFAULT_USER_AGENT, help="HTTP User-Agent (default: %(default)s)")
    args = parser.parse_args(argv)
    try:
        result = download_yahoo_daily(args.symbol, args.start, args.end, args.output, user_agent=args.user_agent)
    except (ValueError, OSError) as exc:
        parser.exit(1, f"{exc}\n")
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
