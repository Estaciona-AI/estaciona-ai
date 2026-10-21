# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest


def load_detector():
    path = Path(__file__).resolve().parents[1] / "src/detector.py"
    spec = importlib.util.spec_from_file_location("permissive_detector", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def session(rows):
    value = Mock()
    value.get_inputs.return_value = [
        SimpleNamespace(name="image", shape=[1, 3, 640, 640], type="tensor(float)"),
        SimpleNamespace(name="im_shape", shape=[1, 2], type="tensor(float)"),
        SimpleNamespace(name="scale_factor", shape=[1, 2], type="tensor(float)"),
    ]
    value.get_outputs.return_value = [
        SimpleNamespace(name="bbox"),
        SimpleNamespace(name="bbox_num"),
    ]
    value.run.return_value = [
        np.asarray(rows, dtype=np.float32).reshape(-1, 6),
        np.array([len(rows)], dtype=np.int32),
    ]
    return value


def test_official_preprocessing_preserves_rgb_and_original_coordinate_scaling():
    model = session([[2, 0.9, -10, 20, 1000, 400]])
    detector = load_detector().RTDetrDetector(model)
    frame = np.zeros((540, 960, 3), dtype=np.uint8)
    frame[:, :, 2] = 255
    boxes = detector.detect(frame, confidence=0.1)
    assert boxes == [(0, 20, 960, 400)]
    inputs = model.run.call_args.args[1]
    assert inputs["image"].shape == (1, 3, 640, 640)
    assert inputs["image"].dtype == np.float32
    assert np.all(inputs["image"][0, 0] == 1.0)
    assert np.all(inputs["image"][0, 2] == 0.0)
    np.testing.assert_allclose(inputs["scale_factor"], [[640 / 540, 640 / 960]])
    np.testing.assert_allclose(inputs["im_shape"], [[640, 640]])


def test_only_confident_cars_and_trucks_with_finite_valid_boxes_are_accepted():
    detector = load_detector().RTDetrDetector(
        session(
            [
                [2, 0.8, 10, 20, 40, 50],
                [7, 0.7, 50, 60, 80, 90],
                [0, 0.99, 1, 2, 3, 4],
                [2, 0.09, 1, 2, 3, 4],
                [2, 0.9, 20, 20, 10, 40],
                [2, float("nan"), 1, 2, 3, 4],
                [2, 0.9, 1, 2, float("inf"), 4],
            ]
        )
    )
    assert detector.detect(np.zeros((100, 100, 3), np.uint8), confidence=0.1) == [
        (10, 20, 40, 50),
        (50, 60, 80, 90),
    ]


def test_rejects_ultralytics_output_signature_and_malformed_outputs():
    model = session([])
    model.get_inputs.return_value = [
        SimpleNamespace(name="images", shape=[1, 3, 640, 640], type="tensor(float)")
    ]
    with pytest.raises(ValueError, match="official"):
        load_detector().RTDetrDetector(model)
    model = session([])
    detector = load_detector().RTDetrDetector(model)
    model.run.return_value = [np.zeros((1, 300, 84), np.float32), np.array([300])]
    with pytest.raises(ValueError, match="output"):
        detector.detect(np.zeros((10, 10, 3), np.uint8))


def test_gpu_request_fails_when_runtime_falls_back_to_cpu(tmp_path):
    module = load_detector()
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    model = session([])
    model.get_providers.return_value = ["CPUExecutionProvider"]
    runtime = Mock()
    runtime.get_available_providers.return_value = [
        "CUDAExecutionProvider",
        "CPUExecutionProvider",
    ]
    runtime.InferenceSession.return_value = model
    with pytest.raises(RuntimeError, match="CUDA"):
        module.load_detector(path, device="cuda", runtime=runtime)


def test_legacy_model_formats_are_not_loaded(tmp_path):
    path = tmp_path / "rtdetr-l.engine"
    path.write_bytes(b"legacy")
    with pytest.raises(ValueError, match="ONNX"):
        load_detector().load_detector(path, device="cpu", runtime=Mock())


def test_runtime_does_not_install_the_ultralytics_package():
    assert importlib.util.find_spec("ultralytics") is None


@pytest.mark.parametrize("suffix", [".onnx", ".LICENSE", ".provenance.json"])
def test_export_does_not_overwrite_existing_model_or_sidecars(tmp_path, suffix):
    path = Path(__file__).resolve().parents[1] / "tools/export_rtdetr.py"
    spec = importlib.util.spec_from_file_location("official_exporter", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    output = tmp_path / "model.onnx"
    existing = output.with_suffix(suffix)
    existing.write_bytes(b"operator-owned")
    with pytest.raises(FileExistsError):
        module.export(output)
    assert existing.read_bytes() == b"operator-owned"


def test_metadata_records_actual_runtime_and_stage_metrics():
    from metrics import Metrics

    module = load_detector()
    model = session([[2, 0.9, 0, 0, 9, 9]])
    model.get_providers.return_value = ["CPUExecutionProvider"]
    metrics = Metrics()
    detector = module.RTDetrDetector(model, metrics=metrics)
    assert detector.detect(np.zeros((10, 10, 3), np.uint8)) == [(0, 0, 9, 9)]
    metadata = detector.metadata()
    assert metadata["providers"] == ["CPUExecutionProvider"]
    assert metadata["input_size"] == [640, 640]
    assert metadata["precision"] == "float32"
    assert set(metrics.snapshot()["stages"]) >= {
        "detector_preprocess_ms",
        "inference_ms",
        "detector_postprocess_ms",
    }
