# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import multiprocessing

import cv2
import numpy as np

import inspection
from capture import CapturedFrame
from occupancy import SpotObservation
from pipeline import FrameResult


def test_preview_requires_demand_and_never_modifies_capture(monkeypatch):
    clock = [10.0]
    monkeypatch.setattr(inspection.time, "monotonic", lambda: clock[0])
    channel = inspection.PreviewChannel(multiprocessing.get_context("spawn"))
    source = np.zeros((1080, 1920, 3), np.uint8)
    frame = CapturedFrame(42, 3, source, 9.8, "2026-10-21T12:00:00Z")
    result = FrameResult(
        ((20, 20, 80, 80),), (SpotObservation("A", "occupied", 0.5, True),)
    )
    publisher = inspection.PreviewPublisher(
        channel, {"A": [[10, 10], [90, 10], [90, 90], [10, 90]]}
    )
    encode = cv2.imencode
    calls = []
    monkeypatch.setattr(
        cv2, "imencode", lambda *args: (calls.append(1), encode(*args))[1]
    )
    publisher.publish(frame, result, latency_ms=20, fps=2.5, pending=3)
    assert not calls
    channel.request_until.value = 12
    publisher.publish(frame, result, latency_ms=20, fps=2.5, pending=3)
    snapshot = channel.read()
    assert snapshot["sequence"] == 42 and snapshot["epoch"] == 3
    assert snapshot["observed_at"] == frame.received_at
    assert snapshot["spots"][0]["status"] == "occupied"
    assert snapshot["pending"] == 3
    assert snapshot["width"] == 960 and snapshot["height"] == 540
    assert snapshot["jpeg"] and not source.any()
    publisher.publish(frame, result, latency_ms=20, fps=2.5, pending=3)
    assert len(calls) == 1
    clock[0] = 12.1
    publisher.publish(frame, result, latency_ms=20, fps=2.5, pending=3)
    assert len(calls) == 1


def test_switch_and_worker_replacement_discard_previous_snapshot(monkeypatch):
    clock = [10.0]
    monkeypatch.setattr(inspection.time, "monotonic", lambda: clock[0])
    ctx = multiprocessing.get_context("spawn")
    broker = inspection.PreviewBroker(("one", "two"))
    one, two = inspection.PreviewChannel(ctx), inspection.PreviewChannel(ctx)
    broker.register("one", one)
    broker.register("two", two)
    assert broker.frame("one")["frame"] is None
    one.write({"sequence": 1, "received_monotonic": 10})
    assert broker.frame("one")["frame"]["sequence"] == 1
    assert broker.frame("two")["frame"] is None
    assert one.request_until.value == 0
    assert broker.frame("one")["frame"] is None
    one.write({"sequence": 2, "received_monotonic": 10})
    broker.register("one", inspection.PreviewChannel(ctx))
    assert broker.frame("one")["frame"] is None
    clock[0] = 20
    assert broker.frame("one")["frame"] is None


def test_bounded_and_poisoned_snapshot_does_not_block_other_inputs():
    ctx = multiprocessing.get_context("spawn")
    channel = inspection.PreviewChannel(ctx)
    assert not channel.write({"jpeg": "x" * (inspection.MAX_SNAPSHOT + 1)})
    assert channel.read() is None
    channel.lock.acquire()
    try:
        assert channel.read() is None
    finally:
        channel.lock.release()


def test_expired_viewer_and_old_generation_cannot_republish_after_switch(monkeypatch):
    clock = [10.0]
    monkeypatch.setattr(inspection.time, "monotonic", lambda: clock[0])
    ctx = multiprocessing.get_context("spawn")
    channel = inspection.PreviewChannel(ctx)
    broker = inspection.PreviewBroker(("one",), max_age=1)
    broker.register("one", channel)
    broker.frame("one")
    generation = channel.generation.value
    channel.write({"sequence": 1, "received_monotonic": 8}, generation=generation)
    state = broker.frame("one")
    assert state["stale"] and state["age_ms"] == 2000
    clock[0] = 13
    assert broker.frame("one")["frame"] is None
    channel.write({"sequence": 2, "received_monotonic": 13}, generation=generation)
    assert broker.frame("one")["frame"] is None
