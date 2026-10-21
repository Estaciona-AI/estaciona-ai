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
    def __init__(self, *, warmup=0):
        if not math.isfinite(warmup) or warmup < 0:
            raise ValueError("invalid warmup")
        self.started = time.monotonic()
        self.measured_from = self.started + warmup
        self.stages = {}
        self.counters = {}
        self.lock = threading.Lock()

    def observe(self, name, value):
        if time.monotonic() < self.measured_from:
            return
        with self.lock:
            self.stages.setdefault(name, Distribution()).add(value)

    def increment(self, name, amount=1):
        if time.monotonic() < self.measured_from:
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
                "measured_seconds": max(0, time.monotonic() - self.measured_from),
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
    temporary = path.with_suffix(".tmp")
    try:
        with temporary.open("x") as stream:
            json.dump(value, stream, allow_nan=False, separators=(",", ":"))
            stream.write("\n")
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)
