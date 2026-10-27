# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Hourly last-observation occupancy; no personal identities or reservations."""

import json
from datetime import datetime, timezone

import pandas as pd
from sqlalchemy import bindparam, text

# Share of the expected spots an hour needs before its occupancy rate counts.
MIN_COVERAGE = 0.9

COUNTS = text("""
    SELECT hour_start AS timestamp,
           SUM(CASE WHEN status='occupied' THEN 1 ELSE 0 END) AS occupancy,
           COUNT(*) AS observed_spots
    FROM physical_occupancy_hourly
    WHERE hour_start >= :lower AND hour_start < :upper AND spot_id IN :spots
    GROUP BY hour_start
""").bindparams(bindparam("spots", expanding=True))
COLUMNS = ["timestamp", "occupancy", "observed_spots", "expected_spots"]
# Bounds that every stored hour falls between.
EARLIEST = datetime(1970, 1, 1, tzinfo=timezone.utc)


def _timestamp(value):
    stamp = pd.Timestamp(value)
    return stamp.tz_localize("UTC") if stamp.tzinfo is None else stamp.tz_convert("UTC")


def expected_spots(connection):
    """The spots each period should report, as (from, until, ids) intervals.

    Each site map revision defines its spots from the moment it was applied
    until the next one; the first also covers the hours before it. Without
    any revision, the currently active spots are expected throughout.
    """
    revisions = connection.execute(
        text("SELECT applied_at, manifest FROM site_map_revisions ORDER BY revision")
    ).fetchall()
    if not revisions:
        active = connection.execute(
            text("SELECT id FROM spots WHERE is_active ORDER BY id")
        ).scalars()
        return [(None, None, sorted(active))]
    intervals = []
    for index, (applied_at, manifest) in enumerate(revisions):
        if isinstance(manifest, str):
            manifest = json.loads(manifest)
        ids = sorted({spot["id"] for spot in manifest.get("spots", [])})
        start = None if index == 0 else _timestamp(applied_at)
        until = (
            _timestamp(revisions[index + 1][0]) if index + 1 < len(revisions) else None
        )
        intervals.append((start, until, ids))
    return intervals


def physical_history(engine, start=None, end=None):
    end = end or datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
    start = _timestamp(start) if start is not None else EARLIEST
    end = _timestamp(end)
    frames = []
    with engine.connect() as connection:
        for begins, until, ids in expected_spots(connection):
            lower = max(start, begins) if begins is not None else start
            upper = min(end, until) if until is not None else end
            if not ids or lower >= upper:
                continue
            frame = pd.read_sql(
                COUNTS,
                connection,
                params={
                    "lower": lower.isoformat(),
                    "upper": upper.isoformat(),
                    "spots": ids,
                },
            )
            frame["expected_spots"] = len(ids)
            frames.append(frame)
    frames = [frame for frame in frames if not frame.empty]
    frame = (
        pd.concat(frames, ignore_index=True)
        if frames
        else pd.DataFrame(columns=COLUMNS)
    )
    frame["timestamp"] = pd.to_datetime(frame["timestamp"], utc=True)
    frame = frame.sort_values("timestamp", ignore_index=True)
    frame["occupied"] = frame["occupancy"].astype(float)
    observed = frame["observed_spots"].astype(float)
    expected = frame["expected_spots"].astype(float)
    # A camera that stops reporting lowers coverage; a spot added to the map
    # is expected only from its revision on, so earlier hours keep counting.
    covered = (observed >= MIN_COVERAGE * expected) & (expected > 0)
    frame["rate"] = (frame["occupied"] / observed).where(covered)
    # Partial coverage is not an observation of the whole parking lot.
    frame["occupancy"] = frame["occupied"].where(observed == expected)
    return frame[
        [
            "timestamp",
            "occupancy",
            "observed_spots",
            "expected_spots",
            "occupied",
            "rate",
        ]
    ]
