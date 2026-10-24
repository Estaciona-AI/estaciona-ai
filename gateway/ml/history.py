# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Hourly last-observation occupancy; no personal identities or reservations."""

from datetime import datetime, timezone
import pandas as pd
from sqlalchemy import text

# Share of monitored spots an hour needs before its occupancy rate counts.
MIN_COVERAGE = 0.9


def physical_history(engine, start=None, end=None):
    end = end or datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
    params = {"end": end.isoformat()}
    lower_bound = ""
    if start is not None:
        lower_bound = "AND h.hour_start >= :start"
        params["start"] = start.isoformat()
    query = text(f"""
        SELECT h.hour_start AS timestamp,
               SUM(CASE WHEN h.status='occupied' THEN 1 ELSE 0 END) AS occupancy,
               COUNT(*) AS observed_spots
        FROM physical_occupancy_hourly h
        JOIN spots s ON s.id=h.spot_id AND s.is_active
        WHERE h.hour_start < :end {lower_bound}
        GROUP BY h.hour_start ORDER BY h.hour_start
    """)
    with engine.connect() as connection:
        capacity = connection.execute(
            text("SELECT COUNT(*) FROM spots WHERE is_active")
        ).scalar_one()
        frame = pd.read_sql(query, connection, params=params)
    frame["occupied"] = frame["occupancy"].astype(float)
    # Spots are not dated, so hours are judged against the most spots seen so far:
    # registering a new spot does not erase the history recorded before it.
    monitored = frame["observed_spots"].cummax().clip(upper=capacity)
    covered = (frame["observed_spots"] >= MIN_COVERAGE * monitored) & (monitored > 0)
    frame["rate"] = (frame["occupied"] / frame["observed_spots"]).where(covered)
    # Partial camera coverage is not an observation of the whole parking lot.
    frame.loc[(frame["observed_spots"] != capacity) | (capacity == 0), "occupancy"] = (
        float("nan")
    )
    frame["timestamp"] = pd.to_datetime(frame["timestamp"], utc=True)
    return frame
