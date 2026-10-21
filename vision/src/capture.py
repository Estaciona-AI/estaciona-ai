# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Continuous decoding with a single recent frame, owned by one input worker."""

from dataclasses import dataclass
import datetime
import math
import threading
import time

import cv2
import numpy as np

from configuration import Source


@dataclass(frozen=True)
class CapturedFrame:
    sequence: int
    epoch: int
    frame: np.ndarray
    received_monotonic: float
    received_at: str


class LatestFrame:
    def __init__(self):
        self._lock = threading.Lock()
        self._sequence = 0
        self._frame = None

    def publish(self, frame, *, epoch, received_at, now):
        frame.setflags(write=False)
        with self._lock:
            self._sequence += 1
            self._frame = CapturedFrame(self._sequence, epoch, frame, now, received_at)

    def get(self, *, after, max_age, now):
        with self._lock:
            frame = self._frame
        if (
            frame is None
            or frame.sequence <= after
            or not 0 <= now - frame.received_monotonic <= max_age
        ):
            return None
        return frame

    def clear(self):
        with self._lock:
            self._frame = None


class LatestCapture:
    def __init__(self, source: Source, *, opener=None, retry_min=1.0, retry_max=10.0):
        self.source = source
        self.slot = LatestFrame()
        self._opener = opener or cv2.VideoCapture
        self._retry_min, self._retry_max = retry_min, retry_max
        self._stop = threading.Event()
        self._thread = None
        self.ended = False
        self.error = None
        self.metrics = None

    def start(self):
        # Resolve before starting a thread, to fail closed on missing credentials.
        self._url = self.source.resolve()
        self._thread = threading.Thread(
            target=self._run, daemon=True, name="vision-capture"
        )
        self._thread.start()

    def latest(self, *, after, max_age):
        return self.slot.get(after=after, max_age=max_age, now=time.monotonic())

    def close(self, timeout=0.5):
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout)
            return not self._thread.is_alive()
        return True

    def _open(self):
        if self.source.kind == "rtsp":
            return self._opener(
                self._url,
                cv2.CAP_FFMPEG,
                [
                    cv2.CAP_PROP_OPEN_TIMEOUT_MSEC,
                    3000,
                    cv2.CAP_PROP_READ_TIMEOUT_MSEC,
                    3000,
                ],
            )
        return self._opener(self._url)

    def _run(self):
        epoch, backoff = 0, self._retry_min
        while not self._stop.is_set():
            cap = None
            self.slot.clear()
            try:
                if self.metrics is not None:
                    self.metrics.increment("capture_attempts")
                cap = self._open()
                if not cap.isOpened():
                    self.error = "open_failed"
                else:
                    epoch += 1
                    period = 0.0
                    if self.source.kind == "file":
                        fps = cap.get(cv2.CAP_PROP_FPS)
                        if not math.isfinite(fps) or not 0 < fps <= 240:
                            self.error = "invalid_file_fps"
                            self.ended = True
                            return
                        period = 1.0 / fps
                    deadline = time.monotonic()
                    while not self._stop.is_set():
                        if self._stop.wait(max(0.0, deadline - time.monotonic())):
                            break
                        read_started = time.perf_counter() if self.metrics else 0
                        ok, frame = cap.read()
                        if self.metrics is not None:
                            self.metrics.observe(
                                "decode_wait_ms",
                                (time.perf_counter() - read_started) * 1000,
                            )
                        if not ok:
                            if self.metrics is not None:
                                self.metrics.increment("capture_read_failures")
                            if self.source.kind == "file" and not self.source.loop:
                                self.ended = True
                                return
                            self.error = "read_failed"
                            break
                        now = time.monotonic()
                        if (
                            not isinstance(frame, np.ndarray)
                            or frame.dtype != np.uint8
                            or frame.ndim != 3
                            or frame.shape[2] != 3
                            or frame.shape[0] * frame.shape[1] > 16 * 1024 * 1024
                        ):
                            self.error = "invalid_frame"
                            self.ended = True
                            return
                        self.error = None
                        if self.metrics is not None:
                            self.metrics.increment("decoded")
                        backoff = self._retry_min
                        stamp = (
                            datetime.datetime.now(datetime.UTC)
                            .isoformat(timespec="microseconds")
                            .replace("+00:00", "Z")
                        )
                        self.slot.publish(
                            frame, epoch=epoch, received_at=stamp, now=now
                        )
                        deadline = now + period
            except Exception:
                # Native RTSP errors can contain credentials; expose only codes.
                self.error = "capture_failed"
            finally:
                if cap is not None:
                    cap.release()
            if self._stop.wait(backoff):
                break
            backoff = min(self._retry_max, backoff * 2)
