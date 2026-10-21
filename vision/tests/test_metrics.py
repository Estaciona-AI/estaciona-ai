# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
import math

import numpy as np
import pytest

from metrics import Metrics, Distribution
from pipeline import VisionPipeline


def test_distribution_is_bounded_and_reports_quantile_upper_bounds():
    values = Distribution()
    for value in range(10000):
        values.add(value)
    result = values.summary()
    assert result["count"] == 10000
    assert 9499 <= result["p95_upper"] <= 9499 * 1.2
    assert result["min"] == 0 and result["max"] == 9999
    assert len(values.buckets) < 200
    assert result["mean"] == pytest.approx(4999.5)
    for value in (-1, math.inf, math.nan):
        with pytest.raises(ValueError):
            values.add(value)


def test_metrics_warmup_and_atomic_snapshot(tmp_path, monkeypatch):
    clock = [10.0]
    monkeypatch.setattr("metrics.time.monotonic", lambda: clock[0])
    metrics = Metrics(warmup=2)
    metrics.observe("pipeline_ms", 100)
    metrics.increment("processed")
    clock[0] = 12
    metrics.observe("pipeline_ms", 25)
    metrics.increment("processed")
    clock[0] = 14
    path = tmp_path / "snapshot.json"
    metrics.write(path, state={"pending": 0})
    data = json.loads(path.read_text())
    assert data["measured_seconds"] == 2
    assert data["counters"]["processed"] == 1
    assert data["stages"]["pipeline_ms"]["mean"] == 25
    assert data["state"]["pending"] == 0
    assert not path.with_suffix(".tmp").exists()


def test_pipeline_instrumentation_preserves_results_and_separates_stages():
    class Detector:
        def detect(self, frame, **kwargs):
            return [(0, 0, 9, 9)]

    frame = np.zeros((10, 10, 3), np.uint8)
    spots = {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}
    metrics = Metrics()
    observed = VisionPipeline(Detector(), spots, frame.shape, metrics=metrics).process(
        frame, now=1
    )
    reference = VisionPipeline(Detector(), spots, frame.shape).process(frame, now=1)
    assert observed == reference
    assert set(metrics.snapshot()["stages"]) >= {
        "preprocess_ms",
        "detect_ms",
        "occupancy_ms",
    }
