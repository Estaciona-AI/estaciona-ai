# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""One input: continuous capture, bounded inference and independent durable IO."""

import asyncio
import datetime
from enum import IntEnum
import json
import os
import time

from capture import LatestCapture
from delivery import LocalDelivery, LocalOutbox
from detector import load_detector
from pipeline import VisionPipeline
from storage import input_directory


class Status(IntEnum):
    STARTING = 0
    WAITING = 1
    ACTIVE = 2
    BACKPRESSURE = 3
    STOPPED = 4
    FAILED = 5
    INVALID = 6


class PermanentInputError(ValueError):
    pass


def _stamp():
    return (
        datetime.datetime.now(datetime.UTC)
        .isoformat(timespec="microseconds")
        .replace("+00:00", "Z")
    )


def _payload(input_id, spot_id, status, confidence, observed_at):
    return json.dumps(
        {
            "type": "SPOT_UPDATE",
            "spot_id": spot_id,
            "status": status,
            "camera_id": input_id,
            "confidence": confidence,
            "timestamp": observed_at,
            "edge_sent_at": _stamp(),
        }
    )


async def run_input(config, item, stop, report):
    """Called inside an exclusively leased worker process; report contains no secrets."""
    credential = os.environ.get(config.credential_env)
    if not credential or len(credential) > 4096 or any(ord(c) < 32 for c in credential):
        raise PermanentInputError("invalid credential environment")
    headers = {"Authorization": f"Bearer {credential}"}
    directory = input_directory(config.state_dir, config.scope, item.input_id)
    outbox = LocalOutbox(directory / "outbox.db")
    delivery = LocalDelivery(config.gateway_url, outbox)
    capture = LatestCapture(item.source)
    tasks = []

    async def persist(text):
        # Backpressure retains the current transition; only decoded frames drop.
        while not stop.is_set():
            try:
                await asyncio.to_thread(outbox.enqueue, text)
                return
            except BufferError:
                report(Status.BACKPRESSURE, True, 0)
                await asyncio.sleep(0.1)

    async def pump():
        ws = None
        try:
            while not stop.is_set():
                if await asyncio.to_thread(outbox.first) is not None:
                    ws = await delivery.send(ws, None, headers)
                await asyncio.sleep(0.1)
        finally:
            if ws is not None:
                await ws.close()

    async def produce(model):
        pipeline = None
        sequence, epoch = 0, None
        next_sample, last_sync = 0.0, time.monotonic()
        period = 1.0 / item.fps
        last_observation = None
        while not stop.is_set():
            now = time.monotonic()
            if now < next_sample:
                await asyncio.sleep(min(0.05, next_sample - now))
                continue
            frame = capture.latest(after=sequence, max_age=config.max_frame_age)
            if frame is None:
                if capture.ended:
                    if capture.error is not None:
                        raise PermanentInputError("invalid captured media")
                    if await asyncio.to_thread(outbox.first) is None:
                        return
                    report(Status.BACKPRESSURE, True, sequence)
                else:
                    report(Status.WAITING, False, sequence)
                await asyncio.sleep(0.05)
                continue
            sequence = frame.sequence
            if frame.frame.shape != (item.mapping.height, item.mapping.width, 3):
                raise PermanentInputError("frame does not match calibration dimensions")
            if pipeline is None:
                pipeline = VisionPipeline(model, item.mapping.spots, frame.frame.shape)
            elif epoch != frame.epoch or (
                last_observation is not None
                and frame.received_monotonic - last_observation
                > max(config.max_frame_age, 2 * period)
            ):
                pipeline.reset_candidates(now=frame.received_monotonic)
            epoch = frame.epoch
            result = await asyncio.to_thread(
                pipeline.process,
                frame.frame,
                now=frame.received_monotonic,
                expires_at=frame.received_monotonic + config.max_frame_age,
            )
            # Never catch up missed sample slots with bursts of stale work.
            next_sample = max(now + period, time.monotonic())
            if result is None:
                last_observation = None
                continue
            last_observation = frame.received_monotonic
            for observation in result.spots:
                if observation.changed:
                    await persist(
                        _payload(
                            item.input_id,
                            observation.spot_id,
                            observation.status,
                            float(observation.ratio)
                            if observation.status == "occupied"
                            else 1.0,
                            frame.received_at,
                        )
                    )
            if (
                time.monotonic() - last_sync >= 30
                and await asyncio.to_thread(outbox.first) is None
            ):
                # A heartbeat represents this actual observation, not wall-clock now.
                for spot_id, status in pipeline.confirmed_statuses().items():
                    await persist(
                        _payload(item.input_id, spot_id, status, 1.0, frame.received_at)
                    )
                last_sync = time.monotonic()
            report(Status.ACTIVE, True, sequence)

    try:
        model = await asyncio.to_thread(
            load_detector, item.detector.path, device=item.detector.device
        )
        if stop.is_set():
            return
        capture.start()
        report(Status.WAITING, True, 0)
        tasks = [asyncio.create_task(produce(model)), asyncio.create_task(pump())]
        done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            task.result()
    finally:
        stop.set()
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        capture.close()
