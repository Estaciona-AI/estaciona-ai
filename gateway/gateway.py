# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import sqlite3
import hmac
import ipaddress
import json
import os
import stat
import uuid
from contextlib import closing
from dataclasses import dataclass

import datetime
import asyncio
import websockets

from protocol import encode_edge_event

ACK_TIMEOUT_SECONDS = 5.0
HANDSHAKE_TIMEOUT_SECONDS = 3.0
RETRY_DELAY_SECONDS = 5.0
LOCAL_BATCH_MAX_MESSAGES = 64
LOCAL_BATCH_FLUSH_BYTES = 256 * 1024
LOCAL_BATCH_MAX_AGE_SECONDS = 0.05
LOCAL_BATCH_IDLE_SECONDS = 0.01
# Confirmed events stay long enough for a producer to replay one it never saw
# acknowledged locally; the server keeps its receipts, so a later replay is
# answered as a duplicate. Metrics are diagnostics.
SYNCED_RETENTION_HOURS = 24
METRICS_RETENTION_DAYS = 7
MAX_DB_BYTES = 2 * 1024**3
RETENTION_BATCH = 5000
RETENTION_INTERVAL_SECONDS = 300
MIN_LOCAL_KEY_BYTES = 32
MAX_SECRET_BYTES = 4096
DEFAULT_GATEWAY_HOST = "127.0.0.1"


@dataclass(frozen=True)
class Retention:
    synced_hours: float = SYNCED_RETENTION_HOURS
    metrics_days: float = METRICS_RETENTION_DAYS
    max_db_bytes: int = MAX_DB_BYTES


@dataclass(frozen=True)
class Settings:
    cloud_url: str
    cloud_key: str
    local_key: str
    host: str
    port: int
    max_in_flight: int
    retention: Retention = Retention()


def read_secret_file(path):
    """A regular file only this process's user can read, as the server requires."""
    before = os.lstat(path)
    if (
        not stat.S_ISREG(before.st_mode)
        or before.st_mode & 0o077
        or before.st_uid != os.geteuid()
    ):
        raise ValueError(f"{path} must be a private regular file owned by this user")
    with open(path, "rb") as handle:
        opened = os.fstat(handle.fileno())
        # The path must still name the inspected file when it is opened.
        if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
            raise ValueError(f"{path} changed while it was opened")
        data = handle.read(MAX_SECRET_BYTES + 1)
    if len(data) > MAX_SECRET_BYTES:
        raise ValueError(f"{path} exceeds {MAX_SECRET_BYTES} bytes")
    text = data.decode("utf-8")
    if text.endswith("\n"):
        text = text[:-1].removesuffix("\r")
    return text


def secret(environ, name):
    path = environ.get(f"{name}_FILE")
    value = read_secret_file(path) if path else environ.get(name)
    if not value or any(ord(c) < 32 for c in value):
        raise ValueError(f"{name} or {name}_FILE is required")
    return value


def load_settings(environ):
    """Two credentials: the cloud one never leaves this process, and producers
    on this host authenticate with a local one the backend does not accept."""
    cloud_key = secret(environ, "EDGE_API_KEY")
    local_key = secret(environ, "GATEWAY_LOCAL_KEY")
    if len(local_key.encode()) < MIN_LOCAL_KEY_BYTES:
        raise ValueError(
            f"GATEWAY_LOCAL_KEY needs at least {MIN_LOCAL_KEY_BYTES} bytes"
        )
    if hmac.compare_digest(local_key.encode(), cloud_key.encode()):
        raise ValueError("GATEWAY_LOCAL_KEY must differ from the cloud EDGE_API_KEY")
    host = environ.get("GATEWAY_HOST", DEFAULT_GATEWAY_HOST)
    try:
        loopback = host == "localhost" or ipaddress.ip_address(host).is_loopback
    except ValueError:
        loopback = False
    if not loopback:
        # The local listener has no TLS: producers must share this host.
        raise ValueError("GATEWAY_HOST must be a loopback address")
    try:
        port = int(environ.get("GATEWAY_PORT", "8001"))
    except ValueError:
        port = 0
    if not 0 < port < 65536:
        raise ValueError("GATEWAY_PORT must be a TCP port")
    try:
        max_in_flight = validate_ack_window(int(environ.get("EDGE_MAX_IN_FLIGHT", "8")))
    except ValueError:
        raise ValueError("EDGE_MAX_IN_FLIGHT must be an integer from 1 to 8") from None

    def positive(name, default, kind):
        try:
            value = kind(environ.get(name, default))
        except ValueError:
            value = 0
        if not value > 0:
            raise ValueError(f"{name} must be a positive number")
        return value

    return Settings(
        retention=Retention(
            synced_hours=positive(
                "GATEWAY_SYNCED_RETENTION_HOURS", SYNCED_RETENTION_HOURS, float
            ),
            metrics_days=positive(
                "GATEWAY_METRICS_RETENTION_DAYS", METRICS_RETENTION_DAYS, float
            ),
            max_db_bytes=positive("GATEWAY_MAX_DB_BYTES", MAX_DB_BYTES, int),
        ),
        cloud_url=environ.get("SERVER_WS_URL", "wss://api.estaciona.tech/ws/edge"),
        cloud_key=cloud_key,
        local_key=local_key,
        host=host,
        port=port,
        max_in_flight=max_in_flight,
    )


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
                "source_payload",
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
            # Pending rows are a small, moving minority of a large table.
            conn.execute(
                "CREATE INDEX IF NOT EXISTS fallback_pending ON fallback_events(id) WHERE synced = 0"
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


def pending_deliveries(db_path, *, after_id=0, limit=None):
    with closing(sqlite3.connect(db_path)) as conn:
        return conn.execute(
            "SELECT id, event_uuid, message_text FROM fallback_events "
            "WHERE synced = 0 AND id > ? ORDER BY id LIMIT ?",
            (after_id, -1 if limit is None else limit),
        ).fetchall()


def _delete_batches(db_path, statement, cutoff):
    deleted = 0
    with closing(sqlite3.connect(db_path)) as conn:
        while True:
            with conn:
                count = conn.execute(statement, (cutoff, RETENTION_BATCH)).rowcount
            deleted += count
            if count < RETENTION_BATCH:
                break
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    return deleted


def _cutoff(age):
    moment = datetime.datetime.now(datetime.UTC) - age
    # SQLite CURRENT_TIMESTAMP format, so text comparison orders correctly.
    return moment.strftime("%Y-%m-%d %H:%M:%S")


def purge_synced_events(db_path, retention):
    """Delete confirmed events older than `retention`; pending ones never."""
    return _delete_batches(
        db_path,
        "DELETE FROM fallback_events WHERE id IN (SELECT id FROM fallback_events "
        "WHERE synced = 1 AND created_at < ? ORDER BY id LIMIT ?)",
        _cutoff(retention),
    )


def purge_metrics(metrics_path, retention):
    return _delete_batches(
        metrics_path,
        "DELETE FROM metrics WHERE id IN (SELECT id FROM metrics "
        "WHERE created_at < ? ORDER BY id LIMIT ?)",
        _cutoff(retention),
    )


def used_bytes(db_path):
    """Bytes in use: deleted rows leave free pages that new rows reuse, so the
    file size alone would never shrink below the budget again."""
    with closing(sqlite3.connect(db_path)) as conn:
        pages = conn.execute("PRAGMA page_count").fetchone()[0]
        free = conn.execute("PRAGMA freelist_count").fetchone()[0]
        size = conn.execute("PRAGMA page_size").fetchone()[0]
    wal = f"{db_path}-wal"
    return (pages - free) * size + (os.path.getsize(wal) if os.path.exists(wal) else 0)


def apply_retention(db_path, metrics_path, retention):
    purge_synced_events(db_path, datetime.timedelta(hours=retention.synced_hours))
    purge_metrics(metrics_path, datetime.timedelta(days=retention.metrics_days))


def has_room(db_path, metrics_path, retention):
    """Whether new events fit the budget, purging expired rows first if not."""
    if used_bytes(db_path) < retention.max_db_bytes:
        return True
    apply_retention(db_path, metrics_path, retention)
    return used_bytes(db_path) < retention.max_db_bytes


async def retention_loop(db_path, metrics_path, retention=Retention()):
    while True:
        try:
            await asyncio.to_thread(apply_retention, db_path, metrics_path, retention)
        except Exception as error:
            print(f"[ERROR] RETENTION_FAILED reason={error}", flush=True)
        await asyncio.sleep(RETENTION_INTERVAL_SECONDS)


# One statement for live traffic and for seeding: they cannot drift apart.
METRIC_INSERT = """INSERT INTO metrics (
    edge_id, spot_id, status, detection_ts, edge_sent_ts, gateway_received_ts, event_uuid
) VALUES (?, ?, ?, ?, ?, ?, ?)"""


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
            METRIC_INSERT,
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


async def receive_ack(connection, in_flight):
    deadline = min(in_flight.values())
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
            and isinstance(ack.get("event_id"), str)
            and ack["event_id"] in in_flight
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
    return hmac.compare_digest(parts[1].encode(), expected_key.encode())


async def handler(
    websocket, db_path, metrics_path, expected_key, sync_event, retention=None
):
    if not is_authorized(websocket.request.headers, expected_key):
        await websocket.close(1008, "Unauthorized")
        return

    print(
        f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] EDGE_CONNECTED",
        flush=True,
    )
    local_delivery = websocket.request.headers.get("X-Estaciona-Local-Delivery") == "1"
    if local_delivery:
        await websocket.send(json.dumps({"type": "LOCAL_READY", "delivery_version": 1}))
    db_conn = sqlite3.connect(db_path)
    metrics_conn = sqlite3.connect(metrics_path)

    try:
        while True:
            messages = [await websocket.recv()]
            loop = asyncio.get_running_loop()
            deadline = loop.time() + LOCAL_BATCH_MAX_AGE_SECONDS
            batch_bytes = len(
                messages[0].encode("utf-8")
                if isinstance(messages[0], str)
                else messages[0]
            )
            disconnected = False

            # A byte threshold can be crossed by the final frame; the websocket's
            # max_size bounds each frame. Count and age never depend on idle input.
            while (
                len(messages) < LOCAL_BATCH_MAX_MESSAGES
                and batch_bytes < LOCAL_BATCH_FLUSH_BYTES
            ):
                remaining = deadline - loop.time()
                if remaining <= 0:
                    break
                try:
                    msg = await asyncio.wait_for(
                        websocket.recv(),
                        timeout=min(LOCAL_BATCH_IDLE_SECONDS, remaining),
                    )
                    messages.append(msg)
                    batch_bytes += len(
                        msg.encode("utf-8") if isinstance(msg, str) else msg
                    )
                except asyncio.TimeoutError:
                    break
                except Exception:
                    disconnected = True
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
                source_payload = None
                event_uuid = str(uuid.uuid4())
                if payload.get("type") == "LOCAL_EVENT":
                    if (
                        not local_delivery
                        or type(payload.get("version")) is not int
                        or payload["version"] != 1
                        or not isinstance(payload.get("payload"), dict)
                    ):
                        await websocket.close(1008, "Invalid local delivery envelope")
                        return
                    try:
                        event_uuid = str(uuid.UUID(payload["event_id"]))
                    except KeyError, ValueError, TypeError, AttributeError:
                        await websocket.close(1008, "Invalid event UUID")
                        return
                    payload = payload["payload"]
                    source_payload = json.dumps(
                        payload, sort_keys=True, separators=(",", ":")
                    )
                elif local_delivery:
                    await websocket.close(1008, "Local delivery envelope required")
                    return
                now_str = (
                    datetime.datetime.now(datetime.UTC)
                    .isoformat(timespec="milliseconds")
                    .replace("+00:00", "Z")
                )
                payload["gateway_received_at"] = now_str

                events_to_insert.append(
                    (json.dumps(payload), event_uuid, source_payload)
                )

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

            if (
                events_to_insert
                and retention is not None
                and not await asyncio.to_thread(
                    has_room, db_path, metrics_path, retention
                )
            ):
                # Nothing of this batch is acknowledged: the producer keeps it
                # in its own bounded outbox and retries later.
                print("[ERROR] GATEWAY_STORAGE_FULL", flush=True)
                await websocket.close(1013, "Gateway storage full")
                return

            local_acks = []
            fresh_metrics = []
            if events_to_insert:
                with db_conn:
                    for event, metric in zip(
                        events_to_insert, metrics_to_insert, strict=True
                    ):
                        message_text, event_uuid, source_payload = event
                        inserted = db_conn.execute(
                            "INSERT INTO fallback_events (message_text, event_uuid, source_payload) VALUES (?, ?, ?) ON CONFLICT(event_uuid) DO NOTHING",
                            event,
                        ).rowcount
                        if inserted:
                            fresh_metrics.append(metric)
                        if source_payload is not None:
                            existing = db_conn.execute(
                                "SELECT source_payload FROM fallback_events WHERE event_uuid=?",
                                (event_uuid,),
                            ).fetchone()[0]
                            outcome = (
                                ("persisted" if inserted else "duplicate")
                                if existing == source_payload
                                else "rejected"
                            )
                            local_acks.append(
                                {
                                    "type": "LOCAL_ACK",
                                    "version": 1,
                                    "event_id": event_uuid,
                                    "outcome": outcome,
                                }
                            )
                # Both local persistence ACK and cloud delivery happen only after
                # commit. Metrics are diagnostic and cannot revoke persistence.
                sync_event.set()
                for ack in local_acks:
                    await asyncio.wait_for(websocket.send(json.dumps(ack)), 3)

            if fresh_metrics:
                with metrics_conn:
                    metrics_conn.executemany(METRIC_INSERT, fresh_metrics)

            if disconnected:
                break

    except Exception as e:
        print(
            f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] EDGE_DISCONNECTED reason={e}",
            flush=True,
        )
    finally:
        db_conn.close()
        metrics_conn.close()


def validate_ack_window(value):
    if type(value) is not int or not 1 <= value <= 8:
        raise ValueError("EDGE_MAX_IN_FLIGHT must be an integer from 1 to 8")
    return value


async def sync_loop(
    db_path, metrics_path, cloud_url, api_key, sync_event, *, max_in_flight=8
):
    validate_ack_window(max_in_flight)
    cloud_conn = None
    headers = {"Authorization": f"Bearer {api_key}"}
    try:
        while True:
            try:
                events = pending_deliveries(db_path, limit=64)
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

                deferred = False
                in_flight = {}
                cursor = 0
                exhausted = False
                loop = asyncio.get_running_loop()
                while not exhausted or in_flight:
                    while not exhausted and len(in_flight) < max_in_flight:
                        if in_flight and min(in_flight.values()) <= loop.time():
                            raise TimeoutError("Cloud ACK deadline expired")
                        if not events:
                            events = pending_deliveries(
                                db_path, after_id=cursor, limit=64
                            )
                            if not events:
                                exhausted = True
                                break
                        row_id, event_uuid, message_text = events.pop(0)
                        cursor = row_id
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
                        if payload.get("type") not in durable_events:
                            # Advance the cursor without consuming a window slot.
                            deferred = True
                            await asyncio.sleep(0)
                            continue
                        now_str = (
                            datetime.datetime.now(datetime.UTC)
                            .isoformat(timespec="milliseconds")
                            .replace("+00:00", "Z")
                        )
                        deadline = loop.time() + ACK_TIMEOUT_SECONDS
                        send_deadline = min([deadline, *in_flight.values()])
                        await asyncio.wait_for(
                            cloud_conn.send(
                                encode_edge_event(event_uuid, payload, now_str)
                            ),
                            timeout=max(0, send_deadline - loop.time()),
                        )
                        in_flight[event_uuid] = deadline
                        record_forwarding(db_path, metrics_path, event_uuid, now_str)
                    if in_flight:
                        ack = await receive_ack(cloud_conn, in_flight)
                        event_uuid = ack["event_id"]
                        acknowledge_event(
                            db_path,
                            metrics_path,
                            event_uuid,
                            ack["outcome"],
                            ack["reason"],
                        )
                        del in_flight[event_uuid]
                        print(
                            f"[INFO] CLOUD_ACK event_id={event_uuid} outcome={ack['outcome']} duplicate={ack['duplicate']}",
                            flush=True,
                        )
                if deferred:
                    # Renegotiate after backoff; unsupported rows retain their UUID.
                    await cloud_conn.close()
                    cloud_conn = None
                    await asyncio.sleep(RETRY_DELAY_SECONDS)
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


async def main():
    from dotenv import load_dotenv

    load_dotenv(os.path.join(os.path.dirname(__file__), "../.env"))
    settings = load_settings(os.environ)

    db_path = "local_fallback.db"
    metrics_path = "metrics.db"

    init_db(db_path)
    init_metrics_db(metrics_path)

    print(
        f"{datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%d %H:%M:%S')} [INFO] STARTING_GATEWAY host={settings.host} port={settings.port}",
        flush=True,
    )

    sync_event = asyncio.Event()

    asyncio.create_task(
        sync_loop(
            db_path,
            metrics_path,
            settings.cloud_url,
            settings.cloud_key,
            sync_event,
            max_in_flight=settings.max_in_flight,
        )
    )

    asyncio.create_task(retention_loop(db_path, metrics_path, settings.retention))

    async def ws_handler(websocket, *args, **kwargs):
        await handler(
            websocket,
            db_path,
            metrics_path,
            settings.local_key,
            sync_event,
            settings.retention,
        )

    async with websockets.serve(ws_handler, settings.host, settings.port):
        await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())
