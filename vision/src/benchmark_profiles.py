# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Explicit benchmark profiles and bounded artifacts; never resolves secret values."""

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import platform
import subprocess

from configuration import ConfigurationError, _fields, _id, _path, _read
from pipeline import BOX_SHRINK_FACTOR, CLAHE_ENABLED, CONF_THRESHOLD, GAMMA_CORRECTION


@dataclass(frozen=True)
class BenchmarkProfile:
    profile_id: str
    path: Path
    device: str


def load_profiles(path):
    path = Path(path).resolve()
    data = _fields(_read(path, 16384), ("version", "profiles"))
    if type(data["version"]) is not int or data["version"] != 1:
        raise ConfigurationError("unsupported benchmark version")
    values = data["profiles"]
    if not isinstance(values, dict) or not 1 <= len(values) <= 8:
        raise ConfigurationError("expected 1 to 8 explicit profiles")
    profiles = []
    for key, value in values.items():
        _id(key)
        _fields(value, ("adapter", "path", "device"))
        if value["adapter"] != "rtdetr" or value["device"] not in ("cpu", "cuda"):
            raise ConfigurationError("unsupported benchmark detector")
        model = _path(path.parent, value["path"])
        if model.suffix.lower() != ".onnx":
            raise ConfigurationError("expected local ONNX artifact")
        profiles.append(BenchmarkProfile(key, model, value["device"]))
    return tuple(profiles)


def artifact(path):
    path = Path(path)
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"sha256": digest, "bytes": path.stat().st_size}


def digest_json(value):
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def provenance():
    repo = Path(__file__).resolve().parents[2]

    def git(*args):
        try:
            return subprocess.check_output(
                ["git", "-C", str(repo), *args],
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=3,
            ).strip()
        except OSError, subprocess.SubprocessError:
            return None

    return {
        "commit": git("rev-parse", "HEAD"),
        "dirty": bool(git("status", "--porcelain")),
        "python": platform.python_version(),
        "timing_semantics": {
            "inference_ms": "blocking ONNX Runtime session.run including transfers",
            "decode_wait_ms": "decoder read including stream waiting",
            "frame_age_ms": "local decode completion to processing completion; camera clock not measured",
            "confirmation_debounce_ms": "first observed opposite candidate to confirmed transition",
            "confirmation_observation_to_enqueue_ms": "confirming frame decode completion to local enqueue; excludes prior debounce",
        },
        "platform": platform.system(),
        "machine": platform.machine(),
        "parameters": {
            "confidence": CONF_THRESHOLD,
            "box_shrink": BOX_SHRINK_FACTOR,
            "gamma": GAMMA_CORRECTION,
            "clahe": CLAHE_ENABLED,
        },
    }


class JsonLines:
    def __init__(self, path, *, max_bytes=64 * 1024 * 1024):
        self.path = Path(path)
        self.max_bytes = max_bytes
        self.size = 0

    def __enter__(self):
        self.stream = self.path.open("x")
        return self

    def write(self, value):
        line = json.dumps(value, allow_nan=False, separators=(",", ":")) + "\n"
        size = len(line.encode())
        if self.size + size > self.max_bytes:
            raise BufferError("benchmark artifact byte limit exceeded")
        self.stream.write(line)
        self.stream.flush()
        self.size += size

    def __exit__(self, *_):
        self.stream.close()


def results_directory(path):
    path = Path(path).resolve()
    if path.is_relative_to(Path(__file__).resolve().parents[2]):
        raise ValueError("benchmark artifacts must remain outside the source checkout")
    path.mkdir(mode=0o700)
    return path
