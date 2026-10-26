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
        BenchmarkProfile(
            "second", model, "cpu", confidence=0.3, gamma=1.0, tile_size=512
        ),
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
    assert report["schema_version"] == 2
    assert report["profile_parameters"]["first"] == {
        "confidence": 0.1,
        "gamma": 1.4,
        "tile_size": 0,
        "tile_overlap": 0.25,
        "tile_scope": "frame",
        "provider": "default",
        "precision": "fp32",
    }
    assert report["profile_parameters"]["second"] == {
        "confidence": 0.3,
        "gamma": 1.0,
        "tile_size": 512,
        "tile_overlap": 0.25,
        "tile_scope": "frame",
        "provider": "default",
        "precision": "fp32",
    }
    for name in ["first", "second"]:
        details = json.loads((tmp_path / mode / f"{name}.json").read_text())
        assert details["parameters"] == report["profile_parameters"][name]
        assert details["schema_version"] == 2
    assert report["installation"]["organization_id"] == cfg.organization_id
    if mode == "simultaneous":
        assert report["measured_overlap_seconds"] > 0
    assert not multiprocessing.active_children()


def test_comparison_applies_and_reports_the_requested_calibration(
    tmp_path, monkeypatch
):
    from unittest.mock import Mock
    import benchmark_comparison as benchmark

    profile = BenchmarkProfile(
        "calibrated",
        tmp_path / "model.onnx",
        "cpu",
        confidence=0.3,
        gamma=1.0,
        tile_size=512,
        tile_overlap=0.25,
        sha256="ab" * 32,
    )
    profile.path.write_bytes(b"fixture")
    model = Mock()
    model.detect.return_value = []
    model.metadata.return_value = {"adapter": "fixture"}
    loader = Mock(return_value=model)
    monkeypatch.setattr(benchmark, "load_detector", loader)
    output = tmp_path / "results"
    output.mkdir()
    frame = np.full((10, 10, 3), 80, np.uint8)
    mapping = config(tmp_path).inputs[0].mapping
    comparison_pass(profile, mapping, [(0, frame)], output, warmup=0)
    assert loader.call_args.kwargs == {
        "device": "cpu",
        "tile_size": 512,
        "tile_overlap": 0.25,
        "tile_scope": "frame",
        "mapping": mapping,
        "provider": "default",
        "precision": "fp32",
        "engine_cache": None,
        "expected_sha256": "ab" * 32,
    }
    np.testing.assert_array_equal(model.detect.call_args.args[0], frame)
    assert model.detect.call_args.kwargs == {"confidence": 0.3}
    report = json.loads((output / "calibrated.json").read_text())
    assert report["parameters"] == {
        "confidence": 0.3,
        "gamma": 1.0,
        "tile_size": 512,
        "tile_overlap": 0.25,
        "tile_scope": "frame",
        "provider": "default",
        "precision": "fp32",
    }
