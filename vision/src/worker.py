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


async def run_input(
    config,
    item,
    stop,
    report,
    *,
    preview=None,
    metrics=None,
    metrics_path=None,
    publish=True,
    capture_threads=None,
):
    """Called inside an exclusively leased worker process; report contains no secrets."""
    if not publish and config.environment == "production":
        raise PermanentInputError("shadow is restricted to demo/test installations")
    headers, outbox, delivery = None, None, None
    if publish:
        credential = os.environ.get(config.credential_env)
        if (
            not credential
            or len(credential) > 4096
            or any(ord(c) < 32 for c in credential)
        ):
            raise PermanentInputError("invalid credential environment")
        headers = {"Authorization": f"Bearer {credential}"}
        directory = input_directory(config.state_dir, config.scope, item.input_id)
        outbox = LocalOutbox(directory / "outbox.db")
        delivery = LocalDelivery(config.gateway_url, outbox)
    capture = (
        LatestCapture(item.source)
        if capture_threads is None
        else LatestCapture(item.source, decoder_threads=capture_threads)
    )
    capture.metrics = metrics
    state = {
        "input_id": item.input_id,
        "delivery": "gateway" if publish else "shadow",
        "pending": None,
        "last_sequence": 0,
        "last_epoch": 0,
        "model": None,
    }
    tasks = []

    async def flush_metrics():
        while not stop.is_set():
            if outbox is not None:
                state["pending"] = await asyncio.to_thread(outbox.pending_count)
            write = asyncio.create_task(
                asyncio.to_thread(metrics.write, metrics_path, state=dict(state))
            )
            try:
                await asyncio.shield(write)
            except asyncio.CancelledError:
                await write
                raise
            await asyncio.sleep(1)

    publisher = None
    if preview is not None:
        from inspection import PreviewPublisher

        publisher = PreviewPublisher(preview, item.mapping.spots)

    async def persist(text, *, heartbeat=False):
        if outbox is None:
            return
        enqueue_started = time.perf_counter()
        # Backpressure retains the current transition; only decoded frames drop.
        while not stop.is_set():
            try:
                enqueue = outbox.enqueue_many if heartbeat else outbox.enqueue
                await asyncio.to_thread(enqueue, text)
                if metrics is not None:
                    metrics.observe(
                        "enqueue_batch_ms" if heartbeat else "enqueue_ms",
                        (time.perf_counter() - enqueue_started) * 1000,
                    )
                    if heartbeat:
                        metrics.increment("heartbeat_events_enqueued", len(text))
                return
            except BufferError:
                report(Status.BACKPRESSURE, True, 0)
                await asyncio.sleep(0.1)

    async def pump():
        ws = None
        try:
            while not stop.is_set():
                if (
                    outbox is not None
                    and await asyncio.to_thread(outbox.first) is not None
                ):
                    delivery_started = time.perf_counter()
                    ws = await delivery.send(ws, None, headers)
                    if metrics is not None:
                        metrics.observe(
                            "delivery_attempt_ms",
                            (time.perf_counter() - delivery_started) * 1000,
                        )
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
        last_processed = None
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
                    if outbox is None or await asyncio.to_thread(outbox.first) is None:
                        return
                    report(Status.BACKPRESSURE, True, sequence)
                else:
                    report(Status.WAITING, False, sequence)
                await asyncio.sleep(0.05)
                continue
            if metrics is not None:
                metrics.increment("sampled")
                metrics.increment("skipped", max(0, frame.sequence - sequence - 1))
            sequence = frame.sequence
            state.update(last_sequence=sequence, last_epoch=frame.epoch)
            if frame.frame.shape != (item.mapping.height, item.mapping.width, 3):
                raise PermanentInputError("frame does not match calibration dimensions")
            if pipeline is None:
                pipeline = VisionPipeline(
                    model,
                    item.mapping.spots,
                    frame.frame.shape,
                    metrics=metrics,
                    confidence=item.detector.confidence,
                    gamma=item.detector.gamma,
                )
            elif epoch != frame.epoch or (
                last_observation is not None
                and frame.received_monotonic - last_observation
                > max(config.max_frame_age, 2 * period)
            ):
                pipeline.reset_candidates(now=frame.received_monotonic)
            epoch = frame.epoch
            processing_started = time.monotonic()
            result = await asyncio.to_thread(
                pipeline.process,
                frame.frame,
                now=frame.received_monotonic,
                expires_at=frame.received_monotonic + config.max_frame_age,
            )
            processing_finished = time.monotonic()
            # Never catch up missed sample slots with bursts of stale work.
            next_sample = max(now + period, time.monotonic())
            if metrics is not None:
                metrics.observe(
                    "pipeline_ms", (processing_finished - processing_started) * 1000
                )
                metrics.observe(
                    "frame_age_ms",
                    max(0, processing_finished - frame.received_monotonic) * 1000,
                )
            if result is None:
                if metrics is not None:
                    metrics.increment("expired")
                last_observation = None
                continue
            if metrics is not None:
                metrics.increment("processed")
            last_observation = frame.received_monotonic
            fps = (
                1.0 / (processing_finished - last_processed)
                if last_processed is not None and processing_finished > last_processed
                else 0.0
            )
            last_processed = processing_finished
            for observation in result.spots:
                if observation.changed and publish:
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
                    if metrics is not None:
                        metrics.observe(
                            "confirmation_observation_to_enqueue_ms",
                            max(0, time.monotonic() - frame.received_monotonic) * 1000,
                        )
            if (
                publish
                and time.monotonic() - last_sync >= 30
                and await asyncio.to_thread(outbox.first) is None
            ):
                # A heartbeat represents this actual observation, not wall-clock now.
                await persist(
                    [
                        _payload(item.input_id, spot_id, status, 1.0, frame.received_at)
                        for spot_id, status in pipeline.confirmed_statuses().items()
                    ],
                    heartbeat=True,
                )
                last_sync = time.monotonic()
            if publisher is not None and publisher.wanted():
                # Optional presentation work is isolated from durable IO and inference.
                try:
                    pending = (
                        await asyncio.to_thread(outbox.pending_count)
                        if outbox is not None
                        else None
                    )
                    published = await asyncio.to_thread(
                        publisher.publish,
                        frame,
                        result,
                        latency_ms=(processing_finished - processing_started) * 1000,
                        fps=fps,
                        pending=pending,
                    )
                    if published is not None:
                        preview.error.value = int(not published)
                except Exception:
                    # Never surface decoder/credential-bearing exception strings.
                    preview.error.value = 1
            report(Status.ACTIVE, True, sequence)

    async def initialize_and_produce():
        try:
            model = await asyncio.to_thread(
                load_detector,
                item.detector.path,
                device=item.detector.device,
                tile_size=item.detector.tile_size,
                tile_overlap=item.detector.tile_overlap,
            )
        except Exception:
            report(Status.FAILED, False, 0)
            # Give durable events a delivery path even with an invalid model.
            # The supervisor still bounds a stalled worker's lifetime.
            while (
                not stop.is_set()
                and outbox is not None
                and await asyncio.to_thread(outbox.first) is not None
            ):
                await asyncio.sleep(0.1)
            raise
        if metrics is not None:
            model.metrics = metrics
            state["model"] = {
                **model.metadata(),
                "confidence": item.detector.confidence,
                "gamma": item.detector.gamma,
            }
        if not stop.is_set():
            capture.start()
            report(Status.WAITING, True, 0)
            await produce(model)

    try:
        tasks = [
            asyncio.create_task(initialize_and_produce()),
            asyncio.create_task(pump()),
        ]
        if metrics is not None and metrics_path is not None:
            tasks.append(asyncio.create_task(flush_metrics()))
        done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            task.result()
        if tasks[0] not in done:
            # A normal pump shutdown must not cancel and hide a producer error.
            # A genuinely blocked producer is still bounded by the supervisor.
            await tasks[0]
    finally:
        stop.set()
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        capture.close()
        if metrics is not None and metrics_path is not None:
            if outbox is not None:
                state["pending"] = await asyncio.to_thread(outbox.pending_count)
            await asyncio.to_thread(metrics.write, metrics_path, state=dict(state))
