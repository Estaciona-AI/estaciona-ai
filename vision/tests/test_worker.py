# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import asyncio
import datetime
import importlib
import json
import threading
import time
from unittest.mock import Mock
from uuid import uuid4

import numpy as np
import pytest
import websockets

from capture import CapturedFrame
from configuration import (
    DetectorProfile,
    InputProfile,
    Mapping,
    RuntimeConfiguration,
    Source,
)
from delivery import LocalOutbox
from storage import input_directory


def config(tmp_path, gateway="ws://127.0.0.1:8001/ws/edge"):
    item = InputProfile(
        "cam-A",
        Source("file", path=tmp_path / "unused"),
        DetectorProfile(tmp_path / "model.onnx", "cpu"),
        Mapping(10, 10, {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}),
        10,
    )
    return RuntimeConfiguration(
        str(uuid4()),
        str(uuid4()),
        "test",
        tmp_path / "state",
        gateway,
        "TEST_EDGE_KEY",
        (item,),
        2,
        30,
        120,
    )


class SyntheticCapture:
    ended = False
    error = None

    def __init__(self, source):
        self.closed = False

    def start(self):
        pass

    def latest(self, *, after, max_age):
        if after:
            return None
        return CapturedFrame(
            1,
            1,
            np.zeros((10, 10, 3), dtype=np.uint8),
            time.monotonic(),
            datetime.datetime.now(datetime.UTC)
            .isoformat(timespec="microseconds")
            .replace("+00:00", "Z"),
        )

    def close(self):
        self.closed = True
        return True


@pytest.mark.parametrize("lose_ack", [False, True])
def test_worker_replays_same_event_after_ack_loss(tmp_path, monkeypatch, lose_ack):
    module = importlib.import_module("worker")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    capture = SyntheticCapture(None)
    monkeypatch.setattr(module, "LatestCapture", lambda _: capture)
    model = Mock()
    model.detect.return_value = [(0, 0, 9, 9)]
    monkeypatch.setattr(module, "load_detector", lambda *args, **kwargs: model)
    received = []
    stop = threading.Event()

    async def scenario():
        async def server(ws):
            assert ws.request.headers["Authorization"] == "Bearer disposable"
            await ws.send(json.dumps({"type": "LOCAL_READY", "delivery_version": 1}))
            event = json.loads(await ws.recv())
            received.append(event)
            if lose_ack and len(received) == 1:
                await ws.close()
                return
            await ws.send(
                json.dumps(
                    {
                        "type": "LOCAL_ACK",
                        "version": 1,
                        "event_id": event["event_id"],
                        "outcome": "persisted",
                    }
                )
            )
            await asyncio.sleep(0.1)
            stop.set()

        async with websockets.serve(server, "127.0.0.1", 0) as listener:
            port = listener.sockets[0].getsockname()[1]
            cfg = config(tmp_path, f"ws://127.0.0.1:{port}/ws/edge")
            reports = []
            await asyncio.wait_for(
                module.run_input(
                    cfg, cfg.inputs[0], stop, lambda *args: reports.append(args)
                ),
                5,
            )
            outbox = LocalOutbox(
                input_directory(cfg.state_dir, cfg.scope, "cam-A") / "outbox.db"
            )
            assert outbox.first() is None
            assert reports

    asyncio.run(scenario())
    assert capture.closed
    assert len(received) == (2 if lose_ack else 1)
    if lose_ack:
        assert received[0] == received[1]
    assert received[-1]["payload"]["camera_id"] == "cam-A"
    assert received[-1]["payload"]["status"] == "occupied"


def test_worker_rejects_resolution_mismatch_without_events(tmp_path, monkeypatch):
    module = importlib.import_module("worker")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    cfg = config(tmp_path)

    class WrongSize(SyntheticCapture):
        def latest(self, **kwargs):
            frame = super().latest(**kwargs)
            return CapturedFrame(
                frame.sequence,
                frame.epoch,
                np.zeros((20, 20, 3), dtype=np.uint8),
                frame.received_monotonic,
                frame.received_at,
            )

    monkeypatch.setattr(module, "LatestCapture", WrongSize)
    monkeypatch.setattr(module, "load_detector", lambda *args, **kwargs: Mock())
    with pytest.raises(module.PermanentInputError):
        asyncio.run(
            module.run_input(cfg, cfg.inputs[0], threading.Event(), lambda *args: None)
        )
    assert (
        LocalOutbox(
            input_directory(cfg.state_dir, cfg.scope, "cam-A") / "outbox.db"
        ).first()
        is None
    )


def test_inference_does_not_block_delivery_of_previous_outbox(tmp_path, monkeypatch):
    module = importlib.import_module("worker")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    monkeypatch.setattr(module, "LatestCapture", SyntheticCapture)
    entered, released = threading.Event(), threading.Event()
    model = Mock()

    def detect(*args, **kwargs):
        entered.set()
        assert released.wait(3)
        return []

    model.detect.side_effect = detect
    monkeypatch.setattr(module, "load_detector", lambda *args, **kwargs: model)
    stop = threading.Event()

    async def scenario():
        async def server(ws):
            await ws.send(json.dumps({"type": "LOCAL_READY", "delivery_version": 1}))
            event = json.loads(await ws.recv())
            await ws.send(
                json.dumps(
                    {
                        "type": "LOCAL_ACK",
                        "version": 1,
                        "event_id": event["event_id"],
                        "outcome": "persisted",
                    }
                )
            )
            assert await asyncio.to_thread(entered.wait, 2)
            released.set()
            stop.set()

        async with websockets.serve(server, "127.0.0.1", 0) as listener:
            cfg = config(
                tmp_path,
                f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws/edge",
            )
            outbox = LocalOutbox(
                input_directory(cfg.state_dir, cfg.scope, "cam-A") / "outbox.db"
            )
            event_id = outbox.enqueue(
                json.dumps({"type": "SPOT_UPDATE", "spot_id": "old"})
            )
            try:
                await asyncio.wait_for(
                    module.run_input(cfg, cfg.inputs[0], stop, lambda *args: None), 5
                )
            finally:
                released.set()
            assert outbox.first() is None or outbox.first()["event_id"] != event_id

    asyncio.run(scenario())


def test_two_inputs_publish_distinct_spots_with_separate_outboxes(
    tmp_path, monkeypatch
):
    from dataclasses import replace

    module = importlib.import_module("worker")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    monkeypatch.setattr(module, "LatestCapture", SyntheticCapture)
    model = Mock()
    model.detect.return_value = []
    monkeypatch.setattr(module, "load_detector", lambda *args, **kwargs: model)
    stop = threading.Event()
    received = {}

    async def scenario():
        async def server(ws):
            await ws.send(json.dumps({"type": "LOCAL_READY", "delivery_version": 1}))
            event = json.loads(await ws.recv())
            payload = event["payload"]
            received[payload["camera_id"]] = payload["spot_id"]
            await ws.send(
                json.dumps(
                    {
                        "type": "LOCAL_ACK",
                        "version": 1,
                        "event_id": event["event_id"],
                        "outcome": "persisted",
                    }
                )
            )
            if len(received) == 2:
                await asyncio.sleep(0.1)
                stop.set()

        async with websockets.serve(server, "127.0.0.1", 0) as listener:
            cfg = config(
                tmp_path,
                f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws/edge",
            )
            second = replace(
                cfg.inputs[0],
                input_id="cam-B",
                mapping=Mapping(10, 10, {"B": [[0, 0], [9, 0], [9, 9], [0, 9]]}),
            )
            cfg = replace(cfg, inputs=(*cfg.inputs, second))
            await asyncio.wait_for(
                asyncio.gather(
                    *(
                        module.run_input(cfg, item, stop, lambda *args: None)
                        for item in cfg.inputs
                    )
                ),
                5,
            )
            directories = {
                input_directory(cfg.state_dir, cfg.scope, item.input_id)
                for item in cfg.inputs
            }
            assert len(directories) == 2
            assert all(
                LocalOutbox(directory / "outbox.db").first() is None
                for directory in directories
            )

    asyncio.run(scenario())
    assert received == {"cam-A": "A", "cam-B": "B"}
