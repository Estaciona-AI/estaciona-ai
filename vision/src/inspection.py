# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Optional, bounded worker snapshots. No camera addresses or credentials cross IPC."""

import base64
import json
import threading
import time

import cv2

from presentation import draw_annotations

MAX_SNAPSHOT = 768 * 1024
VIEWER_TTL = 2.0
PREVIEW_INTERVAL = 0.5


class PreviewChannel:
    """One replaceable buffer per worker generation; no queues or shared worker locks."""

    def __init__(self, context):
        self.lock = context.Lock()
        self.buffer = context.RawArray("B", MAX_SNAPSHOT)
        self.length = context.RawValue("i", 0)
        self.error = context.RawValue("i", 0)
        # Parent is the sole writer of demand; worker is sole snapshot writer.
        self.request_until = context.RawValue("d", 0)
        self.generation = context.RawValue("q", 0)

    def invalidate(self):
        self.request_until.value = 0
        self.generation.value += 1

    def write(self, snapshot, *, generation=None):
        generation = self.generation.value if generation is None else generation
        data = json.dumps(
            {**snapshot, "_generation": generation}, allow_nan=False
        ).encode()
        if len(data) > MAX_SNAPSHOT or not self.lock.acquire(timeout=0.01):
            return False
        try:
            self.buffer[: len(data)] = data
            self.length.value = len(data)
        finally:
            self.lock.release()
        return True

    def read(self):
        if not self.lock.acquire(timeout=0.01):
            return None
        try:
            data = bytes(self.buffer[: self.length.value])
        finally:
            self.lock.release()
        if not data:
            return None
        snapshot = json.loads(data)
        if snapshot.pop("_generation") != self.generation.value:
            return None
        return snapshot


class PreviewPublisher:
    def __init__(self, channel, spots):
        self.channel = channel
        self.spots = spots
        self.next_preview = 0.0

    def wanted(self):
        now = time.monotonic()
        return now < self.channel.request_until.value and now >= self.next_preview

    def publish(self, frame, result, *, latency_ms, fps, pending):
        if not self.wanted():
            return
        generation = self.channel.generation.value
        self.next_preview = time.monotonic() + PREVIEW_INTERVAL
        height, width = frame.frame.shape[:2]
        scale = min(1.0, 960 / width, 540 / height)
        image = draw_annotations(frame.frame, result, self.spots, scale=scale)
        ok, jpeg = cv2.imencode(".jpg", image, [cv2.IMWRITE_JPEG_QUALITY, 75])
        if not ok or time.monotonic() >= self.channel.request_until.value:
            return
        self.channel.write(
            {
                "sequence": frame.sequence,
                "epoch": frame.epoch,
                "observed_at": frame.received_at,
                "received_monotonic": frame.received_monotonic,
                "latency_ms": round(latency_ms, 2),
                "fps": round(fps, 2),
                "pending": pending,
                "width": image.shape[1],
                "height": image.shape[0],
                "detections": result.detections,
                "spots": [
                    {"id": spot.spot_id, "status": spot.status, "ratio": spot.ratio}
                    for spot in result.spots
                ],
                "jpeg": base64.b64encode(jpeg).decode("ascii"),
            },
            generation=generation,
        )


class PreviewBroker:
    """Supervisor-side read-only diagnostic registry and single viewer lease."""

    def __init__(self, input_ids, max_age=2.0, *, profiles=None):
        input_ids = tuple(input_ids)
        self.lock = threading.Lock()
        self.channels = dict.fromkeys(input_ids)
        self.states = {input_id: {"status": "starting"} for input_id in input_ids}
        self.profiles = profiles or {}
        self.selected = None
        self.max_age = max_age

    def register(self, input_id, channel):
        with self.lock:
            old = self.channels[input_id]
            if old is not None:
                old.invalidate()
            self.channels[input_id] = channel

    def update(self, input_id, status):
        with self.lock:
            self.states[input_id] = {"status": status}

    def status(self):
        with self.lock:
            return {
                "inputs": [
                    {"id": key, **self.profiles.get(key, {}), **value}
                    for key, value in self.states.items()
                ]
            }

    def frame(self, input_id):
        with self.lock:
            if input_id not in self.channels:
                raise KeyError(input_id)
            now = time.monotonic()
            if self.selected != input_id:
                old = self.channels.get(self.selected)
                if old is not None:
                    old.invalidate()
                self.selected = input_id
            channel = self.channels[input_id]
            snapshot = None
            if channel is not None:
                if channel.request_until.value <= now:
                    channel.invalidate()
                channel.request_until.value = now + VIEWER_TTL
                snapshot = channel.read()
            age = (
                None
                if snapshot is None
                else max(0, now - snapshot["received_monotonic"])
            )
            return {
                "input_id": input_id,
                **self.profiles.get(input_id, {}),
                **self.states[input_id],
                "frame": snapshot,
                "age_ms": None if age is None else round(age * 1000, 1),
                "stale": age is None or age > self.max_age,
                "preview_error": channel is not None and bool(channel.error.value),
            }
