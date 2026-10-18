# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Inference for the official Paddle RT-DETR HGNetv2-L ONNX export.

No model download or deserialization of PyTorch/TensorRT artifacts occurs here.
"""

from pathlib import Path

import cv2
import numpy as np

INPUT_SIZE = 640
VEHICLE_CLASSES = frozenset((2, 7))  # Dense COCO-80 indices: car and truck.


class RTDetrDetector:
    def __init__(self, session):
        self.session = session
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

    def detect(self, frame, confidence=0.1):
        if frame.dtype != np.uint8 or frame.ndim != 3 or frame.shape[2] != 3:
            raise ValueError("Expected a uint8 BGR frame")
        height, width = frame.shape[:2]
        if height == 0 or width == 0 or not 0 <= confidence <= 1:
            raise ValueError("Invalid frame size or confidence")
        image = cv2.resize(
            frame, (INPUT_SIZE, INPUT_SIZE), interpolation=cv2.INTER_CUBIC
        )
        image = cv2.cvtColor(image, cv2.COLOR_BGR2RGB).astype(np.float32) / 255.0
        feed = {
            "image": np.ascontiguousarray(image.transpose(2, 0, 1)[None]),
            "im_shape": np.array([[INPUT_SIZE, INPUT_SIZE]], dtype=np.float32),
            "scale_factor": np.array(
                [[INPUT_SIZE / height, INPUT_SIZE / width]], dtype=np.float32
            ),
        }
        outputs = self.session.run(None, {key: feed[key] for key in self.inputs})
        # Paddle DETRPostProcess returns original-image coordinates, dense COCO
        # labels, and scores. Do not rescale the returned boxes a second time.
        rows = np.asarray(outputs[0])
        if rows.ndim == 3 and rows.shape[0] == 1:
            rows = rows[0]
        if rows.ndim != 2 or rows.shape[1] != 6:
            raise ValueError("Unexpected official RT-DETR output; expected [N,6]")
        boxes = []
        for label, score, x1, y1, x2, y2 in rows:
            if not np.isfinite([label, score, x1, y1, x2, y2]).all():
                continue
            if label not in VEHICLE_CLASSES or not confidence <= score <= 1:
                continue
            x1, x2 = np.clip([x1, x2], 0, width).astype(int)
            y1, y2 = np.clip([y1, y2], 0, height).astype(int)
            if x2 > x1 and y2 > y1:
                boxes.append((int(x1), int(y1), int(x2), int(y2)))
        return boxes


def load_detector(path, device="cuda", *, runtime=None):
    path = Path(path)
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
    provider = "CUDAExecutionProvider" if device == "cuda" else "CPUExecutionProvider"
    if provider not in runtime.get_available_providers():
        raise RuntimeError(f"Requested {provider} is unavailable")
    options = runtime.SessionOptions()
    options.intra_op_num_threads = 1
    session = runtime.InferenceSession(
        str(path), sess_options=options, providers=[provider]
    )
    if provider not in session.get_providers():
        raise RuntimeError(
            "CUDA initialization failed; install the cuda extra or select VISION_DEVICE=cpu"
        )
    print(f"[VISION MODEL] RT-DETR HGNetv2-L / ONNX Runtime / {provider}")
    return RTDetrDetector(session)
