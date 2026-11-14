# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Prospective comparison: forecasts as issued against what was observed later.

Run from gateway/ with ``python -m ml.prospective [ledger]``; ML_DATABASE_URL
names the read-only forecast role. The ledger defaults to forecast_ledger.db
in the working directory, where the worker writes it.
"""

import json
import os
import sys
from datetime import datetime, timedelta, timezone

import numpy as np
import pandas as pd

from .backtest import BASELINES, HORIZON, compare
from .history import physical_history
from .inference import ForecastLedger
from .train import get_db_engine, history_provenance

METHODS = ("served", "model") + BASELINES


def _hour(value):
    stamp = pd.Timestamp(value)
    stamp = stamp.tz_localize("UTC") if stamp.tzinfo is None else stamp
    return stamp.tz_convert("UTC").floor("h")


def _daily(actual, predictions, days):
    """Days on which the served forecast had the lower error, per method.

    Neighbouring hours and the forecasts that overlap on them are not
    independent, so days, not points, are the units to count.
    """
    mask = np.isfinite(actual)
    for values in predictions.values():
        mask &= np.isfinite(values)
    errors = {name: {} for name in predictions}
    for row in np.flatnonzero(mask.any(axis=1)):
        for name, values in predictions.items():
            error = np.abs(values[row, mask[row]] - actual[row, mask[row]])
            errors[name].setdefault(days[row], []).extend(error.tolist())
    mean = {
        name: {day: float(np.mean(values)) for day, values in by_day.items()}
        for name, by_day in errors.items()
    }
    return {
        "days": len(mean["served"]),
        "served_better_days": {
            name: sum(mean["served"][day] < value for day, value in by_day.items())
            for name, by_day in mean.items()
            if name != "served"
        },
    }


def prospective_report(ledger, engine, now=None):
    """Scores of everything in the ledger on the hours observed since.

    Every method is scored on the same points: target hours that have closed,
    were observed, were still open when the forecast was issued and were
    forecast by all of them. `excluded` counts the rest by reason.
    """
    now = _hour(now or datetime.now(timezone.utc))
    report = {"history": history_provenance(engine), "as_of": now.isoformat()}
    entries = ledger.entries()
    if not entries:
        return {**report, "status": "unavailable", "reason": "no_issued_forecasts"}
    first = _hour(entries[0]["origin"])
    # Closed hours only; none when the first forecast was issued this hour.
    closed = max(int((now - first) / timedelta(hours=1)), 0)
    index = pd.date_range(first, periods=closed, freq="h")
    frame = physical_history(engine, first, now)
    rate = frame.set_index("timestamp")["rate"].reindex(index).to_numpy(np.float64)

    shape = (len(entries), HORIZON)
    actual = np.full(shape, np.nan)
    predictions = {name: np.full(shape, np.nan) for name in METHODS}
    excluded = {"issued_after_target": 0, "pending": 0, "unobserved": 0}
    positions, days = [], []
    for row, entry in enumerate(entries):
        origin = _hour(entry["origin"])
        position = int((origin - first) / timedelta(hours=1))
        positions.append(position)
        days.append(origin.date().isoformat())
        for name, values in entry["rates"].items():
            if name in predictions and values.shape == (HORIZON,):
                predictions[name][row] = values
        issued_at = pd.Timestamp(entry["issued_at"])
        for horizon in range(HORIZON):
            target = position + horizon
            if issued_at >= origin + timedelta(hours=horizon + 1):
                # The hour had closed: a reconstruction, not a forecast.
                excluded["issued_after_target"] += 1
            elif target >= len(index):
                excluded["pending"] += 1
            elif not np.isfinite(rate[target]):
                excluded["unobserved"] += 1
            else:
                actual[row, horizon] = rate[target]

    scores = compare(actual, predictions, positions)
    report.update(
        {
            "status": "evaluated" if scores["points"] else "unavailable",
            "issued_origins": len(entries),
            "first_origin": first.isoformat(),
            "last_origin": _hour(entries[-1]["origin"]).isoformat(),
            "serving": sorted({entry["serving"] or "unrecorded" for entry in entries}),
            "excluded": excluded,
            "scores": scores,
        }
    )
    if scores["points"]:
        report["daily"] = _daily(actual, predictions, days)
    else:
        report["reason"] = "no_comparable_points"
    return report


if __name__ == "__main__":
    path = sys.argv[1] if len(sys.argv) > 1 else "forecast_ledger.db"
    if not os.path.exists(path):
        sys.exit(f"No forecast ledger at {path}")
    print(
        json.dumps(
            prospective_report(ForecastLedger(path), get_db_engine()),
            indent=2,
            allow_nan=False,
        )
    )
