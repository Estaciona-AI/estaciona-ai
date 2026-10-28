# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import asyncio
import hashlib
import json
from pathlib import Path
from unittest.mock import AsyncMock, Mock

import numpy as np
import pytest

import client
from delivery import LocalDelivery, LocalOutbox

REAL_VIDEO_CAPTURE = client.cv2.VideoCapture


@pytest.fixture
def runtime(tmp_path, monkeypatch):
    monkeypatch.setattr(client, "GATEWAY_LOCAL_KEY", "test-only")
    monkeypatch.setattr(client, "VIDEO_PATH", "fixture.mp4")
    monkeypatch.setattr(
        client, "load_spots", lambda _: {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}
    )
    delivery = LocalDelivery(client.WS_URL, LocalOutbox(tmp_path / "outbox.db"))
    monkeypatch.setattr(client, "DELIVERY", delivery)
    socket = AsyncMock()

    async def ack():
        event = json.loads(socket.send.call_args.args[0])
        return json.dumps(
            {
                "type": "LOCAL_ACK",
                "version": 1,
                "event_id": event["event_id"],
                "outcome": "persisted",
            }
        )

    socket.recv.side_effect = ack
    monkeypatch.setattr(client, "connect_ws", AsyncMock(return_value=socket))
    capture = Mock()
    capture.isOpened.return_value = True
    capture.read.side_effect = lambda: (True, np.zeros((10, 10, 3), dtype=np.uint8))
    monkeypatch.setattr(client.cv2, "VideoCapture", lambda _: capture)
    detector = Mock()
    detector.detect.return_value = [(0, 0, 9, 9)]
    monkeypatch.setattr(client, "load_detector", lambda *args, **kwargs: detector)
    return capture, socket, detector, delivery


@pytest.mark.parametrize("headless", [False, True])
def test_visual_and_headless_run_same_pipeline_and_durable_delivery(
    runtime, monkeypatch, headless
):
    capture, socket, detector, delivery = runtime
    gui_calls = {}
    for name in (
        "namedWindow",
        "resizeWindow",
        "imshow",
        "waitKey",
        "destroyAllWindows",
        "putText",
        "polylines",
    ):
        gui_calls[name] = (
            Mock(side_effect=AssertionError(f"headless called {name}"))
            if headless
            else Mock()
        )
        monkeypatch.setattr(client.cv2, name, gui_calls[name])
    if not headless:
        gui_calls["waitKey"].side_effect = [0, 0, ord("q")]
    sleeps = 0

    async def stop_headless(_):
        nonlocal sleeps
        sleeps += 1
        if headless and sleeps == 3:
            raise asyncio.CancelledError

    monkeypatch.setattr(client.asyncio, "sleep", stop_headless)
    if headless:
        with pytest.raises(asyncio.CancelledError):
            asyncio.run(client.main(["--headless"]))
    else:
        asyncio.run(client.main([]))
    assert detector.detect.call_count == 3
    assert socket.send.await_count == 1
    event = json.loads(socket.send.call_args.args[0])
    assert event["type"] == "LOCAL_EVENT"
    assert event["payload"]["type"] == "SPOT_UPDATE"
    assert event["payload"]["spot_id"] == "A"
    assert event["payload"]["status"] == "occupied"
    assert event["payload"]["camera_id"] == client.CAMERA_ID
    assert event["payload"]["confidence"] == pytest.approx(0.64)
    assert delivery.outbox.first() is None
    capture.release.assert_called_once()
    socket.close.assert_awaited_once()
    if headless:
        assert not any(call.called for call in gui_calls.values())
    else:
        assert gui_calls["imshow"].call_count == 3
        gui_calls["destroyAllWindows"].assert_called_once()


@pytest.mark.parametrize(
    "failure", ["open", "first_frame", "model", "spots", "inference", "send"]
)
def test_runtime_releases_resources_on_failure(runtime, monkeypatch, failure):
    capture, socket, detector, _ = runtime
    if failure == "open":
        capture.isOpened.return_value = False
    elif failure == "first_frame":
        capture.read.return_value = (False, None)
        capture.read.side_effect = None
    elif failure == "model":
        monkeypatch.setattr(
            client, "load_detector", Mock(side_effect=ValueError("model failed"))
        )
    elif failure == "spots":
        monkeypatch.setattr(
            client, "load_spots", Mock(side_effect=ValueError("mapping failed"))
        )
    elif failure == "inference":
        detector.detect.side_effect = ValueError("inference failed")
    else:
        monkeypatch.setattr(
            client, "safe_send", AsyncMock(side_effect=ValueError("send failed"))
        )
    if failure in ("open", "first_frame"):
        asyncio.run(client.main(["--headless"]))
    else:
        with pytest.raises(ValueError):
            asyncio.run(client.main(["--headless"]))
    if failure not in ("model", "spots"):
        capture.release.assert_called_once()
    socket.close.assert_awaited_once()


def test_legacy_positional_mapping_remains_available(runtime, monkeypatch):
    load = Mock(side_effect=ValueError("stop after parsing"))
    monkeypatch.setattr(client, "load_spots", load)
    with pytest.raises(ValueError):
        asyncio.run(client.main(["custom-spots.json", "--headless"]))
    load.assert_called_once_with("custom-spots.json")


def test_headless_option_is_available_without_credentials(monkeypatch, capsys):
    monkeypatch.setattr(client, "GATEWAY_LOCAL_KEY", None)
    with pytest.raises(SystemExit) as error:
        asyncio.run(client.main(["--help"]))
    assert error.value.code == 0
    assert "--headless" in capsys.readouterr().out


def test_headless_reads_real_video_and_persists_observation(
    runtime, tmp_path, monkeypatch
):
    import cv2

    _, socket, detector, delivery = runtime
    # Keep this an actual decode integration, without GPU or a GUI dependency.
    path = tmp_path / "source.avi"
    writer = cv2.VideoWriter(str(path), cv2.VideoWriter_fourcc(*"MJPG"), 10, (64, 64))
    assert writer.isOpened()
    try:
        for _ in range(3):
            writer.write(np.zeros((64, 64, 3), dtype=np.uint8))
    finally:
        writer.release()
    # runtime replaces VideoCapture; restore the real constructor for this test.
    monkeypatch.setattr(client.cv2, "VideoCapture", REAL_VIDEO_CAPTURE)
    monkeypatch.setattr(client, "VIDEO_PATH", str(path))
    monkeypatch.setattr(
        client.cv2, "namedWindow", Mock(side_effect=AssertionError("GUI"))
    )
    monkeypatch.setattr(client.cv2, "imshow", Mock(side_effect=AssertionError("GUI")))
    monkeypatch.setattr(client.cv2, "waitKey", Mock(side_effect=AssertionError("GUI")))

    async def stop(_):
        raise asyncio.CancelledError

    monkeypatch.setattr(client.asyncio, "sleep", stop)
    with pytest.raises(asyncio.CancelledError):
        asyncio.run(client.main(["--headless"]))
    assert detector.detect.call_count == 1
    assert detector.detect.call_args.args[0].shape == (64, 64, 3)
    assert json.loads(socket.send.call_args.args[0])["payload"]["status"] == "occupied"
    assert delivery.outbox.first() is None
    socket.close.assert_awaited_once()


@pytest.mark.parametrize(
    "url", ["ws://192.168.0.20:8001/ws/edge", "http://localhost:8001/ws/edge"]
)
def test_legacy_client_refuses_a_plain_gateway_off_this_host(runtime, monkeypatch, url):
    monkeypatch.setattr(client, "WS_URL", url)
    connect = Mock()
    monkeypatch.setattr(client, "connect_ws", connect)
    with pytest.raises(RuntimeError, match="LOCAL_WS_URL"):
        asyncio.run(client.main(["--headless"]))
    connect.assert_not_called()


def test_legacy_client_requires_the_local_gateway_key(runtime, monkeypatch):
    monkeypatch.setattr(client, "GATEWAY_LOCAL_KEY", None)
    with pytest.raises(RuntimeError, match="GATEWAY_LOCAL_KEY"):
        asyncio.run(client.main(["--headless"]))


def test_default_outbox_is_per_camera_and_created_once(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(client, "DELIVERY", None)
    monkeypatch.setattr(client, "CAMERA_ID", "cam-07")
    monkeypatch.delenv("VISION_OUTBOX_PATH", raising=False)
    delivery = client.local_delivery()
    key = hashlib.sha256(b"cam-07").hexdigest()[:16]
    assert delivery.url == client.WS_URL
    assert delivery.outbox.path == Path("data") / f"vision-outbox-{key}.db"
    assert (tmp_path / "data" / f"vision-outbox-{key}.db").is_file()
    assert client.local_delivery() is delivery


def test_outbox_path_can_be_set_by_the_environment(tmp_path, monkeypatch):
    monkeypatch.setattr(client, "DELIVERY", None)
    monkeypatch.setenv("VISION_OUTBOX_PATH", str(tmp_path / "custom" / "outbox.db"))
    assert client.local_delivery().outbox.path == tmp_path / "custom" / "outbox.db"
