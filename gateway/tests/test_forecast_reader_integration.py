# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""The forecast worker reads with a read-only role and publishes as an edge."""

import asyncio
from contextlib import closing, suppress
from datetime import datetime, timedelta
import hashlib
import json
import os
from pathlib import Path
import sqlite3
from urllib.parse import urlsplit, urlunsplit
from uuid import uuid4

import psycopg2
from psycopg2 import errors, sql
import pytest
from sqlalchemy import create_engine

import gateway
from ml import history, worker
from test_delivery_integration import server as delivery_server, until
from test_forecast_model import small_training
from test_forecast_worker import trainings

server = delivery_server
__all__ = ["small_training", "trainings"]
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)
ROLE = "estaciona_forecast_reader"
SCRIPT = Path(__file__).parents[2] / "infra" / "forecast-reader.sql"
FORECAST_KEY = "22" * 32


def owner_connection(server):
    return closing(psycopg2.connect(server.db_url))


@pytest.fixture
def reader(server):
    password = uuid4().hex
    with owner_connection(server) as conn, conn, conn.cursor() as cursor:
        cursor.execute(SCRIPT.read_text())
        cursor.execute(
            sql.SQL("ALTER ROLE {} PASSWORD %s").format(sql.Identifier(ROLE)),
            (password,),
        )
    parsed = urlsplit(server.db_url)
    netloc = f"{ROLE}:{password}@{parsed.hostname}:{parsed.port or 5432}"
    engine = create_engine(
        urlunsplit(parsed._replace(scheme="postgresql", netloc=netloc))
    )
    try:
        yield engine
    finally:
        engine.dispose()
        with owner_connection(server) as conn, conn, conn.cursor() as cursor:
            cursor.execute(sql.SQL("DROP OWNED BY {}").format(sql.Identifier(ROLE)))
            cursor.execute(sql.SQL("DROP ROLE {}").format(sql.Identifier(ROLE)))


def test_reader_role_reads_history_and_cannot_change_any_table(server, reader):
    worker.check_read_only(reader)
    owner = create_engine(server.db_url.replace("postgres://", "postgresql://", 1))
    with pytest.raises(PermissionError, match="can write"):
        worker.check_read_only(owner)
    owner.dispose()
    # Applying the script twice must leave the same privileges.
    with owner_connection(server) as conn, conn, conn.cursor() as cursor:
        cursor.execute(SCRIPT.read_text())
    worker.check_read_only(reader)

    columns = server.query(
        "SELECT DISTINCT ON (table_name) table_name, column_name "
        "FROM information_schema.columns WHERE table_schema='public' "
        "ORDER BY table_name, ordinal_position"
    )
    assert {"spots", "physical_occupancy_hourly", "users", "reservations"} <= {
        table for table, _ in columns
    }
    url = reader.url.render_as_string(hide_password=False)
    with closing(psycopg2.connect(url)) as conn:
        conn.autocommit = True
        with conn.cursor() as cursor:
            cursor.execute("SELECT COUNT(*) FROM physical_occupancy_hourly")
            cursor.execute("SHOW default_transaction_read_only")
            assert cursor.fetchone() == ("on",)
            # Prove the grants themselves, not only the read-only default.
            cursor.execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE")
            for table, column in columns:
                for statement in (
                    "INSERT INTO {table} DEFAULT VALUES",
                    "UPDATE {table} SET {column} = {column} WHERE false",
                    "DELETE FROM {table} WHERE false",
                    "TRUNCATE {table}",
                ):
                    query = sql.SQL(statement).format(
                        table=sql.Identifier(table), column=sql.Identifier(column)
                    )
                    with pytest.raises(errors.InsufficientPrivilege):
                        cursor.execute(query)


def test_worker_publishes_through_its_own_edge_credential(
    server, reader, trainings, tmp_path, monkeypatch
):
    monkeypatch.delenv("FORECAST_PROFILE", raising=False)
    server.query(
        "INSERT INTO physical_occupancy_hourly (spot_id, hour_start, status, observed_at) "
        "SELECT 'R02-A', h, CASE WHEN extract(hour FROM h) BETWEEN 11 AND 21 "
        "THEN 'occupied' ELSE 'free' END, h FROM generate_series("
        "date_trunc('hour', now()) - interval '42 days', "
        "date_trunc('hour', now()) - interval '1 hour', interval '1 hour') AS h"
    )
    # Only trend_prediction: no camera, spot or path authority.
    server.query(
        "INSERT INTO edge_gateways (id, name, credential_digest, trend_prediction) "
        "VALUES (%s, %s, %s, TRUE)",
        (
            str(uuid4()),
            "Forecast worker",
            hashlib.sha256(FORECAST_KEY.encode()).digest(),
        ),
    )
    outbox = str(tmp_path / "forecast_outbox.db")
    gateway.init_db(outbox)  # Polled below before the worker initializes it too.

    def delivered():
        with closing(sqlite3.connect(outbox)) as conn:
            return conn.execute(
                "SELECT delivery_result, message_text FROM fallback_events"
            ).fetchall()

    async def run():
        task = asyncio.create_task(
            worker.run(
                reader,
                str(tmp_path / "models"),
                outbox,
                str(tmp_path / "forecast_metrics.db"),
                server.url,
                FORECAST_KEY,
            )
        )
        try:
            await until(lambda: [row[0] for row in delivered()] == ["applied"])
        finally:
            task.cancel()
            with suppress(asyncio.CancelledError):
                await task

    asyncio.run(run())
    published = json.loads(delivered()[0][1])
    (stored,) = server.query("SELECT payload FROM edge_forecast_state WHERE singleton")
    assert stored[0]["type"] == "TREND_PREDICTION_V2"
    assert stored[0]["max_capacity"] == published["max_capacity"] == 1

    def hours(forecast):
        # The server re-serializes UTC instants with a Z suffix.
        return [
            (datetime.fromisoformat(item["timestamp"]), item["occupancy"])
            for item in forecast["next_24h_occupancy"]
        ]

    assert hours(stored[0]) == hours(published)
    assert len(trainings) == 1


def test_history_uses_the_map_revision_in_force_on_postgres(server, reader):
    """The reader sees revisions (JSONB manifests) and judges each hour by its map."""
    hours = server.query(
        "SELECT h FROM generate_series(date_trunc('hour', now()) - interval '6 hours', "
        "date_trunc('hour', now()) - interval '1 hour', interval '1 hour') AS h"
    )
    hours = [hour for (hour,) in hours]
    expanded = hours[3]
    extra = [f"S-{index}" for index in range(9)]
    for spot in extra:
        server.query(
            "INSERT INTO spots (id, parking_lot, status, x, y, z, is_active) "
            "VALUES (%s, 'Main', 'free', 0, 0, 0, TRUE)",
            (spot,),
        )
    manifest = {
        "parkings": [],
        "spots": [{"id": spot} for spot in ["R02-A", *extra]],
    }
    server.query(
        "INSERT INTO site_map_revisions (fingerprint, manifest, applied_at) "
        "VALUES (%s, %s::jsonb, %s)",
        ("e" * 64, json.dumps(manifest), expanded),
    )
    server.query(
        "INSERT INTO physical_occupancy_hourly (spot_id, hour_start, status, observed_at) "
        "SELECT 'R02-A', h, 'occupied', h FROM unnest(%s::timestamptz[]) AS h",
        (hours,),
    )
    frame = history.physical_history(
        reader, start=hours[0], end=hours[-1] + timedelta(hours=1)
    )
    assert frame["expected_spots"].tolist() == [1, 1, 1, 10, 10, 10]
    assert frame["rate"].tolist()[:3] == [1.0, 1.0, 1.0]
    assert frame["rate"].iloc[3:].isna().all()
