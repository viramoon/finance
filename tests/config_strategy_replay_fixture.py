"""Build the F7 integration replay from the XNYS calendar and integer arithmetic."""

import json
from pathlib import Path
import sys

from fertile_finance.research_catalog import nyse


BARS_REF = "example.reviewed-us-daily@1.0.0"
UNIVERSE_REF = "example.us-cash-equities@1.0.0"
INSTRUMENTS = ("AAA.XNYS", "BBB.XNYS", "CCC.XNYS")


def build(root: Path) -> None:
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
            # All assets gain exactly one dollar per session from distinct starting prices.
            # Delay one first-entry quote and one timed-exit quote by one second.
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
    build(Path(sys.argv[1]))
