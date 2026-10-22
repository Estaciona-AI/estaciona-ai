# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Frozen local frames and editable visual references; never labels from a model."""

import hashlib
import html
import math
from pathlib import Path
import re

import cv2
import numpy as np

from benchmark_comparison import video_frames
from benchmark_profiles import artifact, digest_json, results_directory
from configuration import _fields, _id, _read
from metrics import atomic_json

LABELS = {"occupied", "free", "uncertain"}


def _hash(value):
    if not isinstance(value, str) or not re.fullmatch("[0-9a-f]{64}", value):
        raise ValueError("invalid corpus hash")
    return value


def build_corpus(config, video, output, *, input_id, frames=24, stride=12, warmup=3):
    if (
        config.environment == "production"
        or type(frames) is not int
        or not 2 <= frames <= 120
        or type(stride) is not int
        or not 1 <= stride <= 1000
        or type(warmup) is not int
        or not 0 <= warmup <= 100
    ):
        raise ValueError("invalid corpus limits")
    item = next((i for i in config.inputs if i.input_id == input_id), None)
    video = Path(video).resolve()
    if item is None or not video.is_file() or frames * len(item.mapping.spots) > 50000:
        raise ValueError("invalid corpus input or label budget")
    source = artifact(video)
    capture = cv2.VideoCapture(str(video))
    try:
        count = capture.get(cv2.CAP_PROP_FRAME_COUNT)
    finally:
        capture.release()
    if not math.isfinite(count) or not 1 <= count <= 1_000_000:
        raise ValueError("invalid local video frame count")
    samples = math.ceil(count / stride)
    measured = samples - warmup
    if not frames <= measured <= 1000:
        raise ValueError("not enough samples or comparison limit exceeded")
    selected = set(np.linspace(warmup, samples - 1, frames).astype(int) * stride)
    stream = video_frames(video, samples=samples, stride=stride)
    try:
        fps = next(stream)
        output = results_directory(output)
        rows = []
        for index, frame in stream:
            if frame.shape != (item.mapping.height, item.mapping.width, 3):
                raise ValueError("video does not match mapping")
            if index not in selected:
                continue
            preview = frame.copy()
            for spot_id, points in item.mapping.spots.items():
                polygon = np.asarray(points, dtype=np.int32)
                cv2.polylines(preview, [polygon], True, (0, 220, 255), 2)
                cv2.putText(
                    preview,
                    spot_id,
                    tuple(polygon.mean(0).astype(int)),
                    cv2.FONT_HERSHEY_SIMPLEX,
                    0.45,
                    (0, 80, 255),
                    1,
                )
            image = output / f"frame-{index:06d}.jpg"
            if not cv2.imwrite(str(image), preview):
                raise OSError("cannot write corpus preview")
            rows.append(
                {
                    "frame_index": index,
                    "frame_sha256": hashlib.sha256(frame.tobytes()).hexdigest(),
                    "preview": image.name,
                    "labels": {key: "uncertain" for key in item.mapping.spots},
                }
            )
        if len(rows) != frames or artifact(video) != source:
            raise ValueError("video incomplete or changed while building corpus")
        corpus = {
            "schema_version": 1,
            "video": source,
            "input_id": input_id,
            "mapping_sha256": digest_json(
                {
                    "width": item.mapping.width,
                    "height": item.mapping.height,
                    "spots": item.mapping.spots,
                }
            ),
            "frame_size": [item.mapping.width, item.mapping.height],
            "fps": fps,
            "decoder": cv2.__version__,
            "spot_ids": list(item.mapping.spots),
            "sampling": {
                "stride": stride,
                "warmup_frames": warmup,
                "measured_frames": measured,
            },
            "reviewed": False,
            "frames": rows,
        }
        reference = output / "reference.json"
        atomic_json(reference, corpus)
        page = [
            '<!doctype html><meta charset="utf-8"><title>Referências de ocupação</title>',
            "<style>body{background:#101925;color:#e9f0fa;font:17px system-ui;max-width:1400px;margin:30px auto;padding:20px}img{width:100%}figure{margin:30px 0}</style>",
            "<h1>Revisão visual do corpus</h1><p>Rascunho: edite os rótulos em reference.json. Incerto é excluído da pontuação, nunca tratado como livre. Os polígonos apenas identificam vagas; a avaliação usa os frames originais do vídeo, verificados por hash.</p>",
        ]
        for row in rows:
            page.append(
                f'<figure><figcaption>Frame {row["frame_index"]} · {row["frame_index"] / fps:.2f} s</figcaption><img loading="lazy" src="{html.escape(row["preview"], quote=True)}"></figure>'
            )
        (output / "index.html").write_text("\n".join(page))
        return reference
    finally:
        stream.close()


def load_corpus(path):
    value = _fields(
        _read(Path(path), 8 * 1024 * 1024),
        (
            "schema_version",
            "video",
            "input_id",
            "mapping_sha256",
            "frame_size",
            "fps",
            "spot_ids",
            "sampling",
            "reviewed",
            "frames",
        ),
        ("decoder", "review_notes"),
    )
    if (
        type(value["schema_version"]) is not int
        or value["schema_version"] != 1
        or type(value["reviewed"]) is not bool
    ):
        raise ValueError("invalid reference version or review flag")
    _id(value["input_id"])
    _hash(value["mapping_sha256"])
    source = _fields(value["video"], ("sha256", "bytes"))
    _hash(source["sha256"])
    if type(source["bytes"]) is not int or source["bytes"] < 1:
        raise ValueError("invalid source artifact")
    fps = value["fps"]
    if type(fps) not in (int, float) or not math.isfinite(fps) or not 0 < fps <= 240:
        raise ValueError("invalid reference cadence")
    ids = value["spot_ids"]
    if (
        not isinstance(ids, list)
        or not 1 <= len(ids) <= 10000
        or any(not isinstance(key, str) for key in ids)
        or len(set(ids)) != len(ids)
    ):
        raise ValueError("invalid reference spot IDs")
    for key in ids:
        _id(key)
    sampling = _fields(
        value["sampling"], ("stride", "warmup_frames", "measured_frames")
    )
    if (
        any(type(sampling[k]) is not int for k in sampling)
        or not 1 <= sampling["stride"] <= 1000
        or not 0 <= sampling["warmup_frames"] <= 100
        or not 1 <= sampling["measured_frames"] <= 1000
    ):
        raise ValueError("invalid reference sampling")
    frames = value["frames"]
    if (
        not isinstance(frames, list)
        or not 1 <= len(frames) <= 120
        or len(frames) * len(ids) > 50000
    ):
        raise ValueError("invalid reference frame budget")
    indexes = []
    for frame in frames:
        _fields(frame, ("frame_index", "frame_sha256", "labels"), ("preview", "notes"))
        index = frame["frame_index"]
        _hash(frame["frame_sha256"])
        if (
            type(index) is not int
            or index % sampling["stride"]
            or not sampling["warmup_frames"]
            <= index // sampling["stride"]
            < sampling["warmup_frames"] + sampling["measured_frames"]
        ):
            raise ValueError("reference frame outside sample schedule")
        labels = frame["labels"]
        if (
            not isinstance(labels, dict)
            or labels.keys() != set(ids)
            or any(type(v) is not str or v not in LABELS for v in labels.values())
        ):
            raise ValueError("missing or invalid reference label")
        indexes.append(index)
    if indexes != sorted(set(indexes)):
        raise ValueError("duplicate or unordered reference frames")
    return value
