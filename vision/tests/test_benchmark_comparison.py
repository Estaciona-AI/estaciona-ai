# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json

import numpy as np
import pytest

from benchmark_comparison import comparison_pass, frame_indexes
from benchmark_profiles import BenchmarkProfile
from test_worker import config


@pytest.mark.parametrize("step", [1, 3])
def test_comparison_uses_exact_frames_and_never_opens_delivery(tmp_path, step):
    class Model:
        def detect(self, *args, **kwargs):
            return []

        def metadata(self):
            return {"providers": ["fixture"]}

    frames = [(index, np.full((10, 10, 3), index, np.uint8)) for index in range(8)]
    profile = BenchmarkProfile("baseline", tmp_path / "fixture.onnx", "cpu")
    profile.path.write_bytes(b"fixture")
    cfg = config(tmp_path)
    output = tmp_path / "result"
    output.mkdir()
    comparison_pass(
        profile,
        cfg.inputs[0].mapping,
        iter(frames[::step]),
        output,
        model=Model(),
        warmup=1,
    )
    rows = frame_indexes(output / "baseline.frames.jsonl")
    assert list(rows) == [index for index, _ in frames[::step]][1:]
    assert len(set(rows.values())) == len(rows)
    assert not (tmp_path / "state").exists()
    report = json.loads((output / "baseline.json").read_text())
    assert report["delivery"] == "shadow" and report["frames"] == len(rows)


def comparison_fixture_worker(
    profile, mapping, video, output, count, warmup, stride, barrier
):
    import benchmark_comparison as benchmark
    import time

    class Model:
        def detect(self, *args, **kwargs):
            time.sleep(0.02)
            return []

        def metadata(self):
            return {"adapter": "fixture", "providers": ["fixture"]}

    def frames(video, *, samples, stride):
        yield 30
        for index in range(samples):
            yield index * stride, np.full((10, 10, 3), index, np.uint8)

    benchmark.load_detector = lambda *args, **kwargs: Model()
    benchmark.video_frames = frames
    benchmark._comparison_entry(
        profile, mapping, video, output, count, warmup, stride, barrier
    )


@pytest.mark.parametrize("mode", ["sequential", "simultaneous"])
def test_comparison_processes_match_frames_in_both_modes(tmp_path, monkeypatch, mode):
    import benchmark_comparison as benchmark
    import multiprocessing
    from benchmark_comparison import compare

    cfg = config(tmp_path)
    model = cfg.inputs[0].detector.path
    model.write_bytes(b"fixture")
    video = tmp_path / "video.mp4"
    video.write_bytes(b"fixture")
    profiles = [
        BenchmarkProfile("first", model, "cpu"),
        BenchmarkProfile("second", model, "cpu"),
    ]
    monkeypatch.setattr(benchmark, "_comparison_entry", comparison_fixture_worker)
    report = compare(
        cfg,
        profiles,
        video,
        tmp_path / mode,
        input_id="cam-A",
        mode=mode,
        frames=3,
        warmup=1,
        timeout=5,
    )
    assert report["completed"] and report["same_frames_verified"]
    assert report["installation"]["organization_id"] == cfg.organization_id
    if mode == "simultaneous":
        assert report["measured_overlap_seconds"] > 0
    assert not multiprocessing.active_children()
