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


def test_vehicle_candidates_preserve_order_clipping_and_score_boundaries():
    rows = np.array(
        [
            [7, 1.0, -2.5, 2.9, 120, 90],
            [2, 0.5, 8.9, 3.9, 20.1, 15.9],
            [7, 0.499, 1, 2, 3, 4],
            [2, 1.001, 1, 2, 3, 4],
            [2.1, 0.9, 1, 2, 3, 4],
            [0, 0.9, 1, 2, 3, 4],
            [2, np.nan, 1, 2, 3, 4],
            [np.nan, 0.9, 1, 2, 3, 4],
            [2, 0.9, 1, 2, np.inf, 4],
            [2, 0.9, 20, 20, 10, 40],
            [2, 0.9, -4, -4, -1, -1],
            [2, 0.9, 3.1, 2, 3.9, 4],
        ],
        np.float32,
    )
    before = rows.copy()
    result = load_detector()._vehicle_candidates(rows, 100, 80, 0.5)
    assert result == [(1.0, (0, 2, 100, 80)), (0.5, (8, 3, 20, 15))]
    np.testing.assert_array_equal(rows, before)


@pytest.mark.parametrize("shape", [(0, 6), (1, 0, 6)])
def test_vehicle_candidates_allow_empty_official_outputs(shape):
    assert load_detector()._vehicle_candidates(np.empty(shape), 100, 80, 0.3) == []


@pytest.mark.parametrize("shape", [(6,), (1, 300, 84), (2, 300, 6)])
def test_vehicle_candidates_reject_nonofficial_output_shapes(shape):
    with pytest.raises(ValueError, match="output"):
        load_detector()._vehicle_candidates(np.zeros(shape), 100, 80, 0.3)


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


SPOTS_1080P = {
    "near-origin": [[10, 10], [60, 10], [60, 60], [10, 60]],
    "far-corner": [[1800, 1000], [1900, 1000], [1900, 1070], [1800, 1070]],
}


def test_spot_windows_keep_only_tiles_that_intersect_polygons():
    from tiling import spot_windows, windows

    every = windows(1920, 1080, 512, 0.25)
    selected = spot_windows(1920, 1080, 512, 0.25, SPOTS_1080P.values())
    assert set(selected) < set(every)
    covered = np.zeros((1080, 1920), np.bool_)
    for x, y, w, h in selected:
        covered[y : y + h, x : x + w] = True
    for points in SPOTS_1080P.values():
        mask = np.zeros((1080, 1920), np.uint8)
        cv2.fillPoly(mask, [np.array(points, np.int32)], 1)
        assert covered[mask.astype(bool)].all()
    unused = set(every) - set(selected)
    assert (0, 0, 512, 512) in selected and (704, 284, 512, 512) in unused


def test_spot_windows_reject_mapping_without_spots():
    from tiling import spot_windows

    with pytest.raises(ValueError, match="spot"):
        spot_windows(1920, 1080, 512, 0.25, [])


def test_spot_scoped_detector_skips_tiles_and_rejects_other_frame_sizes():
    model = session([])
    detector = load_detector().RTDetrDetector(
        model, tile_size=512, tile_regions=((768, 512), [(0, 0, 512, 512)])
    )
    assert detector.detect(np.zeros((512, 768, 3), np.uint8)) == []
    assert model.run.call_count == 1
    with pytest.raises(ValueError, match="regions"):
        detector.detect(np.zeros((512, 512, 3), np.uint8))
    assert model.run.call_count == 1
    assert detector.metadata()["tile_count"] == 1


def test_load_detector_derives_spot_tiles_from_mapping(tmp_path):
    module = load_detector()
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    runtime = Mock()
    runtime.get_available_providers.return_value = ["CPUExecutionProvider"]
    runtime.InferenceSession.return_value = session([])
    mapping = SimpleNamespace(width=1920, height=1080, spots=SPOTS_1080P)
    detector = module.load_detector(
        path,
        device="cpu",
        runtime=runtime,
        tile_size=512,
        tile_scope="spots",
        mapping=mapping,
    )
    metadata = detector.metadata()
    assert metadata["tile_scope"] == "spots"
    assert 0 < metadata["tile_count"] < 15


@pytest.mark.parametrize(
    "options",
    [
        {"tile_scope": "spots"},
        {"tile_scope": "spots", "tile_size": 512},
        {"tile_scope": "everything", "tile_size": 512},
    ],
)
def test_spot_tile_scope_requires_tiling_and_mapping(tmp_path, options):
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    runtime = Mock()
    runtime.get_available_providers.return_value = ["CPUExecutionProvider"]
    runtime.InferenceSession.return_value = session([])
    with pytest.raises(ValueError, match="tile"):
        load_detector().load_detector(path, device="cpu", runtime=runtime, **options)


def batched_session(per_image):
    model = session([])
    rows = [row for image in per_image for row in image]
    model.run.return_value = [
        np.asarray(rows, np.float32).reshape(-1, 6),
        np.array([len(image) for image in per_image], np.int32),
    ]
    return model


def test_batched_tiles_run_once_and_split_rows_by_image():
    model = batched_session(
        [[[2, 0.9, 100, 100, 200, 200]], [[2, 0.8, 100, 100, 200, 200]]]
    )
    detector = load_detector().RTDetrDetector(
        model,
        tile_size=512,
        tile_regions=((1024, 512), [(0, 0, 512, 512), (512, 0, 512, 512)]),
        batch=2,
    )
    boxes = detector.detect(np.zeros((512, 1024, 3), np.uint8), confidence=0.5)
    assert set(boxes) == {(100, 100, 200, 200), (612, 100, 712, 200)}
    assert model.run.call_count == 1
    feed = model.run.call_args.args[1]
    assert feed["image"].shape == (2, 3, 640, 640)
    assert feed["scale_factor"].shape == (2, 2)
    assert detector.metadata()["batch"] == 2


def test_batched_output_with_inconsistent_counts_is_rejected():
    model = session([[2, 0.9, 1, 1, 9, 9]])
    model.run.return_value[1] = np.array([1, 1], np.int32)
    detector = load_detector().RTDetrDetector(
        model,
        tile_size=512,
        tile_regions=((1024, 512), [(0, 0, 512, 512), (512, 0, 512, 512)]),
        batch=2,
    )
    with pytest.raises(ValueError, match="batch"):
        detector.detect(np.zeros((512, 1024, 3), np.uint8))


@pytest.mark.parametrize("batch", [0, 65, True, 2.0])
def test_invalid_batch_is_rejected(batch):
    with pytest.raises(ValueError, match="batch"):
        load_detector().RTDetrDetector(session([]), batch=batch)


def tensorrt_runtime(active):
    runtime = Mock()
    runtime.get_available_providers.return_value = [
        "TensorrtExecutionProvider",
        "CUDAExecutionProvider",
        "CPUExecutionProvider",
    ]
    model = session([])
    model.get_providers.return_value = active
    runtime.InferenceSession.return_value = model
    return runtime


def test_tensorrt_profile_builds_fp16_engine_batched_to_spot_tiles(tmp_path):
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    cache = tmp_path / "engines"
    cache.mkdir()
    runtime = tensorrt_runtime(["TensorrtExecutionProvider", "CUDAExecutionProvider"])
    detector = load_detector().load_detector(
        path,
        device="cuda",
        runtime=runtime,
        provider="tensorrt",
        precision="fp16",
        engine_cache=cache,
        tile_size=512,
        tile_scope="spots",
        mapping=SimpleNamespace(width=1920, height=1080, spots=SPOTS_1080P),
    )
    metadata = detector.metadata()
    count = metadata["tile_count"]
    name, options = runtime.InferenceSession.call_args.kwargs["providers"][0]
    assert name == "TensorrtExecutionProvider"
    assert options["trt_fp16_enable"] is True
    assert options["trt_engine_cache_enable"] is True
    assert options["trt_engine_cache_path"] == str(cache)
    shapes = f"image:{count}x3x640x640,im_shape:{count}x2,scale_factor:{count}x2"
    assert options["trt_profile_opt_shapes"] == shapes
    assert options["trt_profile_max_shapes"] == shapes
    assert (metadata["precision"], metadata["batch"]) == ("float16", count)
    assert metadata["providers"][0] == "TensorrtExecutionProvider"


def test_tensorrt_without_tiles_uses_single_image_engine(tmp_path):
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    runtime = tensorrt_runtime(["TensorrtExecutionProvider", "CUDAExecutionProvider"])
    detector = load_detector().load_detector(
        path, device="cuda", runtime=runtime, provider="tensorrt", engine_cache=tmp_path
    )
    _, options = runtime.InferenceSession.call_args.kwargs["providers"][0]
    assert options["trt_fp16_enable"] is False
    assert options["trt_profile_max_shapes"].startswith("image:1x3x640x640")
    assert (detector.metadata()["precision"], detector.metadata()["batch"]) == (
        "float32",
        1,
    )


def test_silent_tensorrt_fallback_is_rejected(tmp_path):
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    runtime = tensorrt_runtime(["CUDAExecutionProvider", "CPUExecutionProvider"])
    with pytest.raises(RuntimeError, match="TensorRT"):
        load_detector().load_detector(
            path,
            device="cuda",
            runtime=runtime,
            provider="tensorrt",
            engine_cache=tmp_path,
        )


@pytest.mark.parametrize(
    "options",
    [
        {"provider": "tensorrt", "device": "cpu"},
        {"provider": "tensorrt", "engine_cache": None},
        {"provider": "tensorrt", "engine_cache": "missing"},
        {"precision": "fp16"},
        {"provider": "openvino"},
        {"precision": "int8", "provider": "tensorrt"},
    ],
)
def test_invalid_accelerator_combinations_fail_before_session(tmp_path, options):
    path = tmp_path / "model.onnx"
    path.write_bytes(b"fixture")
    options = {"device": "cuda", "engine_cache": tmp_path, **options}
    if options["engine_cache"] == "missing":
        options["engine_cache"] = tmp_path / "missing"
    runtime = tensorrt_runtime(["TensorrtExecutionProvider"])
    with pytest.raises(ValueError):
        load_detector().load_detector(path, runtime=runtime, **options)
    runtime.InferenceSession.assert_not_called()


def test_export_rejects_unknown_variant_before_any_download(tmp_path):
    path = Path(__file__).resolve().parents[1] / "tools/export_rtdetr.py"
    spec = importlib.util.spec_from_file_location("official_exporter", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert set(module.VARIANTS) == {"l", "r18"}
    with pytest.raises(ValueError, match="variant"):
        module.export(tmp_path / "model.onnx", "x")
    assert not any(tmp_path.iterdir())
