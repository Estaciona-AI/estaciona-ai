# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from datetime import timedelta

import numpy as np
import pandas as pd
import pytest
from sqlalchemy import create_engine, text

from ml import history


def physical_engine(spots, rows):
    engine = create_engine("sqlite://")
    with engine.begin() as connection:
        connection.execute(
            text("CREATE TABLE spots (id TEXT PRIMARY KEY,is_active BOOLEAN)")
        )
        connection.execute(
            text(
                "CREATE TABLE physical_occupancy_hourly (spot_id TEXT,hour_start TEXT,status TEXT)"
            )
        )
        for spot in spots:
            connection.execute(
                text("INSERT INTO spots VALUES (:id,TRUE)"), {"id": spot}
            )
        if rows:
            connection.execute(
                text(
                    "INSERT INTO physical_occupancy_hourly VALUES (:spot,:hour,:status)"
                ),
                [
                    {"spot": spot, "hour": hour.isoformat(), "status": status}
                    for spot, hour, status in rows
                ],
            )
    return engine


def test_registering_a_spot_keeps_the_earlier_rate_history():
    hours = pd.date_range("2026-10-01", periods=3, freq="h", tz="UTC")
    rows = [(spot, hour, "occupied") for hour in hours for spot in ("a", "b")]
    rows += [("c", hours[-1], "free")]  # Spot c starts reporting in the last hour.
    engine = physical_engine(["a", "b", "c"], rows)
    frame = history.physical_history(engine, end=hours[-1] + timedelta(hours=1))
    assert frame["rate"].tolist() == pytest.approx([1.0, 1.0, 2 / 3])
    # The exact whole-lot count used by the legacy model stays strict.
    assert frame["occupancy"].isna().tolist() == [True, True, False]
    engine.dispose()


@pytest.mark.parametrize("observed, has_rate", [(8, False), (9, True), (10, True)])
def test_hours_need_most_monitored_spots_to_count(observed, has_rate):
    spots = [str(index) for index in range(10)]
    hours = pd.date_range("2026-10-01", periods=2, freq="h", tz="UTC")
    rows = [(spot, hours[0], "free") for spot in spots]
    rows += [(spot, hours[1], "occupied") for spot in spots[:observed]]
    engine = physical_engine(spots, rows)
    frame = history.physical_history(engine, end=hours[-1] + timedelta(hours=1))
    assert np.isfinite(frame["rate"].iloc[1]) == has_rate
    engine.dispose()
