# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""In-memory stand-in for the tables the forecast reader can see."""

import json

from sqlalchemy import create_engine, text


INSTALLATION = "6f1c2a8e-4b5d-4e7f-9a10-3c2b1d0e9f87"


def history_engine(
    active=(), retired=(), rows=(), revisions=(), installation=INSTALLATION
):
    """`rows` are (spot, hour, status); `revisions` are (applied_at, spot ids)."""
    engine = create_engine("sqlite://")
    with engine.begin() as connection:
        connection.execute(
            text("CREATE TABLE spots (id TEXT PRIMARY KEY,is_active BOOLEAN)")
        )
        connection.execute(
            text(
                "CREATE TABLE physical_occupancy_hourly "
                "(spot_id TEXT,hour_start TEXT,status TEXT)"
            )
        )
        connection.execute(
            text(
                "CREATE TABLE site_map_revisions "
                "(revision INTEGER PRIMARY KEY,applied_at TEXT,manifest TEXT)"
            )
        )
        connection.execute(
            text("CREATE TABLE installation_binding (installation_id TEXT)")
        )
        connection.execute(
            text("INSERT INTO installation_binding VALUES (:id)"),
            {"id": installation},
        )
        for spot, is_active in [(s, True) for s in active] + [
            (s, False) for s in retired
        ]:
            connection.execute(
                text("INSERT INTO spots VALUES (:id,:active)"),
                {"id": spot, "active": is_active},
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
        for applied_at, ids in revisions:
            manifest = {"parkings": [], "spots": [{"id": spot} for spot in ids]}
            connection.execute(
                text(
                    "INSERT INTO site_map_revisions (applied_at,manifest) "
                    "VALUES (:applied_at,:manifest)"
                ),
                {
                    "applied_at": applied_at.isoformat(),
                    "manifest": json.dumps(manifest),
                },
            )
    return engine
