# ==============================================================================
# Copyright (C) 2026 Guilherme Pedroza
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU Affero General Public License as
# published by the Free Software Foundation, either version 3 of the
# License, or (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU Affero General Public License for more details.
#
# You should have received a copy of the GNU Affero General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.
# ==============================================================================

import sqlite3
import json
import uuid
from contextlib import closing

import datetime
import asyncio
import websockets

from ml.inference import PredictiveEngine

ACK_TIMEOUT_SECONDS = 5.0
HANDSHAKE_TIMEOUT_SECONDS = 3.0
RETRY_DELAY_SECONDS = 5.0


def init_db(db_path):
    with closing(sqlite3.connect(db_path)) as conn:
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=FULL")
        conn.execute("BEGIN IMMEDIATE")
        try:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS fallback_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    message_text TEXT NOT NULL,
                    synced INTEGER DEFAULT 0,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
                )
            """)
            columns = {
                row[1] for row in conn.execute("PRAGMA table_info(fallback_events)")
            }
            for column in (
                "event_uuid",
                "delivery_result",
                "delivery_reason",
                "forwarded_at",
            ):
                if column not in columns:
                    conn.execute(
                        f"ALTER TABLE fallback_events ADD COLUMN {column} TEXT"
                    )
            for (row_id,) in conn.execute(
                "SELECT id FROM fallback_events WHERE synced = 0 AND event_uuid IS NULL"
            ).fetchall():
                conn.execute(
                    "UPDATE fallback_events SET event_uuid = ? WHERE id = ?",
                    (str(uuid.uuid4()), row_id),
                )
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS fallback_event_uuid ON fallback_events(event_uuid)"
            )
            conn.commit()
        except Exception:
            conn.rollback()
            raise


def init_metrics_db(db_path):
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=FULL")
        conn.execute("""
            CREATE TABLE IF NOT EXISTS metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                edge_id TEXT NOT NULL,
                spot_id TEXT NOT NULL,
                status TEXT NOT NULL,
                detection_ts TEXT NOT NULL,
                edge_sent_ts TEXT NOT NULL,
                gateway_received_ts TEXT NOT NULL,
                gateway_forwarded_ts TEXT,
                cloud_ack INTEGER DEFAULT 0,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
        """)
        columns = {row[1] for row in conn.execute("PRAGMA table_info(metrics)")}
        for column in ("event_uuid", "delivery_result", "delivery_reason"):
            if column not in columns:
                conn.execute(f"ALTER TABLE metrics ADD COLUMN {column} TEXT")
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS metric_event_uuid ON metrics(event_uuid)"
        )


def save_event(db_path, message_text):
    event_uuid = str(uuid.uuid4())
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.execute(
            "INSERT INTO fallback_events (message_text, event_uuid) VALUES (?, ?)",
            (message_text, event_uuid),
        )
    return event_uuid


def get_unsynced_events(db_path):
    with closing(sqlite3.connect(db_path)) as conn:
        return conn.execute(
            "SELECT id, message_text FROM fallback_events WHERE synced = 0 ORDER BY id ASC"
        ).fetchall()


def pending_deliveries(db_path):
    with closing(sqlite3.connect(db_path)) as conn:
        return conn.execute(
            "SELECT id, event_uuid, message_text FROM fallback_events WHERE synced = 0 ORDER BY id"
        ).fetchall()


def mark_events_as_synced(db_path, event_ids):
    if not event_ids:
        return
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.executemany(
            "UPDATE fallback_events SET synced = 1 WHERE id = ?",
            [(i,) for i in event_ids],
        )


def save_metric(
    db_path,
    edge_id,
    spot_id,
    status,
    detection_ts,
    edge_sent_ts,
    gateway_received_ts,
    event_uuid=None,
):
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.execute(
            """
            INSERT INTO metrics (edge_id, spot_id, status, detection_ts,
                                 edge_sent_ts, gateway_received_ts, event_uuid)
            VALUES (?, ?, ?, ?, ?, ?, ?)
        """,
            (
                edge_id,
                spot_id,
                status,
                detection_ts,
                edge_sent_ts,
                gateway_received_ts,
                event_uuid,
            ),
        )


def update_metrics_forwarded_batch(db_path, updates):
    if not updates:
        return
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.executemany(
            """
            UPDATE metrics SET gateway_forwarded_ts = ? WHERE event_uuid = ?
        """,
            updates,
        )


def acknowledge_event(db_path, metrics_path, event_uuid, outcome, reason):
    # Metrics commit first. A crash before the outbox commit replays the same UUID;
    # PostgreSQL returns the original receipt and this metrics update is idempotent.
    with closing(sqlite3.connect(metrics_path)) as conn, conn:
        conn.execute(
            """
            UPDATE metrics SET cloud_ack = 1, delivery_result = ?, delivery_reason = ?
            WHERE event_uuid = ?
        """,
            (outcome, reason, event_uuid),
        )
    finish_event(db_path, event_uuid, outcome, reason)


def finish_event(db_path, event_uuid, outcome, reason=None):
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.execute(
            """
            UPDATE fallback_events SET synced = 1, delivery_result = ?, delivery_reason = ?
            WHERE event_uuid = ? AND synced = 0
        """,
            (outcome, reason, event_uuid),
        )


def record_forwarding(db_path, metrics_path, event_uuid, timestamp):
    with closing(sqlite3.connect(db_path)) as conn, conn:
        conn.execute(
            "UPDATE fallback_events SET forwarded_at = ? WHERE event_uuid = ?",
            (timestamp, event_uuid),
        )
    update_metrics_forwarded_batch(metrics_path, [(timestamp, event_uuid)])


async def receive_ack(connection, event_uuid):
    deadline = asyncio.get_running_loop().time() + ACK_TIMEOUT_SECONDS
    while True:
        remaining = deadline - asyncio.get_running_loop().time()
        if remaining <= 0:
            raise TimeoutError("Cloud ACK deadline expired")
        message = await asyncio.wait_for(connection.recv(), timeout=remaining)
        try:
            ack = json.loads(message)
        except json.JSONDecodeError, TypeError:
            continue
        if (
            isinstance(ack, dict)
            and ack.get("type") == "EDGE_ACK"
            and type(ack.get("version")) is int
            and ack["version"] == 1
            and ack.get("event_id") == event_uuid
            and ack.get("outcome") in ("applied", "ignored", "rejected")
            and type(ack.get("duplicate")) is bool
            and "reason" in ack
            and (ack["reason"] is None or isinstance(ack["reason"], str))
        ):
            return ack


def contains_null_character(value):
    if isinstance(value, str):
        return "\0" in value
    if isinstance(value, dict):
        return any(
            contains_null_character(key) or contains_null_character(item)
            for key, item in value.items()
        )
    if isinstance(value, list):
        return any(contains_null_character(item) for item in value)
    return False


def is_authorized(headers, expected_key):
    auth = headers.get("Authorization") or headers.get("authorization")
    if not auth:
        return False
    parts = auth.split(" ")
    if len(parts) != 2 or parts[0] != "Bearer":
        return False
    return parts[1] == expected_key


async def handler(websocket, db_path, metrics_path, expected_key, sync_event):
    if not is_authorized(websocket.request.headers, expected_key):
        await websocket.close(1008, "Unauthorized")
        return

    print(
        f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] EDGE_CONNECTED",
        flush=True,
    )
    db_conn = sqlite3.connect(db_path)
    metrics_conn = sqlite3.connect(metrics_path)

    try:
        while True:
            messages = [await websocket.recv()]

            while True:
                try:
                    msg = await asyncio.wait_for(websocket.recv(), timeout=0.01)
                    messages.append(msg)
                except asyncio.TimeoutError, Exception:
                    break

            events_to_insert = []
            metrics_to_insert = []

            for message in messages:
                try:
                    payload = json.loads(message)
                    if not isinstance(payload, dict):
                        continue
                except json.JSONDecodeError, TypeError:
                    print("[WARN] INVALID_JSON dropped", flush=True)
                    continue
                now_str = (
                    datetime.datetime.now(datetime.UTC)
                    .isoformat(timespec="milliseconds")
                    .replace("+00:00", "Z")
                )
                payload["gateway_received_at"] = now_str

                event_uuid = str(uuid.uuid4())
                events_to_insert.append((json.dumps(payload), event_uuid))

                edge_id = payload.get("camera_id") or "unknown"
                spot_id = payload.get("spot_id") or "unknown"
                status = payload.get("status") or "unknown"
                detection_ts = payload.get("timestamp") or ""
                edge_sent_ts = (
                    payload.get("edge_sent_at") or payload.get("edge_sent_ts") or ""
                )
                metrics_to_insert.append(
                    (
                        edge_id,
                        spot_id,
                        status,
                        detection_ts,
                        edge_sent_ts,
                        now_str,
                        event_uuid,
                    )
                )

                try:
                    t1 = datetime.datetime.fromisoformat(now_str.replace("Z", "+00:00"))
                    t0 = datetime.datetime.fromisoformat(
                        edge_sent_ts.replace("Z", "+00:00")
                    )
                    latency_ms = (t1 - t0).total_seconds() * 1000
                except Exception:
                    latency_ms = 0.0

                print(
                    f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] MSG_RECEIVED edge_id={edge_id} spot_id={spot_id} latency_ms={latency_ms:.1f}",
                    flush=True,
                )

            if events_to_insert:
                db_conn.executemany(
                    "INSERT INTO fallback_events (message_text, event_uuid) VALUES (?, ?)",
                    events_to_insert,
                )
                db_conn.commit()
                # Delivery must wake even if the separate metrics write fails.
                sync_event.set()

            if metrics_to_insert:
                metrics_conn.executemany(
                    """
                    INSERT INTO metrics (
                        edge_id, spot_id, status, detection_ts, edge_sent_ts, gateway_received_ts, event_uuid
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                    metrics_to_insert,
                )
                metrics_conn.commit()

    except Exception as e:
        print(
            f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] EDGE_DISCONNECTED reason={e}",
            flush=True,
        )
    finally:
        db_conn.close()
        metrics_conn.close()


async def sync_loop(db_path, metrics_path, cloud_url, api_key, sync_event):
    cloud_conn = None
    headers = {"Authorization": f"Bearer {api_key}"}
    try:
        while True:
            try:
                events = pending_deliveries(db_path)
                if not events:
                    await sync_event.wait()
                    sync_event.clear()
                    continue

                if cloud_conn is None:
                    cloud_conn = await websockets.connect(
                        cloud_url, additional_headers=headers, open_timeout=3
                    )
                    ready = json.loads(
                        await asyncio.wait_for(
                            cloud_conn.recv(), timeout=HANDSHAKE_TIMEOUT_SECONDS
                        )
                    )
                    if not (
                        isinstance(ready, dict)
                        and ready.get("type") == "EDGE_READY"
                        and type(ready.get("delivery_version")) is int
                        and ready["delivery_version"] == 1
                    ):
                        raise ValueError("Cloud does not support delivery version 1")
                    durable_events = ready.get("durable_events", ["SPOT_UPDATE"])
                    if not isinstance(durable_events, list) or not all(
                        isinstance(kind, str) for kind in durable_events
                    ):
                        raise ValueError("Invalid durable event capabilities")
                    print("[INFO] CLOUD_CONNECTED delivery_version=1", flush=True)

                for _, event_uuid, message_text in events:
                    try:
                        payload = json.loads(message_text)
                        # Python accepts NaN/Infinity and overflow to inf; Rust JSON does not.
                        json.dumps(payload, allow_nan=False)
                    except ValueError, TypeError:
                        finish_event(
                            db_path, event_uuid, "rejected", "invalid_local_json"
                        )
                        print(
                            f"[ERROR] LOCAL_EVENT_REJECTED event_id={event_uuid}",
                            flush=True,
                        )
                        continue
                    if not isinstance(payload, dict) or contains_null_character(
                        payload
                    ):
                        finish_event(
                            db_path, event_uuid, "rejected", "invalid_local_payload"
                        )
                        continue
                    now_str = (
                        datetime.datetime.now(datetime.UTC)
                        .isoformat(timespec="milliseconds")
                        .replace("+00:00", "Z")
                    )
                    if payload.get("type") in durable_events:
                        frame = {
                            "type": "EDGE_EVENT",
                            "version": 1,
                            "event_id": event_uuid,
                            "payload": payload,
                            "gateway_forwarded_at": now_str,
                        }
                        await cloud_conn.send(json.dumps(frame))
                        record_forwarding(db_path, metrics_path, event_uuid, now_str)
                        ack = await receive_ack(cloud_conn, event_uuid)
                        acknowledge_event(
                            db_path,
                            metrics_path,
                            event_uuid,
                            ack["outcome"],
                            ack["reason"],
                        )
                        print(
                            f"[INFO] CLOUD_ACK event_id={event_uuid} outcome={ack['outcome']} duplicate={ack['duplicate']}",
                            flush=True,
                        )
                    else:
                        # Never turn a transport send into a claim of persistence.
                        # Keep the UUID and payload intact until a capable server is available.
                        raise ValueError(
                            "Server does not support durable delivery for event type"
                        )
            except asyncio.CancelledError:
                raise
            except Exception as error:
                print(f"[ERROR] CLOUD_DISCONNECTED reason={error}", flush=True)
                if cloud_conn is not None:
                    try:
                        await cloud_conn.close()
                    except Exception:
                        pass
                    cloud_conn = None
                await asyncio.sleep(RETRY_DELAY_SECONDS)
    finally:
        if cloud_conn is not None:
            await cloud_conn.close()


async def prediction_loop(db_path, sync_event):
    print(
        f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] LOADING_PREDICTIVE_MODELS",
        flush=True,
    )
    try:
        engine = await asyncio.to_thread(PredictiveEngine)
        print(
            f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] PREDICTIVE_MODELS_READY",
            flush=True,
        )
    except Exception as e:
        print(
            f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [ERROR] ML_INIT_FAILED reason={e}",
            flush=True,
        )
        return

    while True:
        try:
            payload = await asyncio.to_thread(engine.predict_trends)
            await asyncio.to_thread(save_event, db_path, json.dumps(payload))
            sync_event.set()
            print(
                f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] TREND_PREDICTION_QUEUED",
                flush=True,
            )
        except Exception as e:
            print(
                f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [ERROR] PREDICTION_FAILED reason={e}",
                flush=True,
            )

        await asyncio.sleep(300)


async def main():
    import os
    from dotenv import load_dotenv

    load_dotenv(os.path.join(os.path.dirname(__file__), "../.env"))

    db_path = "local_fallback.db"
    metrics_path = "metrics.db"

    init_db(db_path)
    init_metrics_db(metrics_path)

    cloud_url = os.environ.get("SERVER_WS_URL", "wss://api.estaciona.tech/ws/edge")
    api_key = os.environ["EDGE_API_KEY"]
    port = int(os.environ.get("GATEWAY_PORT", "8001"))

    print(
        f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] STARTING_GATEWAY port={port}",
        flush=True,
    )

    sync_event = asyncio.Event()

    asyncio.create_task(
        sync_loop(db_path, metrics_path, cloud_url, api_key, sync_event)
    )

    asyncio.create_task(prediction_loop(db_path, sync_event))

    async def ws_handler(websocket, *args, **kwargs):
        await handler(websocket, db_path, metrics_path, api_key, sync_event)

    async with websockets.serve(ws_handler, "0.0.0.0", port):
        await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())
