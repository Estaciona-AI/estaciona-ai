# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Actual Docker/FFmpeg/MediaMTX/RTSP; detector and gateway protocol are fixtures."""

import asyncio
from dataclasses import replace
import json
import multiprocessing
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
import time
from unittest.mock import Mock

import pytest
import websockets

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/rtsp-lab"))
from capture import LatestCapture
from configuration import Source, load_configuration
from delivery import LocalOutbox
from inspection import PreviewBroker, PreviewChannel
from scenarios import generate
from services import IMAGE, LabSession
from storage import input_directory
import worker

pytestmark = pytest.mark.skipif(
    os.environ.get("ESTACIONA_RTSP_TEST") != "1",
    reason="dedicated RTSP lab job requires Docker",
)


def wait_for(predicate, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("RTSP lab did not reach expected state")


def test_rtsp_delivery_ack_loss_and_publisher_recovery(tmp_path, monkeypatch):
    media = tmp_path / "media"
    media.mkdir()
    # This command uses only generated test data, never camera credentials.
    # Keep stderr under pytest capture so Docker/registry/FFmpeg failures remain
    # visible in CI rather than being replaced by the private lab error message.
    subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--network",
            "none",
            "--cap-drop",
            "ALL",
            "--user",
            f"{os.getuid()}:{os.getgid()}",
            "--mount",
            f"type=bind,src={media},dst=/output",
            "--entrypoint",
            "ffmpeg",
            IMAGE,
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:s=64x48:r=10",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-preset",
            "ultrafast",
            "-g",
            "10",
            "-pix_fmt",
            "yuv420p",
            "/output/video.mp4",
        ],
        check=True,
        timeout=60,
    )
    model = tmp_path / "model.onnx"
    model.write_bytes(b"fixture model; inference is mocked")
    spots = tmp_path / "spots.json"
    spots.write_text(
        json.dumps(
            {
                "A": [[0, 0], [10, 0], [10, 10], [0, 10]],
                "B": [[20, 0], [30, 0], [30, 10], [20, 10]],
            }
        )
    )
    directory = generate(
        tmp_path / "scenario",
        video=media / "video.mp4",
        model=model,
        spots=spots,
        inputs=2,
        total=4,
        device="cpu",
    )
    session = LabSession(directory)
    capture = None
    try:
        state = session.start()
        monkeypatch.setenv("ESTACIONA_LAB_RTSP", state["stream_url"])
        monkeypatch.setenv("ESTACIONA_LAB_EDGE_KEY", "synthetic-lab-key")
        config = load_configuration(directory / "runtime.json")
        detector = Mock()
        detector.detect.return_value = []
        monkeypatch.setattr(worker, "load_detector", lambda *args, **kwargs: detector)
        stop = threading.Event()
        received = []
        broker = PreviewBroker((item.input_id for item in config.inputs))
        channel = PreviewChannel(multiprocessing.get_context("spawn"))
        selected = config.inputs[0].input_id
        broker.register(selected, channel)
        broker.frame(selected)

        async def scenario():
            database = sqlite3.connect(tmp_path / "gateway-fixture.db")
            database.execute(
                "CREATE TABLE events(id TEXT PRIMARY KEY,payload TEXT NOT NULL)"
            )
            dropped = False

            async def gateway(ws):
                nonlocal dropped
                assert ws.request.headers["Authorization"] == "Bearer synthetic-lab-key"
                await ws.send(
                    json.dumps({"type": "LOCAL_READY", "delivery_version": 1})
                )
                async for text in ws:
                    event = json.loads(text)
                    database.execute(
                        "INSERT OR IGNORE INTO events VALUES (?,?)",
                        (event["event_id"], json.dumps(event["payload"])),
                    )
                    database.commit()
                    received.append(event["event_id"])
                    if not dropped:
                        dropped = True
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
                    if (
                        database.execute("SELECT count(*) FROM events").fetchone()[0]
                        == 4
                    ):
                        await asyncio.sleep(0.2)
                        stop.set()

            try:
                async with websockets.serve(gateway, "127.0.0.1", 0) as listener:
                    cfg = replace(
                        config,
                        gateway_url=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws/edge",
                    )

                    async def renew_preview():
                        while not stop.is_set():
                            broker.frame(selected)
                            await asyncio.sleep(0.1)

                    await asyncio.wait_for(
                        asyncio.gather(
                            renew_preview(),
                            *(
                                worker.run_input(
                                    cfg,
                                    item,
                                    stop,
                                    lambda *args: None,
                                    preview=channel
                                    if item.input_id == selected
                                    else None,
                                )
                                for item in cfg.inputs
                            ),
                        ),
                        20,
                    )
                    snapshot = channel.read()
                    assert snapshot is not None
                    assert snapshot["sequence"] > 0 and snapshot["epoch"] >= 1
                    assert (snapshot["width"], snapshot["height"]) == (64, 48)
                    assert {spot["id"] for spot in snapshot["spots"]} == set(
                        cfg.inputs[0].mapping.spots
                    )
                    assert snapshot["jpeg"]
                    payloads = [
                        json.loads(row[0])
                        for row in database.execute("SELECT payload FROM events")
                    ]
                    assert len({entry["spot_id"] for entry in payloads}) == 4
                    assert len({entry["camera_id"] for entry in payloads}) == 2
                    assert len(received) > len(
                        set(received)
                    )  # same durable UUID replayed
                    assert all(
                        LocalOutbox(
                            input_directory(cfg.state_dir, cfg.scope, item.input_id)
                            / "outbox.db"
                        ).first()
                        is None
                        for item in cfg.inputs
                    )
            finally:
                stop.set()
                database.close()

        asyncio.run(scenario())
        capture = LatestCapture(Source("rtsp", env="ESTACIONA_LAB_RTSP"))
        capture.start()
        first = wait_for(lambda: capture.latest(after=0, max_age=1))
        session.pause_publisher()
        wait_for(lambda: capture.latest(after=0, max_age=0.2) is None)
        session.restart_publisher()
        recovered = wait_for(
            lambda: (
                frame
                if (frame := capture.latest(after=first.sequence, max_age=1))
                is not None
                and frame.epoch > first.epoch
                else None
            )
        )
        assert recovered.frame.shape == (48, 64, 3)
    finally:
        if capture is not None:
            capture.close(timeout=4)
        session.stop()
    assert session.status()["server"] is False
    assert session.status()["publisher"] is False
