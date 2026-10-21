# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Linux process samples and device-wide GPU context, with no extra dependency."""

import os
from pathlib import Path
import subprocess


def process_sample(pid):
    try:
        # /proc comm may contain spaces and parentheses; split after its last ')'.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {
            "pid": pid,
            "cpu_seconds": (int(fields[11]) + int(fields[12]))
            / os.sysconf("SC_CLK_TCK"),
            "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
            "start_ticks": int(fields[19]),
        }
    except OSError, ValueError, IndexError:
        return {"pid": pid, "unavailable": True}


def gpu_sample():
    try:
        result = subprocess.run(
            [
                "nvidia-smi",
                "--query-gpu=name,driver_version,memory.total,memory.used,utilization.gpu",
                "--format=csv,noheader,nounits",
            ],
            capture_output=True,
            text=True,
            check=True,
            timeout=2,
        )
        devices = []
        for row in result.stdout.splitlines()[:8]:
            name, driver, total, used, utilization = (
                value.strip() for value in row.split(",")
            )
            devices.append(
                {
                    "name": name,
                    "driver": driver,
                    "total_mib": int(total),
                    "used_mib": int(used),
                    "utilization_percent": int(utilization),
                }
            )
        return {
            "scope": "whole_device_including_unrelated_processes",
            "devices": devices,
        }
    except OSError, subprocess.SubprocessError, ValueError:
        return {"scope": "whole_device", "unavailable": True}


def summarize_resources(path):
    import json

    previous = {}
    peak_rss = 0
    all_phases_peak_rss = 0
    all_phases_gpu_peak = None
    peak_cpu = None
    gpu_peak = None
    gpu_devices = []
    samples = 0
    with Path(path).open() as stream:
        for line in stream:
            row = json.loads(line)
            every_process = row.get(
                "processes", row.get("workers", []) + [row.get("supervisor", {})]
            )
            all_phases_peak_rss = max(
                all_phases_peak_rss,
                sum(item.get("rss_bytes", 0) for item in every_process),
            )
            every_gpu = row.get("gpu", {}).get("devices", [])
            if every_gpu:
                total_used = sum(item["used_mib"] for item in every_gpu)
                all_phases_gpu_peak = (
                    total_used
                    if all_phases_gpu_peak is None
                    else max(all_phases_gpu_peak, total_used)
                )
            if row.get("phase") not in (None, "measuring"):
                continue
            samples += 1
            now = row["at_monotonic"]
            processes = row.get(
                "processes", row.get("workers", []) + [row.get("supervisor", {})]
            )
            rss = 0
            cpu = 0
            cpu_available = False
            for process in processes:
                if process.get("unavailable") or "start_ticks" not in process:
                    continue
                rss += process["rss_bytes"]
                key = (process["pid"], process["start_ticks"])
                if key in previous:
                    then, seconds = previous[key]
                    if now > then:
                        cpu += (
                            max(0, process["cpu_seconds"] - seconds)
                            / (now - then)
                            * 100
                        )
                        cpu_available = True
                previous[key] = (now, process["cpu_seconds"])
            peak_rss = max(peak_rss, rss)
            if cpu_available:
                peak_cpu = cpu if peak_cpu is None else max(peak_cpu, cpu)
            devices = row.get("gpu", {}).get("devices", [])
            if devices:
                gpu_devices = [
                    {"name": device["name"], "driver": device["driver"]}
                    for device in devices
                ]
                used = sum(device["used_mib"] for device in devices)
                gpu_peak = used if gpu_peak is None else max(gpu_peak, used)
    return {
        "samples": samples,
        "owned_peak_rss_bytes": peak_rss,
        "rss_scope": "sum_of_process_rss_may_double_count_shared_pages",
        "all_phases_peak_rss_bytes": all_phases_peak_rss,
        "all_phases_gpu_peak_used_mib": all_phases_gpu_peak,
        "owned_peak_cpu_percent": peak_cpu,
        "cpu_percent_units": "100_percent_per_logical_cpu",
        "gpu_peak_used_mib": gpu_peak,
        "gpu_scope": "whole_device_including_unrelated_processes",
        "gpu_devices": gpu_devices,
        "sampling": "periodic_not_instantaneous_peak",
        "external_services_measured": False,
    }


def cgroup_sample(root="/sys/fs/cgroup"):
    root = Path(root)
    try:
        current = int((root / "memory.current").read_text())
        peak = int((root / "memory.peak").read_text())
        limit = (root / "memory.max").read_text().strip()
        events = dict(
            line.split() for line in (root / "memory.events").read_text().splitlines()
        )
        return {
            "scope": "current_cgroup_may_include_additional_processes",
            "current_bytes": current,
            "peak_bytes": peak,
            "limit_bytes": None if limit == "max" else int(limit),
            "oom_events": int(events.get("oom", 0)),
            "oom_kills": int(events.get("oom_kill", 0)),
        }
    except OSError, ValueError:
        return {"scope": "current_cgroup", "unavailable": True}
