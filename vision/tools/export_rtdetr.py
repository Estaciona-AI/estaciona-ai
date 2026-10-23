# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Build official Apache RT-DETR artifacts without importing Ultralytics.

Run from vision: uv run --project export python tools/export_rtdetr.py [--variant r18]
The Python 3.11 exporter stays separate from the Python 3.14 runtime.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from urllib.request import urlopen

REPOSITORY = "https://github.com/lyuwenyu/RT-DETR.git"
REVISION = "df2a96c8c3a098567a1149bf0248da6efe6d495d"
WEIGHTS_BASE = "https://bj.bcebos.com/v1/paddledet/models/"
# Upstream publishes no checksums. Each SHA-256 pins the weights first
# downloaded for this project (L: original export; R18: 2026-10-22).
VARIANTS = {
    "l": {
        "model": "RT-DETR-HGNetv2-L",
        "config": "rtdetr_hgnetv2_l_6x_coco",
        "weights": "rtdetr_hgnetv2_l_6x_coco.pdparams",
        "sha256": "3bf9283e00183818e53840ea477f3c36f9accb7b5eb0f62a3ece4bee94cdb493",
        "output": "rtdetr-hgnetv2-l.onnx",
    },
    "r18": {
        "model": "RT-DETR-R18",
        "config": "rtdetr_r18vd_6x_coco",
        "weights": "rtdetr_r18vd_dec3_6x_coco.pdparams",
        "sha256": "c93a3b6276d071a0330917e523866ab72d5d40ab8e4c0e49d0efd46c464322d1",
        "output": "rtdetr-r18.onnx",
    },
}


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(command, **kwargs):
    subprocess.run(command, check=True, **kwargs)


def export(output, variant="l"):
    if variant not in VARIANTS:
        raise ValueError(f"Unknown RT-DETR variant: {variant}")
    spec = VARIANTS[variant]
    weights_url = WEIGHTS_BASE + spec["weights"]
    output = Path(output).resolve()
    if output.suffix != ".onnx":
        raise ValueError("Output must have the .onnx extension")
    if any(
        path.exists()
        for path in (
            output,
            output.with_suffix(".provenance.json"),
            output.with_suffix(".LICENSE"),
        )
    ):
        raise FileExistsError("Refusing to overwrite an existing model or provenance")
    with tempfile.TemporaryDirectory(prefix="estaciona-rtdetr-") as directory:
        root = Path(directory)
        repo = root / "upstream"
        run(["git", "init", "--quiet", str(repo)])
        run(
            [
                "git",
                "-C",
                str(repo),
                "fetch",
                "--quiet",
                "--depth=1",
                REPOSITORY,
                REVISION,
            ]
        )
        run(["git", "-C", str(repo), "checkout", "--quiet", "--detach", "FETCH_HEAD"])
        weights = root / "weights.pdparams"
        with (
            urlopen(weights_url, timeout=120) as response,
            weights.open("wb") as target,
        ):
            shutil.copyfileobj(response, target)
        if digest(weights) != spec["sha256"]:
            raise ValueError("Official weight checksum mismatch; do not export")
        upstream = repo / "rtdetr_paddle"
        inference = root / "inference"
        environment = {
            **os.environ,
            "PADDLE_HOME": str(root / "paddle-cache"),
            "OMP_NUM_THREADS": "1",
        }
        run(
            [
                sys.executable,
                "tools/export_model.py",
                "-c",
                f"configs/rtdetr/{spec['config']}.yml",
                "-o",
                f"weights={weights}",
                "trt=True",
                "use_gpu=False",
                f"--output_dir={inference}",
            ],
            cwd=upstream,
            env=environment,
        )
        onnx_path = root / "model.onnx"
        run(
            [
                str(Path(sys.executable).with_name("paddle2onnx")),
                f"--model_dir={inference / spec['config']}",
                "--model_filename=model.pdmodel",
                "--params_filename=model.pdiparams",
                "--opset_version=16",
                f"--save_file={onnx_path}",
            ],
            env=environment,
        )
        import onnx

        onnx.checker.check_model(str(onnx_path))
        provenance = {
            "model": spec["model"],
            "license": "Apache-2.0",
            "source_repository": REPOSITORY,
            "source_revision": REVISION,
            "source_license": f"https://github.com/lyuwenyu/RT-DETR/blob/{REVISION}/LICENSE",
            "weights_url": weights_url,
            "weights_sha256": spec["sha256"],
            "onnx_sha256": digest(onnx_path),
            "input_size": [640, 640],
            "label_space": "COCO-80-dense",
            "export_opset": 16,
            "export_runtime": "Paddle 2.6.2 / Paddle2ONNX 1.0.8",
        }
        output.parent.mkdir(parents=True, exist_ok=True)
        # Exclusive creation prevents overwriting an operator-provided artifact.
        with output.open("xb") as target, onnx_path.open("rb") as source:
            shutil.copyfileobj(source, target)
        with output.with_suffix(".provenance.json").open("x") as target:
            target.write(json.dumps(provenance, indent=2) + "\n")
        with output.with_suffix(".LICENSE").open("xb") as target:
            target.write((repo / "LICENSE").read_bytes())
        print(f"Exported {output}; SHA-256 {provenance['onnx_sha256']}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", choices=sorted(VARIANTS), default="l")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    models = Path(__file__).resolve().parents[1] / "models"
    export(args.output or models / VARIANTS[args.variant]["output"], args.variant)
