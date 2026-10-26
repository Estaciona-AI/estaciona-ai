# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Sequential/simultaneous, shadow-only comparisons on identical local frames."""

import hashlib
import json
import math
import multiprocessing
import os
from pathlib import Path
from types import SimpleNamespace
import threading
import time

import cv2

from benchmark_profiles import (
    JsonLines,
    artifact,
    digest_json,
    detector_parameters,
    provenance,
    results_directory,
)
from benchmark_resources import gpu_sample, process_sample, summarize_resources
from detector import load_detector
from metrics import Metrics, atomic_json
from pipeline import VisionPipeline


def comparison_pass(
    profile, mapping, frames, output, *, model=None, warmup=3, fps=30, barrier=None
):
    model_artifact = artifact(profile.path)
    model = model or load_detector(
        profile.path,
        device=profile.device,
        tile_size=profile.tile_size,
        tile_overlap=profile.tile_overlap,
        tile_scope=profile.tile_scope,
        provider=profile.provider,
        precision=profile.precision,
        engine_cache=profile.engine_cache,
        expected_sha256=profile.sha256,
        mapping=mapping,
    )
    if barrier is not None:
        barrier.wait(timeout=120)
    start = SimpleNamespace(value=math.inf)
    end = SimpleNamespace(value=math.inf)
    metrics = Metrics(control=start, end=end)
    model.metrics = metrics
    pipeline = VisionPipeline(
        model,
        mapping.spots,
        (mapping.height, mapping.width, 3),
        metrics=metrics,
        confidence=profile.confidence,
        gamma=profile.gamma,
    )
    processed = 0
    with JsonLines(output / f"{profile.profile_id}.frames.jsonl") as rows:
        for ordinal, (index, frame) in enumerate(frames):
            if frame.shape != (mapping.height, mapping.width, 3):
                raise ValueError("comparison frame does not match mapping")
            if ordinal == warmup:
                if barrier is not None:
                    barrier.wait(timeout=120)
                start.value = time.monotonic()
            started = time.perf_counter()
            result = pipeline.process(frame, now=index / fps)
            if ordinal < warmup:
                continue
            metrics.observe("pipeline_ms", (time.perf_counter() - started) * 1000)
            metrics.increment("processed")
            rows.write(
                {
                    "frame_index": index,
                    "frame_sha256": hashlib.sha256(frame.tobytes()).hexdigest(),
                    "detections": result.detections,
                    "spots": [
                        {"id": spot.spot_id, "status": spot.status, "ratio": spot.ratio}
                        for spot in result.spots
                    ],
                }
            )
            processed += 1
    end.value = time.monotonic()
    if not processed:
        raise ValueError("no measured frames after warmup")
    if artifact(profile.path) != model_artifact:
        raise ValueError("model changed during comparison")
    atomic_json(
        output / f"{profile.profile_id}.json",
        {
            "schema_version": 2,
            "profile_id": profile.profile_id,
            "delivery": "shadow",
            "frames": processed,
            "artifact": model_artifact,
            "device_requested": profile.device,
            "parameters": detector_parameters(profile),
            "model": model.metadata(),
            "metrics": metrics.snapshot(),
            "process": process_sample(os.getpid()),
        },
    )


def frame_indexes(path):
    indexes = {}
    with Path(path).open() as stream:
        for line in stream:
            row = json.loads(line)
            if row["frame_index"] in indexes:
                raise ValueError("duplicate comparison frame")
            indexes[row["frame_index"]] = row["frame_sha256"]
    return indexes


def video_frames(video, *, samples, stride):
    capture = cv2.VideoCapture(str(video))
    try:
        if not capture.isOpened():
            raise ValueError("cannot decode local comparison video")
        fps = capture.get(cv2.CAP_PROP_FPS)
        if not math.isfinite(fps) or not 0 < fps <= 240:
            raise ValueError("invalid comparison video cadence")
        yield fps
        selected = index = 0
        while selected < samples:
            ok, frame = capture.read()
            if not ok:
                return
            if index % stride == 0:
                frame.setflags(write=False)
                yield index, frame
                selected += 1
            index += 1
    finally:
        capture.release()


def _comparison_entry(profile, mapping, video, output, count, warmup, stride, barrier):
    # Native model/decoder errors must not leak local paths or future credentials.
    with open(os.devnull, "wb") as sink:
        os.dup2(sink.fileno(), 1)
        os.dup2(sink.fileno(), 2)
    try:
        frames = video_frames(video, samples=count + warmup, stride=stride)
        fps = next(frames)
        comparison_pass(
            profile, mapping, frames, output, warmup=warmup, fps=fps, barrier=barrier
        )
    except Exception:
        atomic_json(
            output / f"{profile.profile_id}.error.json",
            {"error": "comparison_failed", "profile_id": profile.profile_id},
        )
        raise SystemExit(1) from None


def _reap(process):
    if process.is_alive():
        process.terminate()
        process.join(1)
        if process.is_alive():
            process.kill()
            process.join(2)
        if process.is_alive():
            raise RuntimeError("benchmark child could not be stopped")
    process.join()
    process.close()


def compare(
    config,
    profiles,
    video,
    output,
    *,
    input_id,
    mode="sequential",
    frames=30,
    warmup=3,
    stride=12,
    stop=None,
    timeout=300,
):
    stop = stop or threading.Event()
    if config.environment == "production":
        raise ValueError("comparison requires a demo/test installation")
    item = next((item for item in config.inputs if item.input_id == input_id), None)
    if item is None or not Path(video).is_file():
        raise ValueError("unknown input or missing local video")
    video_artifact = artifact(video)
    output = results_directory(output)
    context = multiprocessing.get_context("spawn")
    barrier = context.Barrier(len(profiles)) if mode == "simultaneous" else None
    batches = [profiles] if barrier else [(profile,) for profile in profiles]
    succeeded = True
    with JsonLines(output / "resources.jsonl") as resources:
        for batch in batches:
            children = []
            try:
                for profile in batch:
                    process = context.Process(
                        target=_comparison_entry,
                        args=(
                            profile,
                            item.mapping,
                            Path(video),
                            output,
                            frames,
                            warmup,
                            stride,
                            barrier,
                        ),
                    )
                    try:
                        process.start()
                    except BaseException:
                        process.close()
                        raise
                    children.append(process)
                deadline = time.monotonic() + timeout
                while any(child.is_alive() for child in children):
                    resources.write(
                        {
                            "at_monotonic": time.monotonic(),
                            "processes": [
                                process_sample(child.pid) for child in children
                            ],
                            "gpu": gpu_sample(),
                        }
                    )
                    if stop.is_set() or time.monotonic() >= deadline:
                        succeeded = False
                        break
                    stop.wait(1)
                succeeded = succeeded and all(child.exitcode == 0 for child in children)
            finally:
                for child in children:
                    _reap(child)
            if not succeeded:
                break
    reference = None
    same_frames = True
    windows = []
    for profile in profiles:
        path = output / f"{profile.profile_id}.frames.jsonl"
        result = output / f"{profile.profile_id}.json"
        try:
            indexes = frame_indexes(path)
            metadata = json.loads(result.read_text())
            windows.append(
                (
                    metadata["metrics"]["window_start_monotonic"],
                    metadata["metrics"]["window_end_monotonic"],
                )
            )
        except OSError, ValueError, KeyError:
            succeeded = False
            same_frames = False
            continue
        if reference is None:
            reference = indexes
        elif indexes != reference:
            same_frames = False
        if len(indexes) != frames:
            same_frames = False
    overlap = (
        max(
            0, min(end for start, end in windows) - max(start for start, end in windows)
        )
        if windows
        else 0
    )
    if mode == "simultaneous" and len(profiles) > 1 and overlap <= 0:
        succeeded = False
    succeeded = succeeded and same_frames and artifact(video) == video_artifact
    report = {
        "schema_version": 2,
        "installation": {
            "organization_id": config.organization_id,
            "installation_id": config.installation_id,
            "environment": config.environment,
        },
        "kind": "same_frame_comparison",
        "resources": summarize_resources(output / "resources.jsonl"),
        "mode": mode,
        "delivery": "shadow",
        "completed": succeeded,
        "same_frames_verified": bool(reference) and same_frames,
        "measured_overlap_seconds": overlap if mode == "simultaneous" else None,
        "profiles": [profile.profile_id for profile in profiles],
        "profile_parameters": {
            profile.profile_id: detector_parameters(profile) for profile in profiles
        },
        "video": video_artifact,
        "scope": {
            "organization_id": config.organization_id,
            "installation_id": config.installation_id,
            "environment": config.environment,
        },
        "mapping_sha256": digest_json(
            {
                "width": item.mapping.width,
                "height": item.mapping.height,
                "spots": item.mapping.spots,
            }
        ),
        "warmup_frames": warmup,
        "requested_frames": frames,
        "stride": stride,
        "provenance": provenance(),
        "accuracy": "not_evaluated_without_ground_truth",
        "full_pc_validated": False,
    }
    atomic_json(output / "comparison.json", report)
    return report
