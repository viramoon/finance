"""Build the F7 integration replay from the XNYS calendar and integer arithmetic."""

import argparse
import json
from pathlib import Path

from fertile_finance.research_catalog import nyse


BARS_REF = "example.reviewed-us-daily@1.0.0"
UNIVERSE_REF = "example.us-cash-equities@1.0.0"
INSTRUMENTS = ("AAA.XNYS", "BBB.XNYS", "CCC.XNYS")


def ranking_price(index: int, day: str) -> int:
    # 20-session returns at the first decision: 0%, 25%, 100%.
    # On March 5, AAA moves ahead at 150% and displaces BBB.
    if day < "2024-03-01":
        return (100, 80, 25)[index]
    return ((100, 100, 50) if day < "2024-03-05" else (250, 100, 50))[index]


def build(root: Path, *, target_weights=False) -> None:
    first, last = "2024-01-02", "2024-06-28"
    calendar = nyse().schedule.loc[first:last]
    membership, bars = [], []
    for offset, (session, row) in enumerate(calendar.iterrows()):
        day = session.date().isoformat()
        membership.append({
            "date": day,
            "first_available_ts": str(row["open"].value),
            "members": list(INSTRUMENTS),
        })
        for index, instrument in enumerate(INSTRUMENTS):
            if target_weights:
                close = f"{ranking_price(index, day)}.00"
                late = False
            else:
                # All assets gain one dollar per session. Delay two execution quotes.
                close = f"{100 + 20 * index + offset}.00"
                late = (instrument, day) in {("AAA.XNYS", "2024-03-04"), ("BBB.XNYS", "2024-03-19")}
            delay = 1_000_000_000 if late else 0
            bars.append({
                "instrument_id": instrument,
                "date": day,
                "first_available_ts": str(row["close"].value + delay),
                "open": close,
                "high": close,
                "low": close,
                "close": close,
                "volume": "1000",
            })
    snapshot = {
        "schema": "fertile.finance/strategy-replay/v1",
        "bars_ref": BARS_REF,
        "universe_ref": UNIVERSE_REF,
        "calendar": "XNYS",
        "currency": "USD",
        "timeframe": "1d",
        "price_basis": "raw_split_free_reviewed",
        "first_session": first,
        "last_session": last,
        "instruments": list(INSTRUMENTS),
        "membership": membership,
        "bars": bars,
    }
    directory = root / "finance-state/backtests/strategy-inputs" / BARS_REF
    directory.mkdir(parents=True)
    (directory / f"{UNIVERSE_REF}.json").write_text(json.dumps(snapshot), encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--target-weights", action="store_true")
    args = parser.parse_args()
    build(args.root, target_weights=args.target_weights)
