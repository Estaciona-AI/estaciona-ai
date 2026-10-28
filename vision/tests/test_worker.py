# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import asyncio
import datetime
from dataclasses import replace
import importlib
import json
import threading
import time
from types import SimpleNamespace
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


def test_heartbeat_snapshot_is_atomically_visible_with_observation_timestamp(
    tmp_path, monkeypatch
):
    import worker
    from metrics import Metrics

    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    clock = SimpleNamespace(now=0.0)
    monkeypatch.setattr(
        worker,
        "time",
        SimpleNamespace(monotonic=lambda: clock.now, perf_counter=time.perf_counter),
    )
    timestamp = "2026-10-22T10:00:00.123456Z"
    visible_counts = []
    statuses = {f"A-{index:02}": "free" for index in range(20)}

    class Capture(SyntheticCapture):
        def latest(self, *, after, max_age):
            assert after == 0
            return CapturedFrame(
                1, 1, np.zeros((10, 10, 3), dtype=np.uint8), 0.0, timestamp
            )

    class ObservedOutbox(LocalOutbox):
        def enqueue_many(self, payloads):
            ids = super().enqueue_many(payloads)
            visible_counts.append(LocalOutbox(self.path).pending_count())
            return ids

    class Pipeline:
        def __init__(self, *args, **kwargs):
            pass

        def process(self, *args, **kwargs):
            clock.now = 31.0
            return SimpleNamespace(spots=())

        def confirmed_statuses(self):
            return statuses

    monkeypatch.setattr(worker, "LatestCapture", Capture)
    monkeypatch.setattr(worker, "LocalOutbox", ObservedOutbox)
    monkeypatch.setattr(worker, "VisionPipeline", Pipeline)
    model = Mock()
    model.metadata.return_value = {"adapter": "fixture"}
    monkeypatch.setattr(worker, "load_detector", lambda *args, **kwargs: model)

    async def offline(*args, **kwargs):
        raise OSError("offline")

    monkeypatch.setattr(worker.LocalDelivery, "connect", offline)
    cfg = config(tmp_path)
    cfg = replace(
        cfg,
        inputs=(
            replace(
                cfg.inputs[0],
                mapping=Mapping(
                    10,
                    10,
                    {spot_id: [[0, 0], [9, 0], [9, 9]] for spot_id in statuses},
                ),
            ),
        ),
    )
    stop = threading.Event()

    def report(status, *args):
        if status == worker.Status.ACTIVE:
            stop.set()

    metrics_path = tmp_path / "metrics.json"
    asyncio.run(
        worker.run_input(
            cfg,
            cfg.inputs[0],
            stop,
            report,
            metrics=Metrics(),
            metrics_path=metrics_path,
        )
    )
    assert visible_counts == [20]
    outbox = LocalOutbox(
        input_directory(cfg.state_dir, cfg.scope, "cam-A") / "outbox.db"
    )
    for spot_id in statuses:
        event = outbox.first()
        assert event["payload"]["spot_id"] == spot_id
        assert event["payload"]["timestamp"] == timestamp
        assert event["payload"]["camera_id"] == "cam-A"
        assert event["payload"]["status"] == statuses[spot_id]
        outbox.acknowledge(event["event_id"])
    measured = json.loads(metrics_path.read_text())
    assert measured["stages"]["enqueue_batch_ms"]["count"] == 1
    assert measured["counters"]["heartbeat_events_enqueued"] == 20


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
    loader = Mock(return_value=model)
    monkeypatch.setattr(module, "load_detector", loader)
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
            calibrated = replace(
                cfg.inputs[0].detector, confidence=0.3, gamma=1.0, tile_size=512
            )
            cfg = replace(cfg, inputs=(replace(cfg.inputs[0], detector=calibrated),))
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
    kwargs = dict(loader.call_args.kwargs)
    assert set(kwargs.pop("mapping").spots) == {"A"}
    assert kwargs == {
        "device": "cpu",
        "tile_size": 512,
        "tile_overlap": 0.25,
        "tile_scope": "frame",
        "provider": "default",
        "precision": "fp32",
        "engine_cache": None,
        "expected_sha256": None,
    }
    assert model.detect.call_args.kwargs["confidence"] == 0.3
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


@pytest.mark.parametrize("load_failure", [False, True])
def test_delivery_drains_while_model_is_starting_or_failed(
    tmp_path, monkeypatch, load_failure
):
    module = importlib.import_module("worker")
    if load_failure:
        from types import SimpleNamespace

        async def pump_finishes_first(tasks, **kwargs):
            await tasks[1]
            return {tasks[1]}, {tasks[0]}

        # Exercise the shutdown ordering seen on CI, without modifying asyncio
        # globally (the real WebSocket server also uses it).
        scheduler = SimpleNamespace(**vars(asyncio))
        scheduler.wait = pump_finishes_first
        monkeypatch.setattr(module, "asyncio", scheduler)
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    monkeypatch.setattr(module, "LatestCapture", SyntheticCapture)
    entered, released = threading.Event(), threading.Event()
    stop = threading.Event()

    def loader(*args, **kwargs):
        entered.set()
        if load_failure:
            raise ValueError("invalid model fixture")
        assert released.wait(3), "delivery was blocked by model startup"
        model = Mock()
        model.detect.return_value = []
        return model

    monkeypatch.setattr(module, "load_detector", loader)
    received = []

    async def scenario():
        async def server(ws):
            await ws.send(json.dumps({"type": "LOCAL_READY", "delivery_version": 1}))
            event = json.loads(await ws.recv())
            received.append(event["event_id"])
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
            released.set()
            await asyncio.sleep(0.1)
            stop.set()

        async with websockets.serve(server, "127.0.0.1", 0) as listener:
            cfg = config(
                tmp_path,
                f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws/edge",
            )
            outbox = LocalOutbox(
                input_directory(cfg.state_dir, cfg.scope, "cam-A") / "outbox.db"
            )
            event_id = outbox.enqueue('{"type":"SPOT_UPDATE","spot_id":"previous"}')
            try:
                if load_failure:
                    with pytest.raises(ValueError, match="invalid model"):
                        await asyncio.wait_for(
                            module.run_input(
                                cfg, cfg.inputs[0], stop, lambda *args: None
                            ),
                            5,
                        )
                else:
                    await asyncio.wait_for(
                        module.run_input(cfg, cfg.inputs[0], stop, lambda *args: None),
                        5,
                    )
            finally:
                released.set()
            assert event_id in received

    asyncio.run(scenario())


@pytest.mark.parametrize("preview_enabled", [False, True])
def test_worker_preview_matches_processed_frame_without_extra_capture(
    tmp_path, monkeypatch, preview_enabled
):
    import multiprocessing
    import cv2
    from inspection import PreviewChannel

    module = importlib.import_module("worker")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    capture = SyntheticCapture(None)
    captures = []
    monkeypatch.setattr(
        module, "LatestCapture", lambda source: (captures.append(source), capture)[1]
    )
    model = Mock()
    model.detect.return_value = [(0, 0, 9, 9)]
    monkeypatch.setattr(module, "load_detector", lambda *args, **kwargs: model)
    stop = threading.Event()
    channel = (
        PreviewChannel(multiprocessing.get_context("spawn"))
        if preview_enabled
        else None
    )
    if channel is not None:
        channel.request_until.value = time.monotonic() + 2
    else:
        monkeypatch.setattr(
            cv2, "imencode", lambda *_: pytest.fail("headless encoded an image")
        )

    def report(status, progress, sequence):
        if int(status) == 2:
            stop.set()

    async def scenario():
        cfg = config(tmp_path)
        await asyncio.wait_for(
            module.run_input(cfg, cfg.inputs[0], stop, report, preview=channel), 3
        )
        if channel is not None:
            snapshot = channel.read()
            assert snapshot["sequence"] == 1 and snapshot["epoch"] == 1
            assert snapshot["spots"][0]["id"] == "A"
            assert snapshot["spots"][0]["status"] == "occupied"
            assert snapshot["pending"] >= 0
            assert "disposable" not in json.dumps(snapshot)

    asyncio.run(scenario())
    assert len(captures) == 1
    assert capture.closed


@pytest.mark.parametrize("failure", ["exception", "encode_false", "buffer_full"])
def test_preview_failure_does_not_stop_occupancy_delivery(
    tmp_path, monkeypatch, failure
):
    import multiprocessing
    from inspection import PreviewChannel
    import cv2

    module = importlib.import_module("worker")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    monkeypatch.setattr(module, "LatestCapture", SyntheticCapture)
    model = Mock()
    model.detect.return_value = []
    monkeypatch.setattr(module, "load_detector", lambda *args, **kwargs: model)
    if failure == "exception":
        monkeypatch.setattr(
            cv2, "imencode", lambda *_: (_ for _ in ()).throw(cv2.error("fixture"))
        )
    elif failure == "encode_false":
        monkeypatch.setattr(cv2, "imencode", lambda *_: (False, None))
    else:
        monkeypatch.setattr(PreviewChannel, "write", lambda *args, **kwargs: False)
    stop = threading.Event()
    channel = PreviewChannel(multiprocessing.get_context("spawn"))
    channel.request_until.value = time.monotonic() + 2
    cfg = config(tmp_path)

    def report(status, *args):
        if int(status) == 2:
            stop.set()

    asyncio.run(module.run_input(cfg, cfg.inputs[0], stop, report, preview=channel))
    outbox = LocalOutbox(
        input_directory(cfg.state_dir, cfg.scope, "cam-A") / "outbox.db"
    )
    assert outbox.first()["payload"]["spot_id"] == "A"
    assert channel.error.value == 1


def test_shadow_worker_never_creates_outbox_or_requires_credentials(
    tmp_path, monkeypatch
):
    from metrics import Metrics
    import worker

    monkeypatch.delenv("TEST_EDGE_KEY", raising=False)
    monkeypatch.setattr(worker, "LatestCapture", SyntheticCapture)
    monkeypatch.setattr(
        worker,
        "LocalOutbox",
        lambda *args: pytest.fail("shadow created durable delivery"),
    )

    class Model:
        def detect(self, *args, **kwargs):
            return []

        def metadata(self):
            return {"adapter": "fixture", "providers": ["fixture"]}

    monkeypatch.setattr(worker, "load_detector", lambda *args, **kwargs: Model())
    stop = threading.Event()
    metrics = Metrics()
    path = tmp_path / "metrics.json"

    def report(status, *args):
        if status == worker.Status.ACTIVE:
            stop.set()

    cfg = config(tmp_path)
    asyncio.run(
        worker.run_input(
            cfg,
            cfg.inputs[0],
            stop,
            report,
            publish=False,
            metrics=metrics,
            metrics_path=path,
        )
    )
    snapshot = json.loads(path.read_text())
    assert snapshot["state"]["delivery"] == "shadow"
    assert snapshot["counters"]["processed"] == 1
    assert snapshot["state"]["pending"] is None
    assert not (tmp_path / "state").exists()
