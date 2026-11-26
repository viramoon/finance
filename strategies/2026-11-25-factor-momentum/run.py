"""Reproduce the issue #21 research, reports, and compressed audit ledgers."""

import argparse
from bisect import bisect_left, bisect_right
import csv
from datetime import date
from decimal import Decimal, localcontext
from gzip import GzipFile
from hashlib import sha256
from importlib.metadata import version
from io import TextIOWrapper
import json
from pathlib import Path
import subprocess

from fertile_finance.research_decimal import RESEARCH_CONTEXT
from inputs import load_comparisons, load_french_snapshot
from ledger import simulate, write_audit
from metrics import REGRESSION_FACTORS, correlations, factor_regression, flip_rows, flip_summary, window_row, window_rows
from signals import CORE, MAIN_FACTORS, VARIANTS, build_plans, close_bounds

ROOT = Path(__file__).resolve().parent
COSTS = (0, 10, 25, 50)
PRIMARY_START = date(1970, 1, 31)
SUPPLEMENT_START, SUPPLEMENT_END = date(1964, 7, 31), date(1969, 12, 31)
OOS = {
    "oos_jf_2022": date(2020, 1, 31),
    "oos_nber_2019": date(2016, 1, 31),
}
SEGMENTS = {
    "1973_1974": (date(1973, 1, 31), date(1974, 12, 31)),
    "2000_2002": (date(2000, 1, 31), date(2002, 12, 31)),
    "2008_2009": (date(2008, 1, 31), date(2009, 12, 31)),
    "2009_rebound": (date(2009, 4, 30), date(2009, 12, 31)),
    "2017_2020_value_drawdown": (date(2017, 1, 31), date(2020, 12, 31)),
    "2020_03": (date(2020, 3, 31), date(2020, 3, 31)),
    "2022": (date(2022, 1, 31), date(2022, 12, 31)),
}


def _code_version(ref):
    repo = ROOT.parents[1]
    package = repo / "python/fertile_finance"
    files = [*sorted(ROOT.glob("*.py")), package / "research_windows.py", package / "research_decimal.py"]
    relative = [str(path.relative_to(repo)) for path in files]
    commit = subprocess.check_output(
        ["git", "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}"], cwd=repo, text=True
    ).strip()
    # Refuse to attach a commit to a report produced from differing source bytes.
    for path, name in zip(files, relative, strict=True):
        committed = subprocess.check_output(["git", "show", f"{commit}:{name}"], cwd=repo)
        if path.read_bytes() != committed:
            raise ValueError(f"Research source differs from {commit}: {name}")
    return {"commit": commit, "files_sha256": {
        name: sha256(path.read_bytes()).hexdigest() for path, name in zip(files, relative, strict=True)
    }}


def _columns(rows, names):
    return {name: {month: values.get(name) for month, values in rows.items()} for name in names}


def _count_daily(daily, coverage):
    first, last = coverage["actual_start"], coverage["actual_end"]
    selected = ()
    if first and last:
        first_session = date.fromisoformat(first).replace(day=1)
        last_session = date.fromisoformat(last)
        selected = daily[bisect_left(daily, first_session):bisect_right(daily, last_session)]
    return {
        "source_daily_ff5_mom_rf_first": selected[0].isoformat() if selected else None,
        "source_daily_ff5_mom_rf_last": selected[-1].isoformat() if selected else None,
        "source_daily_ff5_mom_rf_observations": len(selected),
        "catalog_observations": 0,
        "engine_bars": 0,
        "catalog_engine_status": "not_run",
    }


def _verdict(windows):
    comparisons = {}
    for name in ("longest_sample", *OOS):
        left = windows["main", 25][name]["metrics"]
        right = windows["static", 25][name]["metrics"]
        main, static = [None if row is None else row["sharpe_precise"] for row in (left, right)]
        with localcontext(RESEARCH_CONTEXT):
            difference = Decimal(main) - Decimal(static) if main is not None and static is not None else None
        comparisons[name] = {
            "main_sharpe": main,
            "static_sharpe": static,
            "difference": difference,
            "outperformance": difference > 0 if difference is not None else None,
            "flip": difference <= 0 if difference is not None and name in OOS else None,
        }
    passed = all(row["outperformance"] is True for row in comparisons.values())
    return {
        "verdict": "值得继续" if passed else "不通过",
        "passed": passed,
        "cost_bps_each_traded_dollar_both_strategies": 25,
        "rule": "strictly positive Sharpe spread in longest_sample and both OOS; OOS spread <= 0 is a flip",
        "comparisons": comparisons,
    }


def _csv_rows(stream, records):
    if not records:
        raise ValueError("No rows to write")
    writer = csv.DictWriter(stream, fieldnames=records[0].keys(), lineterminator="\n")
    writer.writeheader()
    writer.writerows(records)


def _write_csv(path, records):
    with path.open("w", newline="", encoding="utf-8") as stream:
        _csv_rows(stream, records)


def _write_gzip_csv(path, records):
    with path.open("wb") as raw, GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as compressed:
        with TextIOWrapper(compressed, encoding="utf-8", newline="") as stream:
            _csv_rows(stream, records)


def _flat_windows(windows):
    for (name, cost), rows in windows.items():
        for row in rows.values():
            data = {key: value for key, value in row.items() if key != "metrics"}
            if row["metrics"]:
                data.update({key: value for key, value in row["metrics"].items() if key != "factor_weights"})
            yield {"strategy": name, "cost_bps": cost, **data}


def _write_windows(path, windows):
    rows = list(_flat_windows(windows))
    columns = dict.fromkeys(key for row in rows for key in row)
    _write_csv(path, [{key: row.get(key) for key in columns} for row in rows])


def _write_json(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False, allow_nan=False, default=str) + "\n",
                    encoding="utf-8")


def run(raw, comparisons_dir, output, code_commit):
    code = _code_version(code_commit)
    monthly, daily, french_source = load_french_snapshot(raw)
    comparators, comparator_source = load_comparisons(comparisons_dir)
    end = max(day for day, row in monthly.items() if all(row.get(name) is not None
                                                        for name in (*REGRESSION_FACTORS, "RF")))
    timing = close_bounds(daily, date(1963, 7, 1), end)
    daily_common = sorted(day for day, row in daily.items()
                          if all(row.get(name) is not None for name in (*REGRESSION_FACTORS, "RF")))
    references = _columns(monthly, REGRESSION_FACTORS) | _columns(comparators, ("TSMOM12", "vol_managed", "faber_sma10"))
    rf = _columns(monthly, ("RF",))["RF"]

    simulations, windows, supplement, segments = {}, {}, {}, {}
    correlation_rows, regression_rows, flips, flip_aggregates = [], [], [], {}
    monthly_rows, weight_rows, signal_samples = [], [], []
    for name, factors in (CORE | VARIANTS).items():
        plans = build_plans(monthly, timing, name, start=PRIMARY_START, end=end)
        early_plans = build_plans(monthly, timing, name, start=SUPPLEMENT_START, end=SUPPLEMENT_END)
        for bps in COSTS:
            key = name, bps
            simulation = simulations[key] = simulate(plans, monthly, cost_bps=bps)
            observations = {row.month: row for row in simulation.months}
            rows = windows[key] = window_rows(observations, end, factors)
            rows.update({label: window_row(observations, label, start, end, factors) for label, start in OOS.items()})
            for coverage in rows.values():
                coverage.update(_count_daily(daily_common, coverage))
                if coverage["metrics"]:
                    for factor, weights in coverage["metrics"]["factor_weights"].items():
                        weight_rows.append({"strategy": name, "cost_bps": bps, "window": coverage["window"],
                                            "factor": factor, **weights})

            earlier = simulations[f"pre1970_{name}", bps] = simulate(early_plans, monthly, cost_bps=bps)
            early_observations = {row.month: row for row in earlier.months}
            early_row = window_row(early_observations, "pre1970", min(early_observations, default=SUPPLEMENT_START),
                                   SUPPLEMENT_END, factors)
            early_row.update(_count_daily(daily_common, early_row))
            supplement[key] = {"pre1970": early_row}

            for row in simulation.months:
                monthly_rows.append({
                    "strategy": name, "cost_bps": bps, "month": row.month, "signal_ts": row.signal_ts.isoformat(),
                    "fill_ts": row.fill_ts.isoformat(), "nav_open": row.nav_open, "nav_close": row.nav_close,
                    "net_return": row.net_return, "rf": row.rf, "turnover": row.turnover,
                    "gross_exposure": row.gross_exposure, "net_exposure": row.net_exposure,
                    "factor_pnl": row.factor_pnl, "rf_interest": row.rf_interest,
                    "financing_cost": row.financing_cost, "cost": row.cost,
                })
            if bps == 25:
                endpoints = simulation.months[:1]
                if len(simulation.months) > 1:
                    endpoints += simulation.months[-1:]
                signal_samples.extend({
                    "strategy": name, "month": row.month, "signal_ts": row.signal_ts.isoformat(),
                    "fill_ts": row.fill_ts.isoformat(), "weights": row.weights,
                } for row in endpoints)
            if name in CORE:
                segments[key] = {
                    label: window_row(observations, label, start, finish, factors)
                    for label, (start, finish) in SEGMENTS.items()
                }
                for label, coverage in rows.items():
                    first, last = coverage["requested_start"], coverage["requested_end"]
                    if first is None or last is None:
                        continue
                    first_day, last_day = date.fromisoformat(first), date.fromisoformat(last)
                    returns = {day: row.net_return for day, row in observations.items() if first_day <= day <= last_day}
                    keys = {"strategy": name, "cost_bps": bps, "window": label}
                    correlation_rows.extend({**keys, "reference": ref, **details}
                                            for ref, details in correlations(returns, references).items())
                    regression_rows.append({**keys, **factor_regression(returns, rf, references)})
            if name == "main":
                events = flip_rows(observations, references, MAIN_FACTORS)
                flips.extend({"cost_bps": bps, **event} for event in events)
                flip_aggregates[bps] = flip_summary(events, MAIN_FACTORS)

    output.mkdir(parents=True, exist_ok=True)
    audit = write_audit(simulations, monthly, output / "audit")
    verdict = _verdict(windows)
    _write_windows(output / "windows.csv", windows)
    _write_windows(output / "pre1970_windows.csv", supplement)
    _write_windows(output / "segments.csv", segments)
    _write_gzip_csv(output / "monthly.csv.gz", monthly_rows)
    _write_csv(output / "window_weights.csv", weight_rows)
    _write_csv(output / "correlations.csv", correlation_rows)
    _write_csv(output / "signal_flips.csv", flips)
    _write_csv(output / "key_sharpe.csv", [
        {"window": label, "cost_bps": bps, **{
            name: (windows[name, bps][label]["metrics"] or {}).get("sharpe") for name in CORE
        }}
        for label in windows["main", 0] for bps in COSTS
    ])
    def grouped(table):
        return {name: {str(cost): table[name, cost] for cost in COSTS}
                for name in dict.fromkeys(name for name, _ in table)}

    _write_json(output / "summary.json", {
        "mode": "non_system_monthly_factor_research",
        "engine": {"executed": False, "catalog_imported": False, "name": "offline Decimal factor settlement",
                   "version": code["commit"], "nautilus_trader_installed_unused": version("nautilus_trader")},
        "dependencies": {name: version(name) for name in
                         ("exchange-calendars", "numpy", "scipy", "statsmodels")},
        "code": code,
        "source": {"french": french_source, "comparators": comparator_source},
        "parameters": {"primary_first_return_month": PRIMARY_START, "last_return_month": end,
                       "lookback_months": 12, "threshold": "strictly > 0", "volatility_lookback_months": 36,
                       "maximum_scaled_gross": "2", "financing_spread_monthly_bps": 0, "costs_bps": COSTS,
                       "rf": "Ken French FF3 monthly RF", "fill_timing": "first next-month observed XNYS close label",
                       "payout": "complete official next-month return",
                       "paper_sample_ends": {"jf_2022": "2019-12", "nber_2019": "2015-12"}},
        "decision": verdict,
        "windows": grouped(windows),
        "pre1970_supplement": grouped(supplement),
        "segments": grouped(segments),
        "correlations": correlation_rows,
        "regressions": regression_rows,
        "signal_flip_summary": flip_aggregates,
        "signal_fill_samples_25bp": signal_samples,
        "audit": audit,
    })
    print(json.dumps(verdict, ensure_ascii=False, default=str, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--raw", type=Path, default=ROOT / "data/raw", help="Verified source ZIPs and manifest")
    parser.add_argument("--comparisons", type=Path, default=ROOT / "data/comparisons")
    parser.add_argument("--output", type=Path, default=ROOT / "results")
    parser.add_argument("--code-commit", default="HEAD", help="Commit whose files match the executed source")
    options = parser.parse_args()
    run(options.raw, options.comparisons, options.output, options.code_commit)
