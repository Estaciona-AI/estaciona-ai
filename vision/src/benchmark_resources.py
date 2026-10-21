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
