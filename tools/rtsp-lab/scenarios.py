# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Explicit synthetic spot assignments, compatible with the production runner."""

import os
from pathlib import Path
import tempfile
from urllib.parse import urlsplit
from uuid import uuid4

from configuration import load_configuration
from services import IMAGE, probe_video
from artifacts import LabError, checksum, read_json, write_json


def generate(
    output,
    *,
    video,
    model,
    spots,
    inputs=20,
    total=400,
    device="cuda",
    gateway_url="ws://127.0.0.1:18001/ws/edge",
):
    output = Path(output).resolve()
    video, model, spots = (Path(path).resolve() for path in (video, model, spots))
    if output.exists() or output.is_symlink():
        raise LabError("output already exists; choose a new scenario directory")
    if (
        type(inputs) is not int
        or not 1 <= inputs <= 35
        or type(total) is not int
        or not inputs <= total <= 10000
    ):
        raise LabError("invalid input or spot count")
    try:
        gateway = urlsplit(gateway_url)
        local = (
            gateway.scheme == "ws"
            and gateway.hostname in ("127.0.0.1", "localhost", "::1")
            and gateway.username is None
            and not gateway.query
            and not gateway.fragment
            and gateway.port != 0
        )
    except ValueError, TypeError:
        local = False
    if not local:
        raise LabError("lab gateway must be explicitly local, without URL credentials")
    metadata = probe_video(video)
    original = read_json(spots)
    if (
        not isinstance(original, dict)
        or not original
        or any(not isinstance(key, str) for key in original)
    ):
        raise LabError("expected original spot ID to polygon mapping")
    ids = sorted(original)
    if (total + inputs - 1) // inputs > len(ids):
        raise LabError("not enough distinct source polygons per input")
    if not model.is_file() or model.suffix.lower() != ".onnx":
        raise LabError("explicit local ONNX model is required")
    run_id = uuid4().hex
    descriptor = {
        "schema_version": 1,
        "organization": {
            "id": str(uuid4()),
            "slug": "synthetic-lab",
            "name": "Synthetic laboratory",
        },
        "installation": {
            "id": str(uuid4()),
            "slug": "rtsp-demo",
            "name": "RTSP simulation",
        },
        "environment": "demo",
    }
    profile = {
        "version": 1,
        "installation_manifest": "installation.json",
        "state_dir": "state",
        "gateway_url": gateway_url,
        "credential_env": "ESTACIONA_LAB_EDGE_KEY",
        "detectors": {
            "main": {"adapter": "rtdetr", "path": str(model), "device": device}
        },
        "sources": {"shared-video": {"kind": "rtsp", "env": "ESTACIONA_LAB_RTSP"}},
        "inputs": [],
    }
    assignments = []
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=".rtsp-build-", dir=output.parent
    ) as staging:
        directory = Path(staging)
        (directory / "mappings").mkdir()
        spot_index = 0
        for index in range(inputs):
            count = total // inputs + (index < total % inputs)
            camera = f"sim-cam-{index + 1:03}"
            mapping = {}
            for offset in range(count):
                source_id = ids[(index + offset) % len(ids)]
                spot_index += 1
                destination = f"sim-spot-{spot_index:04}"
                mapping[destination] = original[source_id]
                assignments.append(
                    {
                        "input_id": camera,
                        "source_spot_id": source_id,
                        "spot_id": destination,
                    }
                )
            name = f"mappings/{camera}.json"
            write_json(
                directory / name,
                {
                    "width": metadata["width"],
                    "height": metadata["height"],
                    "spots": mapping,
                },
            )
            profile["inputs"].append(
                {
                    "input_id": camera,
                    "source": "shared-video",
                    "detector": "main",
                    "mapping": name,
                    "fps": 2.5,
                }
            )
        write_json(directory / "installation.json", descriptor)
        write_json(directory / "runtime.json", profile)
        load_configuration(directory / "runtime.json")
        write_json(
            directory / "scenario.json",
            {
                "version": 1,
                "run_id": run_id,
                "simulation": True,
                "inputs": inputs,
                "total_spots": total,
                "video": str(video),
                "video_sha256": checksum(video),
                "source_spots": str(spots),
                "spots_sha256": checksum(spots),
                "model_sha256": checksum(model),
                "media": metadata,
                "image": IMAGE,
                "assignments": assignments,
            },
        )
        # No user files are overwritten; generated assets move together.
        os.rename(directory, output)
    return output
