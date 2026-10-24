# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Durable local delivery; gateway persistence is distinct from a cloud ACK."""

import asyncio
from contextlib import closing
import json
from pathlib import Path
import sqlite3
from uuid import uuid4

import websockets


class LocalOutbox:
    def __init__(self, path, max_events=10000, max_bytes=32 * 1024 * 1024):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.max_events, self.max_bytes = max_events, max_bytes
        with closing(self.connect()) as conn, conn:
            conn.execute("PRAGMA journal_mode=WAL")
            conn.execute(
                "CREATE TABLE IF NOT EXISTS local_outbox (sequence INTEGER PRIMARY KEY AUTOINCREMENT,event_id TEXT UNIQUE NOT NULL,payload TEXT NOT NULL,bytes INTEGER NOT NULL)"
            )

    def connect(self):
        conn = sqlite3.connect(self.path)
        conn.execute("PRAGMA synchronous=FULL")
        return conn

    def enqueue(self, payload_text):
        return self.enqueue_many((payload_text,))[0]

    def enqueue_many(self, payload_texts):
        """Commit a bounded, ordered batch, with an independent UUID per event."""
        rows, batch_bytes = [], 0
        for text in payload_texts:
            payload = json.loads(text)
            if not isinstance(payload, dict):
                raise ValueError("Local delivery requires an object payload")
            text = json.dumps(payload, separators=(",", ":"))
            size = len(text.encode("utf-8"))
            if size > 512 * 1024:
                raise BufferError("Local delivery payload exceeds 512 KiB")
            batch_bytes += size
            if len(rows) >= self.max_events or batch_bytes > self.max_bytes:
                raise BufferError("Batch exceeds vision outbox capacity")
            rows.append((str(uuid4()), text, size))
        if not rows:
            return []
        with closing(self.connect()) as conn, conn:
            conn.execute("BEGIN IMMEDIATE")
            count, total = conn.execute(
                "SELECT COUNT(*),COALESCE(SUM(bytes),0) FROM local_outbox"
            ).fetchone()
            if (
                count + len(rows) > self.max_events
                or total + batch_bytes > self.max_bytes
            ):
                raise BufferError(
                    "Vision outbox full; restore delivery before continuing"
                )
            conn.executemany(
                "INSERT INTO local_outbox(event_id,payload,bytes) VALUES (?,?,?)",
                rows,
            )
        return [row[0] for row in rows]

    def first(self):
        with closing(self.connect()) as conn:
            row = conn.execute(
                "SELECT event_id,payload FROM local_outbox ORDER BY sequence LIMIT 1"
            ).fetchone()
        if row is None:
            return None
        return {
            "type": "LOCAL_EVENT",
            "version": 1,
            "event_id": row[0],
            "payload": json.loads(row[1]),
        }

    def pending_count(self):
        """Bounded diagnostic read; never changes delivery/ACK state."""
        with closing(sqlite3.connect(self.path, timeout=0.05)) as conn:
            return conn.execute("SELECT COUNT(*) FROM local_outbox").fetchone()[0]

    def acknowledge(self, event_id):
        with closing(self.connect()) as conn, conn:
            conn.execute("DELETE FROM local_outbox WHERE event_id=?", (event_id,))


class LocalDelivery:
    def __init__(self, url, outbox):
        self.url, self.outbox = url, outbox

    async def connect(self, headers):
        ws = await websockets.connect(
            self.url,
            additional_headers={**headers, "X-Estaciona-Local-Delivery": "1"},
            open_timeout=3,
        )
        try:
            ready = json.loads(await asyncio.wait_for(ws.recv(), 3))
            if (
                not isinstance(ready, dict)
                or ready.get("type") != "LOCAL_READY"
                or type(ready.get("delivery_version")) is not int
                or ready["delivery_version"] != 1
            ):
                raise ValueError("Gateway does not support durable local delivery")
        except BaseException:
            await ws.close()
            raise
        return ws

    async def send(self, ws, payload_text, headers):
        if payload_text is not None:
            # Oversized individual frames cannot be fixed by draining the queue.
            size = len(
                json.dumps(json.loads(payload_text), separators=(",", ":")).encode(
                    "utf-8"
                )
            )
            if size > min(512 * 1024, self.outbox.max_bytes):
                raise BufferError("Local delivery payload exceeds outbox byte budget")
            while True:
                try:
                    self.outbox.enqueue(payload_text)
                    break
                except BufferError:
                    print(
                        "[CONN] Vision paused: outbox full; draining before accepting transition"
                    )
                    ws = await self.send(ws, None, headers)
                    if self.outbox.first() is not None:
                        await asyncio.sleep(1)
        for attempt in range(2):
            try:
                if ws is None:
                    ws = await self.connect(headers)
                # Bound catch-up work in the inference loop as well as storage.
                for _ in range(64):
                    event = self.outbox.first()
                    if event is None:
                        break
                    await asyncio.wait_for(ws.send(json.dumps(event)), 3)
                    ack = json.loads(await asyncio.wait_for(ws.recv(), 3))
                    if (
                        not isinstance(ack, dict)
                        or ack.get("type") != "LOCAL_ACK"
                        or type(ack.get("version")) is not int
                        or ack["version"] != 1
                        or ack.get("event_id") != event["event_id"]
                        or ack.get("outcome") not in ("persisted", "duplicate")
                    ):
                        raise ValueError("Invalid or rejected local persistence ACK")
                    self.outbox.acknowledge(event["event_id"])
                return ws
            except Exception as error:
                print(f"[CONN] Local delivery pending: {error}")
                if ws is not None:
                    try:
                        await ws.close()
                    except Exception:
                        pass
                ws = None
        return None
