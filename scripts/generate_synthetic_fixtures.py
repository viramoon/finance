"""Reproduce the small, hand-designed finance fixtures without external data."""

import argparse
import json
from pathlib import Path


def rows(*records):
    return "".join(",".join(record) + "\n" for record in records)


def french(description, *records):
    return (
        "This file was created by the synthetic fixture using a sample daily archive format.\n"
        f"{description}\n\n"
        + rows(*records)
        + "\nCopyright notice placeholder for synthetic fixture\n"
    )


def fred_observations():
    vintage = "2024-07-10"
    fields = {
        "realtime_start": vintage,
        "realtime_end": vintage,
        "observation_start": "2024-07-03",
        "observation_end": "2024-07-05",
        "units": "lin",
        "output_type": 1,
        "file_type": "json",
        "order_by": "observation_date",
        "sort_order": "asc",
        "count": 3,
        "offset": 0,
        "limit": 100000,
    }
    series = [
        json.dumps({"realtime_start": vintage, "realtime_end": vintage, "date": day, "value": value})
        for day, value in (
            ("2024-07-03", "4.12345678901234567890123456789"),
            ("2024-07-04", "."),
            ("2024-07-05", "4.3900"),
        )
    ]
    return json.dumps(fields, indent=2)[:-2] + ',\n  "observations": [\n    ' + ",\n    ".join(series) + "\n  ]\n}\n"


def yahoo(symbol, exchange, full_exchange, asset, timestamps, quote, adjusted_close, events=""):
    # The numeric tokens are authored as strings to reproduce the producer's
    # JSON number lexemes exactly, including scale and nulls.
    prices = ",\n".join(f'              "{key}": [{value}]' for key, value in quote.items())
    return f'''{{
  "chart": {{
    "result": [
      {{
        "meta": {{
          "currency": "USD",
          "symbol": "{symbol}",
          "exchangeName": "{exchange}",
          "fullExchangeName": "{full_exchange}",
          "instrumentType": "{asset}",
          "exchangeTimezoneName": "America/New_York",
          "dataGranularity": "1d"
        }},
        "timestamp": [{timestamps}],
        "indicators": {{
          "quote": [
            {{
{prices}
            }}
          ],
          "adjclose": [
            {{"adjclose": [{adjusted_close}]}}
          ]
        }}{events}
      }}
    ],
    "error": null
  }}
}}
'''


def fixtures():
    ibm_events = ''',
        "events": {
          "dividends": {
            "1704292200": {"amount": 0.212345678901234567, "date": 1704292200},
            "1733149800": {"amount": 0.375, "date": 1733149800}
          },
          "splits": {
            "1720013400": {"date": 1720013400, "numerator": 2.0, "denominator": 1.0, "splitRatio": "2:1"}
          }
        }'''
    historical = {
        "french_daily.csv": french(
            "The RF column shows a daily risk-free return in percent.",
            ("", "Mkt-RF", "SMB", "HML", "RF"),
            ("19691231", "    0.50", "   -0.02", "    0.40", "    0.02"),
            ("19700102", "   -0.25", "    0.01", "   -0.02", "    0.03"),
            ("20240703", "    1.23", "   -0.23", "    0.00", "    0.02"),
            ("20240705", "    0.02", "  -99.99", "    0.04", " -999.00"),
        ),
        "french_momentum_daily.csv": french(
            "It contains a daily momentum return in percent.",
            ("", "Mom"),
            ("19700102", "    0.11"),
            ("20240703", "   -0.17"),
            ("20240705", "  -99.99"),
            ("20240708", "    0.33"),
        ),
        "fred_dgs10_daily.csv": rows(
            ("observation_date", "DGS10"),
            ("1969-12-31", "7.50"),
            ("1970-01-02", "7.23"),
            ("2024-07-03", "4.36"),
            ("2024-07-04", "."),
        ),
        "fred_dgs10_observations.json": fred_observations(),
        "yahoo_gspc.json": yahoo(
            "^GSPC", "SNP", "SNP", "INDEX", "-34200, 138600, 397800",
            {"open": "null, 0, 95.05", "high": "null, null, 96.25",
             "low": "null, null, 94.50", "close": "null, 95.10, 95.75", "volume": "null, null, 0"},
            "null, null, 95.75",
        ),
        "yahoo_ibm.json": yahoo(
            "IBM", "NYQ", "NYSE", "EQUITY", "1704292200, 1719927000, 1720013400, 1732890600",
            {"open": "148.1234567890123456, 149, 75.40, 77.35", "high": "153.35, 151, 78.15, 80.45",
             "low": "146.25, 147, 74.60, 76.20", "close": "150.1234567890123456, 150, 77.50, 79.35",
             "volume": "12345678, 23456789, 34567890, 0"},
            "75.0617283945061728, 100, 76.25, 79.35", ibm_events,
        ),
    }
    return {
        **{f"tests/fixtures/historical/{name}": text for name, text in historical.items()},
        "config/collector/replay.csv": rows(
            ("open", "high", "low", "close", "volume"),
            ("591.123456789", "591.220000000", "591.010000000", "591.131234567", "153"),
            ("591.131234567", "591.330000000", "591.110000000", "591.240000000", "272"),
            ("591.240000000", "591.250000000", "591.070000000", "591.111111111", "119"),
        ),
        "tests/fixtures/factor_returns.csv": rows(
            ("date", "Mkt-RF", "SMB", "HML", "Mom", "RF"),
            ("2024-01-02", "-0.030", "0.010", "0.006", "0.001", "0.0001"),
            ("2024-01-03", "-0.020", "0.000", "-0.003", "0.004", "0.0001"),
            ("2024-01-04", "-0.010", "0.003", "0.005", "-0.002", "0.0001"),
            ("2024-01-05", "0.000", "0.006", "0.001", "0.003", "0.0001"),
            ("2024-01-08", "0.010", "-0.003", "0.008", "-0.005", "0.0001"),
            ("2024-01-09", "0.020", "-0.008", "0.002", "0.002", "0.0001"),
            ("2024-01-10", "0.030", "0.004", "-0.001", "0.006", "0.0001"),
            ("2024-01-11", "0.040", "0.001", "-0.006", "-0.004", "0.0001"),
            ("2024-01-12", "0.050", "-0.004", "-0.005", "0.000", "0.0001"),
        ),
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    output = parser.parse_args().output
    for name, text in fixtures().items():
        path = output / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(text.encode("utf-8"))
