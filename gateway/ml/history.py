# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Hourly last-observation occupancy; no personal identities or reservations."""

from datetime import datetime, timezone
import pandas as pd
from sqlalchemy import text


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
        frame = pd.read_sql(query, connection, params=params)
    frame["timestamp"] = pd.to_datetime(frame["timestamp"], utc=True)
    return frame
