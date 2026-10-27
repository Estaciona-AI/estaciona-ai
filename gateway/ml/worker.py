# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Forecast worker: reads history with a read-only role, publishes as an edge.

Run from gateway/ with ``python -m ml.worker``. ML_DATABASE_URL names the role
created by infra/forecast-reader.sql; FORECAST_EDGE_API_KEY is an edge
credential of its own, granted only trend_prediction.
"""

import asyncio
import json
import os
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

from sqlalchemy import text

import gateway

from .inference import ForecastLedger, PredictiveEngine, StaleArtifact
from .site_calendar import profile_from_env
from .train import get_db_engine, train_occupancy_model

# Minutes past each hour, once the closed hour has its samples.
INFERENCE_MINUTE = 5
# Local time of the daily retrain.
RETRAIN_HOUR, RETRAIN_MINUTE = 3, 30
READ_TABLES = (
    "installation_binding",
    "physical_occupancy_hourly",
    "site_map_revisions",
    "spots",
)
WRITABLE_TABLES = text("""
    SELECT DISTINCT c.relname
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    CROSS JOIN unnest(ARRAY['INSERT', 'UPDATE', 'DELETE', 'TRUNCATE']) AS p(privilege)
    WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
      AND n.nspname NOT IN ('pg_catalog', 'information_schema')
      AND n.nspname NOT LIKE 'pg_toast%'
      AND has_table_privilege(c.oid, p.privilege)
    ORDER BY c.relname
""")


def log(level, event):
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
    print(f"{stamp} [{level}] {event}", flush=True)


def check_read_only(engine):
    """Refuse a role that could change any table; the forecast only reads."""
    with engine.connect() as connection:
        writable = connection.execute(WRITABLE_TABLES).scalars().all()
        readable = all(
            connection.execute(
                text("SELECT has_table_privilege(:table, 'SELECT')"),
                {"table": table},
            ).scalar_one()
            for table in READ_TABLES
        )
    if writable:
        raise PermissionError(
            "Forecast database role can write to: " + ", ".join(writable)
        )
    if not readable:
        raise PermissionError("Forecast database role cannot read occupancy history")


def next_inference(now):
    run = now.replace(minute=INFERENCE_MINUTE, second=0, microsecond=0)
    return run if run > now else run + timedelta(hours=1)


def next_retrain(now, zone):
    local = now.astimezone(ZoneInfo(zone))
    run = local.replace(
        hour=RETRAIN_HOUR, minute=RETRAIN_MINUTE, second=0, microsecond=0
    )
    if run <= local:
        run = (local + timedelta(days=1)).replace(
            hour=RETRAIN_HOUR, minute=RETRAIN_MINUTE, second=0, microsecond=0
        )
    return run.astimezone(timezone.utc)


def retrain(engine, models_dir, ledger=None):
    train_occupancy_model(engine, models_dir)
    return PredictiveEngine(models_dir, engine, ledger)


def ensure_model(engine, models_dir, ledger=None):
    """Serve the stored artifact, or train one when it is missing, invalid or
    trained for another installation, site profile or capacity."""
    try:
        return PredictiveEngine(models_dir, engine, ledger)
    except (OSError, ValueError) as error:
        log("INFO", f"FORECAST_MODEL_UNAVAILABLE reason={error}")
    return retrain(engine, models_dir, ledger)


async def forecast_loop(
    engine,
    models_dir,
    outbox,
    sync_event,
    *,
    ledger=None,
    clock=lambda: datetime.now(timezone.utc),
    sleep=asyncio.sleep,
):
    predictor = await asyncio.to_thread(ensure_model, engine, models_dir, ledger)
    capacity = await asyncio.to_thread(predictor.get_capacity)
    zone = profile_from_env(capacity).timezone
    retrain_at = next_retrain(clock(), zone)
    publish_at = clock()
    while True:
        if clock() >= retrain_at:
            try:
                predictor = await asyncio.to_thread(retrain, engine, models_dir, ledger)
                log("INFO", "FORECAST_MODEL_RETRAINED")
            except Exception as error:
                # The previous model keeps serving until a retrain succeeds.
                log("ERROR", f"FORECAST_RETRAIN_FAILED reason={error}")
            retrain_at = next_retrain(clock(), zone)
        if clock() >= publish_at:
            try:
                payload = await asyncio.to_thread(predictor.predict_trends)
                await asyncio.to_thread(
                    gateway.save_event, outbox, json.dumps(payload, allow_nan=False)
                )
                sync_event.set()
                log("INFO", "TREND_PREDICTION_QUEUED")
            except StaleArtifact as error:
                # The map, profile or installation changed: retrain right away.
                log("INFO", f"FORECAST_MODEL_STALE reason={error}")
                retrain_at = clock()
                continue
            except Exception as error:
                log("ERROR", f"PREDICTION_FAILED reason={error}")
            publish_at = next_inference(clock())
        # Wake for whichever comes first: the retrain keeps its own time.
        wake = min(publish_at, retrain_at)
        await sleep(max(0.0, (wake - clock()).total_seconds()))


async def run(engine, models_dir, outbox, metrics, cloud_url, api_key, ledger):
    gateway.init_db(outbox)
    gateway.init_metrics_db(metrics)
    sync_event = asyncio.Event()
    await asyncio.gather(
        gateway.sync_loop(outbox, metrics, cloud_url, api_key, sync_event),
        forecast_loop(
            engine, models_dir, outbox, sync_event, ledger=ForecastLedger(ledger)
        ),
    )


def main():
    api_key = os.environ["FORECAST_EDGE_API_KEY"]
    cloud_url = os.environ.get("SERVER_WS_URL", "wss://api.estaciona.tech/ws/edge")
    engine = get_db_engine()
    check_read_only(engine)
    log("INFO", "FORECAST_WORKER_STARTING")
    models_dir = os.path.join(os.path.dirname(__file__), "models")
    asyncio.run(
        run(
            engine,
            models_dir,
            "forecast_outbox.db",
            "forecast_metrics.db",
            cloud_url,
            api_key,
            "forecast_ledger.db",
        )
    )


if __name__ == "__main__":
    main()
