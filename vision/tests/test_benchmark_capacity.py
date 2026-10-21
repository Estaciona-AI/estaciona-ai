# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
from types import SimpleNamespace

import numpy as np
import pytest

from benchmark_capacity import capacity_summary
from test_worker import config


def test_capacity_cannot_pass_with_missing_input_or_worker_restart():
    metrics = {
        "measured_seconds": 10,
        "counters": {"processed": 25},
        "stages": {"frame_age_ms": {"p95_upper": 100}},
        "state": {"pending": None, "delivery": "shadow"},
    }
    inputs = [
        SimpleNamespace(input_id="A", fps=2.5),
        SimpleNamespace(input_id="B", fps=2.5),
    ]
    report = capacity_summary(inputs, {"A": metrics}, restarts=0, completed=True)
    assert not report["vision_target_met"]
    assert report["minimum_input_fps"] == 0
    report = capacity_summary(
        inputs, {"A": metrics, "B": metrics}, restarts=1, completed=True
    )
    assert not report["vision_target_met"]
    report = capacity_summary(
        inputs, {"A": metrics, "B": metrics}, restarts=0, completed=True
    )
    assert report["vision_target_met"]
    assert not report["full_pc_validated"]


def measured_fixture_worker(config, item, health, preview=None, measurement=None):
    import datetime
    import time
    import worker
    from capture import CapturedFrame
    import runtime

    class Capture:
        ended = False
        error = None

        def __init__(self, *args):
            self.sequence = 0

        def start(self):
            pass

        def latest(self, **kwargs):
            self.sequence += 1
            return CapturedFrame(
                self.sequence,
                1,
                np.zeros((10, 10, 3), np.uint8),
                time.monotonic(),
                datetime.datetime.now(datetime.UTC).isoformat(),
            )

        def close(self):
            return True

    class Model:
        def detect(self, *args, **kwargs):
            return []

        def metadata(self):
            return {"adapter": "fixture", "providers": ["fixture"]}

    worker.LatestCapture = Capture
    worker.load_detector = lambda *args, **kwargs: Model()
    runtime._worker_entry(config, item, health, preview, measurement)


def test_capacity_runs_real_spawn_and_common_window_without_delivery(
    tmp_path, monkeypatch
):
    from dataclasses import replace
    import runtime
    from configuration import Mapping
    from benchmark_capacity import capacity_trial

    cfg = config(tmp_path)
    cfg.inputs[0].detector.path.write_bytes(b"fixture")
    second = replace(
        cfg.inputs[0],
        input_id="cam-B",
        mapping=Mapping(10, 10, {"B": [[0, 0], [9, 0], [9, 9], [0, 9]]}),
    )
    cfg = replace(cfg, inputs=(*cfg.inputs, second))
    monkeypatch.setattr(runtime, "_worker_entry", measured_fixture_worker)
    report = capacity_trial(cfg, tmp_path / "capacity", count=2, seconds=1, warmup=0.1)
    assert report["completed"] and report["scope"] == "vision_only"
    assert report["worker_restarts"] == 0
    assert len(report["inputs"]) == 2
    assert all(item["effective_fps"] > 0 for item in report["inputs"])
    assert not list((tmp_path / "capacity").rglob("*.db"))
    values = [
        json.loads(path.read_text())
        for path in (tmp_path / "capacity/metrics").glob("*.json")
    ]
    assert len(values) == 2
    assert all(value["measured_seconds"] == pytest.approx(1) for value in values)


def test_capacity_cancellation_is_preserved(tmp_path, monkeypatch):
    import runtime
    import threading
    from benchmark_capacity import capacity_trial

    cfg = config(tmp_path)
    cfg.inputs[0].detector.path.write_bytes(b"fixture")
    monkeypatch.setattr(runtime, "_worker_entry", measured_fixture_worker)
    stop = threading.Event()
    stop.set()
    report = capacity_trial(
        cfg, tmp_path / "cancelled", count=1, seconds=1, warmup=0, stop=stop
    )
    assert not report["completed"] and stop.is_set()
    assert not report["vision_target_met"]


def test_larger_ramp_requires_measured_margin():
    from benchmark_capacity import can_advance

    report = {"vision_target_met": False, "resources": {"samples": 3}, "models": []}
    assert can_advance(report, 20)
    assert not can_advance(report, 25)
    report["vision_target_met"] = True
    assert can_advance(report, 25)
    report["models"] = [{"device_requested": "cuda"}]
    assert not can_advance(report, 25)
    report["resources"].update(gpu_total_mib=16000, gpu_peak_used_mib=15500)
    assert not can_advance(report, 25)
    report["resources"]["gpu_peak_used_mib"] = 14000
    assert can_advance(report, 25)
