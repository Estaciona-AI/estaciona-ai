# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Validated local profiles. Installation coordinates are not authorization."""

from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import re
from urllib.parse import urlsplit
from uuid import UUID


class ConfigurationError(ValueError):
    pass


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ConfigurationError("duplicate JSON key")
        result[key] = value
    return result


def _read(path, limit):
    try:
        with path.open("rb") as stream:
            data = stream.read(limit + 1)
        if len(data) > limit:
            raise ConfigurationError("configuration file too large")
        return json.loads(data, object_pairs_hook=_pairs)
    except OSError, UnicodeError, json.JSONDecodeError:
        raise ConfigurationError("cannot read configuration JSON") from None


def _fields(value, required, optional=()):
    if (
        not isinstance(value, dict)
        or not set(required) <= value.keys()
        or value.keys() - set(required) - set(optional)
    ):
        raise ConfigurationError("missing or unknown configuration fields")
    return value


def _id(value):
    if not isinstance(value, str) or not re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", value
    ):
        raise ConfigurationError("invalid input, profile or spot ID")
    return value


def _env(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Z_][A-Z0-9_]{0,127}", value):
        raise ConfigurationError("invalid environment reference")
    return value


def _number(value, low, high):
    if (
        type(value) not in (int, float)
        or not math.isfinite(value)
        or not low <= value <= high
    ):
        raise ConfigurationError("invalid numeric limit")
    return float(value)


def _path(base, value, *, file=True):
    if not isinstance(value, str) or not value or len(value) > 4096:
        raise ConfigurationError("invalid local path")
    result = (base / value).resolve()
    if file and not result.is_file():
        raise ConfigurationError("required local file is missing")
    return result


def _uuid(value):
    try:
        result = UUID(value)
        if result.int:
            return str(result)
    except ValueError, TypeError, AttributeError:
        pass
    raise ConfigurationError("invalid installation identity")


@dataclass(frozen=True)
class Source:
    kind: str
    path: Path | None = None
    env: str | None = None
    loop: bool = False

    def resolve(self):
        if self.kind == "file":
            return str(self.path)
        value = os.environ.get(self.env, "")
        try:
            url = urlsplit(value)
            valid = url.scheme in ("rtsp", "rtsps") and url.hostname and url.port != 0
        except ValueError:
            valid = False
        if not valid or len(value) > 4096 or any(ord(c) < 32 for c in value):
            raise ConfigurationError("invalid or missing source environment")
        return value


@dataclass(frozen=True)
class DetectorProfile:
    path: Path
    device: str
    confidence: float = 0.1
    gamma: float = 1.4
    tile_size: int = 0
    tile_overlap: float = 0.25
    tile_scope: str = "frame"


def parse_detector_profile(value, base):
    """One validation path for runtime and benchmark detector profiles."""
    _fields(
        value,
        ("adapter", "path", "device"),
        ("confidence", "gamma", "tile_size", "tile_overlap", "tile_scope"),
    )
    if value["adapter"] != "rtdetr" or value["device"] not in ("cpu", "cuda"):
        raise ConfigurationError("unsupported detector profile")
    model = _path(base, value["path"])
    if model.suffix.lower() != ".onnx":
        raise ConfigurationError("model must be ONNX")
    tile_size = value.get("tile_size", 0)
    if type(tile_size) is not int or (tile_size != 0 and not 256 <= tile_size <= 2048):
        raise ConfigurationError("invalid detector tile size")
    tile_scope = value.get("tile_scope", "frame")
    if tile_scope not in ("frame", "spots") or (
        tile_scope == "spots" and not tile_size
    ):
        raise ConfigurationError("invalid detector tile scope")
    return DetectorProfile(
        model,
        value["device"],
        _number(value.get("confidence", 0.1), 0.01, 1),
        _number(value.get("gamma", 1.4), 0.1, 4),
        tile_size,
        _number(value.get("tile_overlap", 0.25), 0.1, 0.5),
        tile_scope,
    )


@dataclass(frozen=True)
class Mapping:
    width: int
    height: int
    spots: dict


@dataclass(frozen=True)
class InputProfile:
    input_id: str
    source: Source
    detector: DetectorProfile
    mapping: Mapping
    fps: float = 2.5


@dataclass(frozen=True)
class RuntimeConfiguration:
    organization_id: str
    installation_id: str
    environment: str
    state_dir: Path
    gateway_url: str
    credential_env: str
    inputs: tuple[InputProfile, ...]
    max_frame_age: float
    stall_timeout: float
    startup_timeout: float
    inputs_per_process: int = 1

    @property
    def scope(self):
        return f"{self.organization_id}/{self.installation_id}/{self.environment}"


def _mapping(path):
    value = _fields(_read(path, 2 * 1024 * 1024), ("width", "height", "spots"))
    width, height = value["width"], value["height"]
    if (
        type(width) is not int
        or type(height) is not int
        or not 1 <= width <= 8192
        or not 1 <= height <= 8192
        or width * height > 16 * 1024 * 1024
    ):
        raise ConfigurationError("invalid mapping dimensions")
    spots = value["spots"]
    if not isinstance(spots, dict) or not 1 <= len(spots) <= 10000:
        raise ConfigurationError("invalid spot collection")
    for spot_id, points in spots.items():
        _id(spot_id)
        if not isinstance(points, list) or not 3 <= len(points) <= 64:
            raise ConfigurationError("invalid polygon")
        for point in points:
            if (
                not isinstance(point, list)
                or len(point) != 2
                or any(type(p) is not int for p in point)
                or not 0 <= point[0] < width
                or not 0 <= point[1] < height
            ):
                raise ConfigurationError("polygon outside reference frame")
        area = sum(
            a[0] * b[1] - b[0] * a[1] for a, b in zip(points, points[1:] + points[:1])
        )
        if area == 0:
            raise ConfigurationError("zero-area polygon")
    return Mapping(width, height, spots)


def load_configuration(path) -> RuntimeConfiguration:
    path = Path(path).resolve()
    base = path.parent
    data = _fields(
        _read(path, 512 * 1024),
        (
            "version",
            "installation_manifest",
            "state_dir",
            "gateway_url",
            "credential_env",
            "detectors",
            "sources",
            "inputs",
        ),
        ("max_frame_age", "stall_timeout", "startup_timeout", "inputs_per_process"),
    )
    if type(data["version"]) is not int or data["version"] != 1:
        raise ConfigurationError("unsupported runtime version")
    descriptor = _read(_path(base, data["installation_manifest"]), 16 * 1024)
    if (
        not isinstance(descriptor, dict)
        or type(descriptor.get("schema_version")) is not int
        or descriptor["schema_version"] != 1
    ):
        raise ConfigurationError("unsupported installation manifest")
    try:
        org = _uuid(descriptor["organization"]["id"])
        installation = _uuid(descriptor["installation"]["id"])
        environment = descriptor["environment"]
    except KeyError, TypeError:
        raise ConfigurationError("missing installation identity") from None
    if environment not in ("production", "demo", "test"):
        raise ConfigurationError("invalid installation environment")
    try:
        url = urlsplit(data["gateway_url"])
        valid_url = (
            url.scheme in ("ws", "wss")
            and url.hostname
            and url.username is None
            and url.password is None
            and not url.fragment
            and url.port != 0
        )
    except ValueError, TypeError, AttributeError:
        valid_url = False
    if not valid_url:
        raise ConfigurationError("invalid gateway URL")
    sources, detectors = {}, {}
    if (
        not isinstance(data["sources"], dict)
        or not isinstance(data["detectors"], dict)
        or len(data["sources"]) > 64
        or len(data["detectors"]) > 64
    ):
        raise ConfigurationError("invalid profile collections")
    for key, value in data["sources"].items():
        _id(key)
        if isinstance(value, dict) and value.get("kind") == "file":
            _fields(value, ("kind", "path"), ("loop",))
            loop = value.get("loop", False)
            if type(loop) is not bool or environment == "production":
                raise ConfigurationError(
                    "file source prohibited in production or invalid loop flag"
                )
            sources[key] = Source("file", _path(base, value["path"]), loop=loop)
        else:
            _fields(value, ("kind", "env"))
            if value["kind"] != "rtsp":
                raise ConfigurationError("unsupported source kind")
            sources[key] = Source("rtsp", env=_env(value["env"]))
    for key, value in data["detectors"].items():
        _id(key)
        detectors[key] = parse_detector_profile(value, base)
    if not isinstance(data["inputs"], list) or not 1 <= len(data["inputs"]) <= 64:
        raise ConfigurationError("expected 1 to 64 inputs")
    inputs, ids, destinations = [], set(), set()
    for value in data["inputs"]:
        _fields(value, ("input_id", "source", "detector", "mapping"), ("fps",))
        input_id = _id(value["input_id"])
        if input_id in ids:
            raise ConfigurationError("duplicate input ID")
        ids.add(input_id)
        try:
            source, detector = sources[value["source"]], detectors[value["detector"]]
        except KeyError, TypeError:
            raise ConfigurationError("unknown source or detector reference") from None
        mapping = _mapping(_path(base, value["mapping"]))
        if destinations.intersection(mapping.spots):
            raise ConfigurationError("duplicate destination spot")
        destinations.update(mapping.spots)
        inputs.append(
            InputProfile(
                input_id,
                source,
                detector,
                mapping,
                _number(value.get("fps", 2.5), 0.1, 30),
            )
        )
    age = _number(data.get("max_frame_age", 2), 0.1, 30)
    stall = _number(data.get("stall_timeout", 30), 5, 300)
    if stall <= age:
        raise ConfigurationError("stall timeout must exceed frame age")
    if any(stall <= 1.0 / item.fps + age for item in inputs):
        raise ConfigurationError("stall timeout must allow sampling and processing age")
    group_size = data.get("inputs_per_process", 1)
    if type(group_size) is not int or not 1 <= group_size <= 4:
        raise ConfigurationError("inputs_per_process must be an integer from 1 to 4")
    return RuntimeConfiguration(
        org,
        installation,
        environment,
        _path(base, data["state_dir"], file=False),
        data["gateway_url"],
        _env(data["credential_env"]),
        tuple(inputs),
        age,
        stall,
        _number(data.get("startup_timeout", 120), 10, 600),
        group_size,
    )
