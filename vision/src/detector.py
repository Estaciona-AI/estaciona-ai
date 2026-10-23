# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Inference for official Paddle RT-DETR ONNX exports (HGNetv2-L, R18).

No model download or deserialization of PyTorch artifacts occurs here. TensorRT
engines are built locally by ONNX Runtime from the ONNX model and cached.
"""

import ctypes
import importlib.util
from pathlib import Path
from contextlib import nullcontext
from typing import Protocol

import cv2
import numpy as np

from tiling import detect_tiled, spot_windows, windows

INPUT_SIZE = 640
MAX_BATCH = 64  # Equals the tiling window limit.
VEHICLE_CLASSES = frozenset((2, 7))  # Dense COCO-80 indices: car and truck.


def _image_tensor(frame, out=None):
    image = cv2.resize(frame, (INPUT_SIZE, INPUT_SIZE), interpolation=cv2.INTER_CUBIC)
    tensor = (
        np.empty((1, 3, INPUT_SIZE, INPUT_SIZE), np.float32) if out is None else out
    )
    # Copy BGR directly into contiguous RGB planes. Keep division (not a
    # reciprocal multiply) to preserve the original FP32 rounding exactly.
    tensor[0] = image[:, :, ::-1].transpose(2, 0, 1)
    np.divide(tensor, np.float32(255), out=tensor)
    return tensor


def _vehicle_candidates(rows, width, height, confidence):
    # Paddle DETRPostProcess already returns original-image coordinates.
    rows = np.asarray(rows)
    if rows.ndim == 3 and rows.shape[0] == 1:
        rows = rows[0]
    if rows.ndim != 2 or rows.shape[1] != 6:
        raise ValueError("Unexpected official RT-DETR output; expected [N,6]")
    labels, scores = rows[:, 0], rows[:, 1]
    selected = rows[
        np.isfinite(rows).all(axis=1)
        & np.isin(labels, tuple(VEHICLE_CLASSES))
        & (scores >= confidence)
        & (scores <= 1)
    ]
    boxes = np.clip(selected[:, 2:], 0, [width, height, width, height]).astype(int)
    valid = (boxes[:, 2] > boxes[:, 0]) & (boxes[:, 3] > boxes[:, 1])
    return [
        (float(score), tuple(box))
        for score, box in zip(selected[valid, 1], boxes[valid].tolist(), strict=True)
    ]


TENSORRT_RUNTIME = (
    "libnvinfer.so.10",
    "libnvinfer_plugin.so.10",
    "libnvonnxparser.so.10",
)


def _load_tensorrt_runtime():
    # Importing tensorrt_libs loads the builder resources of every GPU
    # architecture (~2.3 GB more RSS per process). libnvinfer opens only the
    # one it needs, and only when an engine is not cached yet.
    spec = importlib.util.find_spec("tensorrt_libs")
    if spec is None or not spec.submodule_search_locations:
        return
    directory = Path(spec.submodule_search_locations[0])
    for name in TENSORRT_RUNTIME:
        if (directory / name).is_file():
            ctypes.CDLL(str(directory / name), mode=ctypes.RTLD_GLOBAL)


class Detector(Protocol):
    def detect(
        self, frame: np.ndarray, confidence: float = 0.1
    ) -> list[tuple[int, int, int, int]]: ...

    def metadata(self) -> dict: ...


class RTDetrDetector:
    def __init__(
        self,
        session,
        *,
        metrics=None,
        tile_size=0,
        tile_overlap=0.25,
        tile_regions=None,
        tile_scope="frame",
        batch=1,
        precision="float32",
    ):
        if type(batch) is not int or not 1 <= batch <= MAX_BATCH:
            raise ValueError("invalid inference batch")
        if type(tile_size) is not int or tile_size < 0:
            raise ValueError("invalid tile size")
        if tile_size:
            windows(tile_size, tile_size, tile_size, tile_overlap)
        elif tile_regions is not None:
            raise ValueError("tile regions require a tile size")
        self.tile_size = tile_size
        self.tile_overlap = tile_overlap
        # ((width, height), windows) fixed by the input mapping, or None.
        self.tile_regions = tile_regions
        self.tile_scope = tile_scope
        self.batch = batch
        self.precision = precision
        self.session = session
        self.metrics = metrics
        self.inputs = {item.name: item for item in session.get_inputs()}
        if "image" not in self.inputs or not self.inputs.keys() <= {
            "image",
            "im_shape",
            "scale_factor",
        }:
            raise ValueError(
                "Expected the official Paddle RT-DETR ONNX input signature"
            )
        image = self.inputs["image"]
        if image.type != "tensor(float)" or image.shape[-3:] != [
            3,
            INPUT_SIZE,
            INPUT_SIZE,
        ]:
            raise ValueError("Expected official FP32 input [batch,3,640,640]")

    def metadata(self):
        return {
            "adapter": "rtdetr",
            "tile_size": self.tile_size,
            "tile_overlap": self.tile_overlap,
            "tile_scope": self.tile_scope if self.tile_size else None,
            "tile_count": len(self.tile_regions[1]) if self.tile_regions else None,
            "runtime_version": getattr(self, "runtime_version", None),
            "providers": list(self.session.get_providers()),
            "input_size": [INPUT_SIZE, INPUT_SIZE],
            "precision": self.precision,
            "batch": self.batch,
            "vehicle_classes": sorted(VEHICLE_CLASSES),
        }

    def detect(self, frame, confidence=0.1):
        if frame.dtype != np.uint8 or frame.ndim != 3 or frame.shape[2] != 3:
            raise ValueError("Expected a uint8 BGR frame")
        height, width = frame.shape[:2]
        if height == 0 or width == 0 or not 0 <= confidence <= 1:
            raise ValueError("Invalid frame size or confidence")
        if self.tile_size:
            if self.tile_regions is None:
                regions = windows(width, height, self.tile_size, self.tile_overlap)
            elif self.tile_regions[0] != (width, height):
                raise ValueError("frame does not match the configured tile regions")
            else:
                regions = self.tile_regions[1]
            return detect_tiled(frame, self._candidates, confidence, regions)
        return [box for _, box in self._candidates([frame], confidence)[0]]

    def _candidates(self, frames, confidence):
        found = []
        for start in range(0, len(frames), self.batch):
            found.extend(self._run(frames[start : start + self.batch], confidence))
        return found

    def _run(self, frames, confidence):
        with (
            self.metrics.stage("detector_preprocess_ms")
            if self.metrics
            else nullcontext()
        ):
            count = len(frames)
            image = np.empty((count, 3, INPUT_SIZE, INPUT_SIZE), np.float32)
            for index, frame in enumerate(frames):
                _image_tensor(frame, out=image[index : index + 1])
            feed = {
                "image": image,
                "im_shape": np.full((count, 2), INPUT_SIZE, np.float32),
                "scale_factor": np.array(
                    [
                        [INPUT_SIZE / f.shape[0], INPUT_SIZE / f.shape[1]]
                        for f in frames
                    ],
                    np.float32,
                ),
            }
        with self.metrics.stage("inference_ms") if self.metrics else nullcontext():
            outputs = self.session.run(None, {key: feed[key] for key in self.inputs})
        with (
            self.metrics.stage("detector_postprocess_ms")
            if self.metrics
            else nullcontext()
        ):
            if count == 1:
                parts = [outputs[0]]
            else:
                # bbox_num holds the row count of each image in the batch.
                rows = np.asarray(outputs[0])
                counts = np.asarray(outputs[1]).reshape(-1)
                if (
                    rows.ndim != 2
                    or len(counts) != count
                    or (counts < 0).any()
                    or counts.sum() != len(rows)
                ):
                    raise ValueError("Unexpected official RT-DETR batch output")
                parts = np.split(rows, np.cumsum(counts)[:-1])
            return [
                _vehicle_candidates(rows, f.shape[1], f.shape[0], confidence)
                for rows, f in zip(parts, frames, strict=True)
            ]


def load_detector(
    path,
    device="cuda",
    *,
    runtime=None,
    tile_size=0,
    tile_overlap=0.25,
    tile_scope="frame",
    mapping=None,
    provider="default",
    precision="fp32",
    engine_cache=None,
):
    path = Path(path)
    if provider not in ("default", "tensorrt") or precision not in ("fp32", "fp16"):
        raise ValueError("unsupported detector provider or precision")
    if provider == "tensorrt" and (
        device != "cuda" or engine_cache is None or not Path(engine_cache).is_dir()
    ):
        raise ValueError(
            "TensorRT requires CUDA and an existing engine cache directory"
        )
    if precision == "fp16" and provider != "tensorrt":
        raise ValueError("FP16 inference requires the TensorRT provider")
    if tile_scope not in ("frame", "spots") or (
        tile_scope == "spots" and (not tile_size or mapping is None)
    ):
        raise ValueError("spot tile scope requires a tile size and an input mapping")
    if path.suffix.lower() != ".onnx":
        raise ValueError(
            "Only the official ONNX export is supported; regenerate legacy models"
        )
    if not path.is_file():
        raise FileNotFoundError(
            f"RT-DETR ONNX model not found: {path}; run tools/export_rtdetr.py"
        )
    if device not in ("cuda", "cpu"):
        raise ValueError("VISION_DEVICE must be cuda or cpu")
    if runtime is None:
        import onnxruntime as runtime
    if device == "cuda" and hasattr(runtime, "preload_dlls"):
        runtime.preload_dlls(directory="")
    regions = None
    if tile_scope == "spots":
        size = (mapping.width, mapping.height)
        regions = (
            size,
            spot_windows(*size, tile_size, tile_overlap, mapping.spots.values()),
        )
    elif tile_size and mapping is not None:
        size = (mapping.width, mapping.height)
        regions = (size, windows(*size, tile_size, tile_overlap))
    # Batching only pays off with TensorRT; on the CUDA provider it measured
    # slower than one crop per call, so other providers keep batch 1.
    batch = len(regions[1]) if provider == "tensorrt" and regions else 1
    selected = "CUDAExecutionProvider" if device == "cuda" else "CPUExecutionProvider"
    providers = [selected]
    if provider == "tensorrt":
        _load_tensorrt_runtime()
        selected = "TensorrtExecutionProvider"

        def shapes(n):
            return f"image:{n}x3x{INPUT_SIZE}x{INPUT_SIZE},im_shape:{n}x2,scale_factor:{n}x2"

        providers = [
            (
                selected,
                {
                    "trt_fp16_enable": precision == "fp16",
                    "trt_engine_cache_enable": True,
                    "trt_engine_cache_path": str(engine_cache),
                    # Cache names ignore profile shapes: inputs with different
                    # tile counts would overwrite and rebuild each other.
                    "trt_engine_cache_prefix": f"rtdetr_b{batch}",
                    "trt_profile_min_shapes": shapes(1),
                    "trt_profile_opt_shapes": shapes(batch),
                    "trt_profile_max_shapes": shapes(batch),
                },
            ),
            "CUDAExecutionProvider",
        ]
    if selected not in runtime.get_available_providers():
        raise RuntimeError(f"Requested {selected} is unavailable")
    options = runtime.SessionOptions()
    options.intra_op_num_threads = 1
    session = runtime.InferenceSession(
        str(path), sess_options=options, providers=providers
    )
    # ONNX Runtime silently falls back to the next provider when one fails.
    if session.get_providers()[0] != selected:
        if provider == "tensorrt":
            raise RuntimeError(
                "TensorRT initialization failed; install the tensorrt extra"
            )
        raise RuntimeError(
            "CUDA initialization failed; install the cuda extra or select VISION_DEVICE=cpu"
        )
    print(f"[VISION MODEL] RT-DETR / ONNX Runtime / {selected} / {precision}")
    detector = RTDetrDetector(
        session,
        tile_size=tile_size,
        tile_overlap=tile_overlap,
        tile_regions=regions,
        tile_scope=tile_scope,
        batch=batch,
        precision="float16" if precision == "fp16" else "float32",
    )
    detector.runtime_version = getattr(runtime, "__version__", None)
    return detector
