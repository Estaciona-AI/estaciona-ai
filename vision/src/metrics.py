# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Optional bounded telemetry. Quantiles are histogram upper bounds, not exact samples."""

import bisect
from contextlib import contextmanager
import json
import math
import os
from pathlib import Path
import threading
import tempfile
import time

BOUNDS = tuple(2 ** (index / 4) for index in range(-40, 121))


class Distribution:
    def __init__(self):
        self.buckets = [0] * (len(BOUNDS) + 1)
        self.count = 0
        self.total = 0.0
        self.minimum = None
        self.maximum = None

    def add(self, value):
        if not math.isfinite(value) or value < 0:
            raise ValueError("invalid metric value")
        self.buckets[bisect.bisect_left(BOUNDS, value)] += 1
        self.count += 1
        self.total += value
        self.minimum = value if self.minimum is None else min(self.minimum, value)
        self.maximum = value if self.maximum is None else max(self.maximum, value)

    def summary(self):
        def quantile(fraction):
            if not self.count:
                return None
            goal, seen = math.ceil(self.count * fraction), 0
            for index, count in enumerate(self.buckets):
                seen += count
                if seen >= goal:
                    return BOUNDS[index] if index < len(BOUNDS) else self.maximum

        return {
            "count": self.count,
            "min": self.minimum,
            "max": self.maximum,
            "mean": self.total / self.count if self.count else None,
            "p50_upper": quantile(0.5),
            "p95_upper": quantile(0.95),
            "p99_upper": quantile(0.99),
        }


class Metrics:
    def __init__(self, *, warmup=0, control=None, end=None):
        if not math.isfinite(warmup) or warmup < 0:
            raise ValueError("invalid warmup")
        self.started = time.monotonic()
        self._measured_from = self.started + warmup
        self.control = control
        self.end = end
        self.stages = {}
        self.counters = {}
        self.lock = threading.Lock()

    @property
    def measured_from(self):
        return self.control.value if self.control is not None else self._measured_from

    @property
    def recording(self):
        now = time.monotonic()
        return (
            self.measured_from
            <= now
            < (self.end.value if self.end is not None else math.inf)
        )

    def observe(self, name, value):
        if not self.recording:
            return
        with self.lock:
            self.stages.setdefault(name, Distribution()).add(value)

    def increment(self, name, amount=1):
        if not self.recording:
            return
        if type(amount) is not int or amount < 0:
            raise ValueError("invalid metric counter")
        with self.lock:
            self.counters[name] = self.counters.get(name, 0) + amount

    @contextmanager
    def stage(self, name):
        started = time.perf_counter()
        try:
            yield
        finally:
            self.observe(name, (time.perf_counter() - started) * 1000)

    def snapshot(self):
        with self.lock:
            return {
                "schema_version": 1,
                "pid": os.getpid(),
                "window_start_monotonic": self.measured_from
                if math.isfinite(self.measured_from)
                else None,
                "window_end_monotonic": min(
                    time.monotonic(),
                    self.end.value if self.end is not None else math.inf,
                ),
                "measured_seconds": max(
                    0,
                    min(
                        time.monotonic(),
                        self.end.value if self.end is not None else math.inf,
                    )
                    - self.measured_from,
                ),
                "quantiles": "histogram_upper_bounds",
                "counters": dict(self.counters),
                "stages": {
                    name: values.summary() for name, values in self.stages.items()
                },
            }

    def write(self, path, *, state=None):
        atomic_json(path, {**self.snapshot(), "state": state or {}})


def atomic_json(path, value):
    path = Path(path)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", prefix=".metrics-", suffix=".tmp", dir=path.parent, delete=False
        ) as stream:
            temporary = Path(stream.name)
            json.dump(value, stream, allow_nan=False, separators=(",", ":"))
            stream.write("\n")
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
