# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
import os

import pytest

from benchmark_resources import process_sample, summarize_resources


def test_process_sample_reports_live_cpu_and_memory_without_commandline():
    row = process_sample(os.getpid())
    assert row["rss_bytes"] > 0 and row["cpu_seconds"] >= 0
    assert set(row) == {"pid", "cpu_seconds", "rss_bytes", "start_ticks"}
    assert process_sample(999999999)["unavailable"]


def test_summary_distinguishes_device_wide_gpu_from_owned_process_memory(tmp_path):
    path = tmp_path / "resources.jsonl"
    rows = []
    for t, cpu, memory in ((1, 1, 100), (2, 1.5, 150)):
        rows.append(
            {
                "phase": "measuring",
                "at_monotonic": t,
                "supervisor": {
                    "pid": 1,
                    "start_ticks": 10,
                    "cpu_seconds": 0,
                    "rss_bytes": 10,
                },
                "workers": [
                    {
                        "pid": 2,
                        "start_ticks": 20,
                        "cpu_seconds": cpu,
                        "rss_bytes": memory,
                    }
                ],
                "gpu": {
                    "scope": "whole_device_including_unrelated_processes",
                    "devices": [
                        {"name": "fixture", "driver": "fixture", "used_mib": 200}
                    ],
                },
            }
        )
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    result = summarize_resources(path)
    assert result["owned_peak_rss_bytes"] == 160
    assert result["owned_peak_cpu_percent"] == pytest.approx(50)
    assert result["gpu_peak_used_mib"] == 200
    assert result["gpu_scope"] == "whole_device_including_unrelated_processes"


def test_cgroup_memory_reports_limit_and_oom_without_confusing_rss(tmp_path):
    from benchmark_resources import cgroup_sample

    for name, value in {
        "memory.current": "1024",
        "memory.peak": "2048",
        "memory.max": "8192",
        "memory.events": "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n",
    }.items():
        (tmp_path / name).write_text(value)
    row = cgroup_sample(tmp_path)
    assert row["current_bytes"] == 1024 and row["peak_bytes"] == 2048
    assert row["limit_bytes"] == 8192 and row["oom_kills"] == 1
    (tmp_path / "memory.max").write_text("max")
    assert cgroup_sample(tmp_path)["limit_bytes"] is None
