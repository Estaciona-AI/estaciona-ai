# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Disposable lab media services; no host publisher processes or shell commands."""

from fractions import Fraction
import json
from pathlib import Path
import subprocess

IMAGE = "bluenviron/mediamtx:1.21.2-ffmpeg@sha256:c0feb4acc548d5f410d57d9046f62dfa355e2abc4098f5243de9e86430c77a95"


class LabError(ValueError):
    pass


def docker(*args, timeout=60):
    try:
        result = subprocess.run(
            ["docker", *map(str, args)],
            check=True,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        raise LabError(
            "Docker lab operation failed; inspect the owned container logs"
        ) from None
    return result.stdout.strip()


def media_mount(video):
    path = Path(video).resolve()
    if not path.is_file() or "," in str(path):
        raise LabError("invalid media path for Docker mount")
    return f"type=bind,src={path},dst=/media/source.mp4,readonly"


def probe_video(video):
    raw = docker(
        "run",
        "--rm",
        "--network",
        "none",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--mount",
        media_mount(video),
        "--entrypoint",
        "ffprobe",
        IMAGE,
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=codec_name,width,height,r_frame_rate",
        "-of",
        "json",
        "/media/source.mp4",
    )
    try:
        stream = json.loads(raw)["streams"][0]
        width, height = stream["width"], stream["height"]
        fps = stream["r_frame_rate"]
        if (
            type(width) is not int
            or type(height) is not int
            or not 0 < width <= 8192
            or not 0 < height <= 8192
            or not 0 < Fraction(fps) <= 240
        ):
            raise ValueError
        if stream["codec_name"] not in ("h264", "hevc"):
            raise LabError(
                "streamcopy requires H.264 or H.265; convert explicitly before using this lab"
            )
        return {
            "width": width,
            "height": height,
            "codec": stream["codec_name"],
            "fps": fps,
        }
    except (KeyError, IndexError, TypeError, ValueError, ZeroDivisionError):
        raise LabError("unsupported or invalid video metadata") from None
