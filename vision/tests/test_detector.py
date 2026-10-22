# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest
import cv2


def load_detector():
    path = Path(__file__).resolve().parents[1] / "src/detector.py"
    spec = importlib.util.spec_from_file_location("permissive_detector", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def session(rows):
    value = Mock()
    value.get_providers.return_value = ["CPUExecutionProvider"]
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


@pytest.mark.parametrize("shape", [(512, 512, 3), (108, 192, 3), (7, 11, 3)])
@pytest.mark.parametrize("strided", [False, True])
def test_image_tensor_is_bitwise_equal_to_official_preprocessing(shape, strided):
    frame = np.random.default_rng(42).integers(0, 256, shape, dtype=np.uint8)
    if strided:
        frame = frame[::2, ::-1]
    original = frame.copy()
    resized = cv2.resize(frame, (640, 640), interpolation=cv2.INTER_CUBIC)
    expected = cv2.cvtColor(resized, cv2.COLOR_BGR2RGB).astype(np.float32) / 255.0
    expected = np.ascontiguousarray(expected.transpose(2, 0, 1)[None])

    actual = load_detector()._image_tensor(frame)

    assert actual.shape == (1, 3, 640, 640)
    assert actual.dtype == np.float32
    assert actual.flags.c_contiguous
    assert np.array_equal(actual, expected)
    assert np.array_equal(frame, original)
    assert not np.shares_memory(actual, frame)


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


def test_tiled_detection_remaps_deduplicates_and_preserves_outer_border():
    model = session([])
    # Two 512px crops over a 768px frame: the same car appears twice.
    model.run.side_effect = [
        [
            np.array(
                [
                    [2, 0.8, 300, 20, 400, 120],
                    [2, 0.9, 0, 20, 60, 120],
                    [2, 0.99, 480, 20, 512, 120],
                ],
                np.float32,
            )
        ],
        [np.array([[2, 0.9, 44, 20, 144, 120]], np.float32)],
    ]
    detector = load_detector().RTDetrDetector(model, tile_size=512)
    boxes = detector.detect(np.zeros((512, 768, 3), np.uint8), confidence=0.5)
    assert set(boxes) == {(300, 20, 400, 120), (0, 20, 60, 120)}
    assert model.run.call_count == 2
    assert detector.metadata()["tile_size"] == 512


def test_tiled_detection_rejects_excessive_work_before_inference():
    model = session([])
    detector = load_detector().RTDetrDetector(model, tile_size=256)
    with pytest.raises(ValueError, match="tile"):
        detector.detect(np.zeros((2048, 4096, 3), np.uint8))
    model.run.assert_not_called()


def test_tile_grid_covers_frame_without_gaps():
    from tiling import windows

    width, height = 1920, 1080
    covered = np.zeros((height, width), np.bool_)
    tiles = windows(width, height, 512, 0.25)
    assert len(tiles) == 15
    for x, y, w, h in tiles:
        assert 0 <= x < x + w <= width
        assert 0 <= y < y + h <= height
        covered[y : y + h, x : x + w] = True
    assert covered.all()
    assert windows(100, 80, 512, 0.25) == [(0, 0, 100, 80)]


@pytest.mark.parametrize(
    "size,overlap",
    [(True, 0.25), (128, 0.25), (512, float("nan")), (512, True), (512, 0.9)],
)
def test_bad_tiling_parameters_fail_before_model_execution(size, overlap):
    with pytest.raises(ValueError, match="tile"):
        load_detector().RTDetrDetector(
            session([]), tile_size=size, tile_overlap=overlap
        )


def test_tiled_detection_suppresses_partial_duplicate_without_removing_neighbor():
    model = session(
        [
            [2, 0.86, 100, 145, 191, 211],
            [2, 0.49, 138, 134, 196, 170],
            [2, 0.91, 81, 190, 172, 261],
        ]
    )
    detector = load_detector().RTDetrDetector(model, tile_size=512)
    assert set(detector.detect(np.zeros((512, 512, 3), np.uint8), confidence=0.3)) == {
        (100, 145, 191, 211),
        (81, 190, 172, 261),
    }
