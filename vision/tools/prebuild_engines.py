# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Build every TensorRT engine a runtime.json needs before workers start.

Run from vision on the target host and GPU:
    uv run --extra tensorrt python tools/prebuild_engines.py runtime.json

Engines are specific to GPU, driver and TensorRT version, so build them where
they will run. Profiles sharing a model, precision, cache and tile count share
one engine and are built once; a second run finds the cache warm and returns
quickly. Exit status is non-zero when any engine could not be built.
"""

import argparse
import json
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import numpy as np  # noqa: E402

from configuration import load_configuration  # noqa: E402
from detector import load_detector  # noqa: E402
from tiling import spot_windows, windows  # noqa: E402


def _tile_count(profile, mapping):
    """Mirror load_detector: TensorRT batches every crop in a single call."""
    if not profile.tile_size:
        return 1
    size = (mapping.width, mapping.height)
    if profile.tile_scope == "spots":
        return len(
            spot_windows(
                *size, profile.tile_size, profile.tile_overlap, mapping.spots.values()
            )
        )
    return len(windows(*size, profile.tile_size, profile.tile_overlap))


def _engines(cache):
    return {path.name for path in Path(cache).glob("*.engine")}


def prebuild(inputs, *, loader=load_detector):
    """Build one engine per distinct profile; return one result per engine."""
    groups = {}
    skipped = []
    for item in inputs:
        profile = item.detector
        if profile.provider != "tensorrt":
            skipped.append(item.input_id)
            continue
        key = (
            str(profile.path),
            profile.precision,
            str(profile.engine_cache),
            _tile_count(profile, item.mapping),
        )
        groups.setdefault(key, []).append(item)
    results = []
    for (model, precision, cache, tiles), items in groups.items():
        item = items[0]
        profile = item.detector
        before = _engines(cache)
        started = time.monotonic()
        result = {
            "model": model,
            "precision": precision,
            "engine_cache": cache,
            "tile_count": tiles,
            "inputs": [member.input_id for member in items],
        }
        try:
            detector = loader(
                profile.path,
                profile.device,
                tile_size=profile.tile_size,
                tile_overlap=profile.tile_overlap,
                tile_scope=profile.tile_scope,
                mapping=item.mapping,
                provider=profile.provider,
                precision=profile.precision,
                engine_cache=profile.engine_cache,
            )
            # The engine is built or loaded for the exact batch on first use.
            detector.detect(
                np.zeros((item.mapping.height, item.mapping.width, 3), np.uint8),
                confidence=profile.confidence,
            )
            result["batch"] = detector.metadata().get("batch")
            result["status"] = "built" if _engines(cache) - before else "cached"
        except Exception as error:  # report every failing engine, then fail
            result["status"] = "failed"
            result["error"] = f"{type(error).__name__}: {error}"
        result["seconds"] = round(time.monotonic() - started, 1)
        results.append(result)
    return results, skipped


def main(argv=None, *, configuration=load_configuration, loader=load_detector):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("config", type=Path, help="runtime.json to prepare")
    parser.add_argument("--report", type=Path, help="write the results as JSON")
    args = parser.parse_args(argv)
    results, skipped = prebuild(configuration(args.config).inputs, loader=loader)
    for result in results:
        detail = result.get("error", "")
        print(
            f"{result['status']:6} {result['tile_count']:>2} tiles "
            f"{result['precision']} {result['seconds']:>6}s "
            f"{len(result['inputs'])} input(s) {detail}".rstrip(),
            flush=True,
        )
    if skipped:
        print(f"skipped {len(skipped)} input(s) without the tensorrt provider")
    if not results:
        print("no TensorRT profile in this configuration", file=sys.stderr)
    if args.report:
        args.report.write_text(
            json.dumps({"engines": results, "skipped": skipped}, indent=2)
        )
    failed = [result for result in results if result["status"] == "failed"]
    return 1 if failed or not results else 0


if __name__ == "__main__":
    sys.exit(main())
