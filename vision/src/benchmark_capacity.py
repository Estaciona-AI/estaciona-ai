# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Measured runtime capacity, isolated state and explicit incomplete outcomes."""

from dataclasses import replace
import math
import multiprocessing
import os
import threading
import time
from urllib.parse import urlsplit

from benchmark_profiles import JsonLines, artifact, provenance, results_directory
from benchmark_resources import (
    gpu_sample,
    process_sample,
    summarize_resources,
    cgroup_sample,
)
from configuration import _read
from metrics import atomic_json
from runtime import Supervisor


def capacity_summary(inputs, snapshots, *, restarts, completed):
    results = []
    for item in inputs:
        snapshot = snapshots.get(item.input_id, {})
        seconds = snapshot.get("measured_seconds", 0)
        fps = (
            snapshot.get("counters", {}).get("processed", 0) / seconds
            if seconds > 0
            else 0
        )
        age = snapshot.get("stages", {}).get("frame_age_ms", {}).get("p95_upper")
        pending = snapshot.get("state", {}).get("pending")
        delivery = snapshot.get("state", {}).get("delivery")
        valid = (
            fps >= item.fps
            and age is not None
            and age <= 2000
            and (delivery == "shadow" or pending == 0)
        )
        results.append(
            {
                "input_id": item.input_id,
                "target_fps": item.fps,
                "effective_fps": fps,
                "frame_age_p95_upper_ms": age,
                "pending": pending,
                "target_met": valid,
            }
        )
    return {
        "inputs": results,
        "minimum_input_fps": min(row["effective_fps"] for row in results),
        "vision_target_met": completed
        and restarts == 0
        and all(row["target_met"] for row in results),
        "worker_restarts": restarts,
        "completed": completed,
        "full_pc_validated": False,
        "unvalidated": [
            "remote_backend_persistence",
            "gateway_and_publisher_resource_budget",
            "accuracy_ground_truth",
        ],
    }


def capacity_trial(
    config, output, *, count, seconds, warmup, delivery="shadow", stop=None
):
    if (
        type(count) is not int
        or not 1 <= count <= len(config.inputs)
        or not math.isfinite(seconds)
        or not 1 <= seconds <= 28800
        or not math.isfinite(warmup)
        or not 0 <= warmup <= 600
        or delivery not in ("shadow", "gateway")
    ):
        raise ValueError("invalid capacity limits")
    if delivery == "gateway" and urlsplit(config.gateway_url).hostname not in (
        "127.0.0.1",
        "localhost",
        "::1",
    ):
        raise ValueError("capacity delivery requires an explicitly local gateway")
    stop = stop or threading.Event()
    trial_stop = threading.Event()
    if config.environment == "production":
        raise ValueError("benchmark requires demo/test installation")
    if count > len(config.inputs):
        raise ValueError("not enough configured inputs")
    output = results_directory(output)
    directory = output / "metrics"
    directory.mkdir(mode=0o700)
    cfg = replace(config, inputs=config.inputs[:count], state_dir=output / "state")
    for item in cfg.inputs:
        item.source.resolve()
    context = multiprocessing.get_context("spawn")
    start, end = context.RawValue("d", math.inf), context.RawValue("d", math.inf)
    cgroup_before = cgroup_sample()
    initialized = time.monotonic()
    last_sample = 0.0
    complete = False
    reason = "startup"
    snapshots = {}
    model_artifacts = {
        path: artifact(path) for path in {item.detector.path for item in cfg.inputs}
    }
    with JsonLines(
        output / "resources.jsonl", max_bytes=256 * 1024 * 1024
    ) as resources:

        def observe(supervisor):
            nonlocal last_sample, complete, reason
            now = time.monotonic()
            if stop.is_set():
                reason = "cancelled"
                trial_stop.set()
                return
            health = [slot.snapshot() for slot in supervisor.slots]
            if any(
                slot.disabled or slot.starts > 1 or slot.failures
                for slot in supervisor.slots
            ):
                reason = "worker_failed_or_restarted"
                trial_stop.set()
                return
            if math.isinf(start.value):
                if all(value is not None and value[2] > 0 for value in health):
                    start.value = now + warmup
                    end.value = start.value + seconds
                    reason = "warming_up"
                elif now - initialized > cfg.startup_timeout:
                    reason = "startup_timeout"
                    trial_stop.set()
                    return
            if now >= end.value:
                complete = True
                reason = "measurement_complete"
                trial_stop.set()
            if now - last_sample >= 1 or complete:
                last_sample = now
                resources.write(
                    {
                        "at_monotonic": now,
                        "phase": "measuring" if now >= start.value else reason,
                        "supervisor": process_sample(os.getpid()),
                        "workers": [
                            process_sample(slot.process.pid)
                            for slot in supervisor.slots
                            if slot.process is not None
                        ],
                        "gpu": gpu_sample(),
                        "cgroup_memory": cgroup_sample(),
                    }
                )

        supervisor = Supervisor(
            cfg,
            measurement=(directory, start, end, delivery == "gateway"),
            observer=observe,
        )
        try:
            result = supervisor.run(trial_stop)
            if result:
                complete = False
                reason = "worker_failed"
        except OSError, BufferError:
            complete = False
            reason = "measurement_io_failed"
        finally:
            supervisor.close()
        for item in cfg.inputs:
            path = directory / f"{item.input_id}.json"
            if path.exists():
                snapshots[item.input_id] = _read(path, 2 * 1024 * 1024)
        restarts = sum(max(0, slot.starts - 1) for slot in supervisor.slots)
    if any(artifact(path) != value for path, value in model_artifacts.items()):
        complete = False
        reason = "model_artifact_changed"
    report = {
        **capacity_summary(
            cfg.inputs, snapshots, restarts=restarts, completed=complete
        ),
        "schema_version": 1,
        "kind": "capacity_trial",
        "resources": summarize_resources(output / "resources.jsonl"),
        "reason": reason,
        "delivery": delivery,
        "input_count": count,
        "spot_count": sum(len(item.mapping.spots) for item in cfg.inputs),
        "seconds_requested": seconds,
        "warmup_seconds": warmup,
        "provenance": provenance(),
        "inspector_enabled": False,
        "cgroup_before": cgroup_before,
        "cgroup_after": cgroup_sample(),
        "worker_outcomes": [
            {
                "input_id": slot.item.input_id,
                "starts": slot.starts,
                "exit_code": slot.last_exit_code,
            }
            for slot in supervisor.slots
        ],
        "backend": "remote_unmeasured" if delivery == "gateway" else "not_connected",
        "scope": "vision_only"
        if delivery == "shadow"
        else "vision_to_local_gateway_ack",
        "models": [
            {
                "input_id": item.input_id,
                "device_requested": item.detector.device,
                "artifact": model_artifacts[item.detector.path],
                "model": snapshots.get(item.input_id, {}).get("state", {}).get("model"),
            }
            for item in cfg.inputs
        ],
    }
    atomic_json(output / "capacity.json", report)
    return report


def can_advance(report, next_count):
    if next_count <= 20:
        return True
    if not report["vision_target_met"] or report["resources"]["samples"] < 2:
        return False
    if any(model["device_requested"] == "cuda" for model in report["models"]):
        total = report["resources"].get("gpu_total_mib")
        peak = report["resources"].get("gpu_peak_used_mib")
        return total is not None and peak is not None and total - peak >= 1024
    return True
