# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib
import time
from unittest.mock import Mock

import cv2
import numpy as np

from configuration import Source


def wait_for(predicate, timeout=3):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.005)
    raise AssertionError("capture did not make progress")


def test_latest_slot_is_bounded_and_stale_frames_are_not_returned():
    module = importlib.import_module("capture")
    slot = module.LatestFrame()
    for index in range(20):
        slot.publish(
            np.full((4, 4, 3), index, dtype=np.uint8),
            epoch=2,
            received_at="test",
            now=float(index),
        )
    item = slot.get(after=0, max_age=1, now=19)
    assert (item.sequence, item.epoch, item.frame[0, 0, 0]) == (20, 2, 19)
    assert slot.get(after=20, max_age=1, now=19) is None
    assert slot.get(after=0, max_age=1, now=21) is None
    slot.clear()
    assert slot.get(after=0, max_age=1, now=19) is None


def test_capture_reconnects_with_new_epoch_and_releases(monkeypatch):
    module = importlib.import_module("capture")
    source = Source("rtsp", env="TEST_RTSP")
    monkeypatch.setenv("TEST_RTSP", "rtsp://localhost/synthetic")
    broken, recovered = Mock(), Mock()
    broken.isOpened.return_value = recovered.isOpened.return_value = True
    broken.read.return_value = (False, None)
    recovered.read.side_effect = lambda: (True, np.zeros((4, 4, 3), dtype=np.uint8))
    calls = []

    def open_capture(*args):
        calls.append(args)
        return broken if len(calls) == 1 else recovered

    capture = module.LatestCapture(
        source, opener=open_capture, retry_min=0.01, retry_max=0.02
    )
    capture.start()
    try:
        item = wait_for(lambda: capture.latest(after=0, max_age=1))
        assert item.epoch == 2
        assert calls[0][1] == cv2.CAP_FFMPEG
        assert cv2.CAP_PROP_OPEN_TIMEOUT_MSEC in calls[0][2]
    finally:
        assert capture.close()
    broken.release.assert_called_once()
    recovered.release.assert_called_once()


def test_local_file_is_paced_and_non_looping_eof_stops(tmp_path):
    module = importlib.import_module("capture")
    path = tmp_path / "source.avi"
    writer = cv2.VideoWriter(str(path), cv2.VideoWriter_fourcc(*"MJPG"), 10, (32, 32))
    assert writer.isOpened()
    for _ in range(4):
        writer.write(np.zeros((32, 32, 3), dtype=np.uint8))
    writer.release()
    capture = module.LatestCapture(Source("file", path=path))
    start = time.monotonic()
    capture.start()
    try:
        wait_for(lambda: capture.ended)
        item = capture.latest(after=0, max_age=2)
        assert item.sequence == 4
        assert 0.25 <= time.monotonic() - start < 2
        assert item.received_at.endswith("Z")
    finally:
        assert capture.close()


def test_grouped_capture_limits_ffmpeg_decoder_threads(monkeypatch):
    from capture import LatestCapture

    monkeypatch.setenv("TEST_RTSP", "rtsp://localhost/synthetic")
    calls = []
    for source in (Source("rtsp", env="TEST_RTSP"), Source("file", path="fixture")):
        capture = LatestCapture(
            source, opener=lambda *args: calls.append(args), decoder_threads=1
        )
        capture._url = source.resolve()
        capture._open()
    assert all(call[1] == cv2.CAP_FFMPEG for call in calls)
    assert all(call[2][:2] == [cv2.CAP_PROP_N_THREADS, 1] for call in calls)
