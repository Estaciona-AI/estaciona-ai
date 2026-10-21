# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Disposable lab media services; no host publisher processes or shell commands."""

from fractions import Fraction
import json
from pathlib import Path
import subprocess

from artifacts import LabError, checksum, read_json, write_json

IMAGE = "bluenviron/mediamtx:1.21.2-ffmpeg@sha256:c0feb4acc548d5f410d57d9046f62dfa355e2abc4098f5243de9e86430c77a95"


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


def server_config(port=8554):
    return {
        "logLevel": "info",
        "rtsp": True,
        "rtspTransports": ["tcp"],
        "rtspAddress": f"127.0.0.1:{port}",
        "api": False,
        "metrics": False,
        "pprof": False,
        "rtmp": False,
        "hls": False,
        "webrtc": False,
        "srt": False,
        "moq": False,
        "paths": {"showcase": {"source": "publisher"}},
    }


class LabSession:
    def __init__(self, directory):
        self.directory = Path(directory).resolve()

    def name(self, role, run_id):
        import re

        if (
            role not in ("server", "publisher")
            or not isinstance(run_id, str)
            or not re.fullmatch("[0-9a-f]{32}", run_id)
        ):
            raise LabError("invalid lab resource identity")
        return f"estaciona-rtsp-{run_id}-{role}"

    def owned(self, role, run_id):
        name = self.name(role, run_id)
        found = docker("ps", "-aq", "--filter", f"name=^/{name}$")
        if not found:
            return None
        value = json.loads(docker("inspect", name))[0]
        labels = value.get("Config", {}).get("Labels", {}) or {}
        if (
            labels.get("tech.estaciona.rtsp.run") != run_id
            or labels.get("tech.estaciona.rtsp.role") != role
        ):
            raise LabError("container ownership mismatch; refusing mutation")
        return value

    def remove_owned(self, role, run_id):
        value = self.owned(role, run_id)
        if value is not None:
            docker("rm", "-f", value["Id"])

    def manifest(self):
        value = read_json(self.directory / "scenario.json")
        if (
            not isinstance(value, dict)
            or value.get("version") != 1
            or value.get("simulation") is not True
            or value.get("image") != IMAGE
        ):
            raise LabError("not a supported generated lab scenario")
        self.name("server", value.get("run_id"))
        return value

    def stream_url(self, manifest):
        import re

        state = read_json(self.directory / "lab-state.json")
        url = state.get("stream_url")
        if (
            state.get("run_id") != manifest["run_id"]
            or not isinstance(url, str)
            or not re.fullmatch(r"rtsp://127\.0\.0\.1:[0-9]{1,5}/showcase", url)
        ):
            raise LabError("invalid lab stream state")
        return url

    def _publisher(self, manifest):
        if checksum(manifest["video"]) != manifest["video_sha256"]:
            raise LabError("source video changed; generate a new scenario")
        run_id = manifest["run_id"]
        import datetime
        import time

        since = datetime.datetime.now(datetime.UTC).isoformat().replace("+00:00", "Z")
        container_id = docker(
            "run",
            "-d",
            "--name",
            self.name("publisher", run_id),
            "--label",
            f"tech.estaciona.rtsp.run={run_id}",
            "--label",
            "tech.estaciona.rtsp.role=publisher",
            "--network",
            f"container:{self.name('server', run_id)}",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "64",
            "--memory",
            "512m",
            "--mount",
            media_mount(manifest["video"]),
            "--entrypoint",
            "ffmpeg",
            IMAGE,
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-re",
            "-stream_loop",
            "-1",
            "-i",
            "/media/source.mp4",
            "-map",
            "0:v:0",
            "-an",
            "-c:v",
            "copy",
            "-rtsp_transport",
            "tcp",
            "-f",
            "rtsp",
            self.stream_url(manifest),
        )

        deadline = time.monotonic() + 10
        while True:
            publisher = self.owned("publisher", run_id)
            if publisher is None or not publisher["State"]["Running"]:
                raise LabError("publisher exited before publishing its RTSP stream")
            logs = docker("logs", "--since", since, self.name("server", run_id))
            if "is publishing to path 'showcase'" in logs:
                return container_id
            if time.monotonic() >= deadline:
                raise LabError("publisher RTSP path did not become ready")
            time.sleep(0.1)

    def _stop(self, manifest):
        run_id = manifest["run_id"]
        state_path = self.directory / "lab-state.json"
        expected_env = None
        if state_path.exists() or state_path.is_symlink():
            if state_path.is_symlink():
                raise LabError("refusing symlinked lab state")
            expected_env = f"ESTACIONA_LAB_RTSP={self.stream_url(manifest)}\n"
        self.remove_owned("publisher", run_id)
        self.remove_owned("server", run_id)
        env = self.directory / "stream.env"
        if expected_env is not None and env.is_file() and not env.is_symlink():
            if env.read_text() == expected_env:
                env.unlink()
        state_path.unlink(missing_ok=True)

    def start(self):
        import socket
        import time
        from storage import FileLease

        with FileLease(self.directory / "lab-control.lock"):
            manifest = self.manifest()
            env = self.directory / "stream.env"
            if env.exists() or env.is_symlink():
                raise LabError("reserved stream.env already exists; preserving it")
            if (self.directory / "lab-state.json").exists():
                raise LabError(
                    "lab state already exists; stop it before starting again"
                )
            run_id = manifest["run_id"]
            for role in ("server", "publisher"):
                if self.owned(role, run_id) is not None:
                    raise LabError(
                        "owned container already exists; use stop to reconcile"
                    )
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            config = self.directory / "mediamtx.json"
            if config.exists():
                previous = read_json(config)
                try:
                    old_port = int(previous["rtspAddress"].removeprefix("127.0.0.1:"))
                except (KeyError, TypeError, ValueError):
                    raise LabError("unexpected media server configuration") from None
                if previous != server_config(old_port):
                    raise LabError("unexpected media server configuration")
            write_json(config, server_config(port))
            state = {
                "run_id": run_id,
                "stream_url": f"rtsp://127.0.0.1:{port}/showcase",
            }
            write_json(self.directory / "lab-state.json", state)
            try:
                docker(
                    "run",
                    "-d",
                    "--name",
                    self.name("server", run_id),
                    "--label",
                    f"tech.estaciona.rtsp.run={run_id}",
                    "--label",
                    "tech.estaciona.rtsp.role=server",
                    "--read-only",
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges",
                    "--pids-limit",
                    "64",
                    "--memory",
                    "256m",
                    "--network",
                    "host",
                    "--mount",
                    f"type=bind,src={config},dst=/mediamtx.yml,readonly",
                    IMAGE,
                    "/mediamtx.yml",
                )
                end = time.monotonic() + 10
                while True:
                    server = self.owned("server", run_id)
                    if server is None or not server["State"]["Running"]:
                        raise LabError("owned media server failed to start")
                    logs = docker("logs", self.name("server", run_id))
                    if (
                        f"[RTSP] started with listeners on 127.0.0.1:{port} (TCP/RTSP)"
                        in logs
                    ):
                        break
                    if time.monotonic() >= end:
                        raise LabError("owned RTSP listener did not become ready")
                    time.sleep(0.1)
                self._publisher(manifest)
                write_json(self.directory / "lab-state.json", state)
                env = self.directory / "stream.env"
                with env.open("x") as stream:
                    stream.write(f"ESTACIONA_LAB_RTSP={state['stream_url']}\n")
                return state
            except BaseException:
                self._stop(manifest)
                raise

    def stop(self):
        from storage import FileLease

        with FileLease(self.directory / "lab-control.lock"):
            self._stop(self.manifest())

    def restart_publisher(self):
        from storage import FileLease

        with FileLease(self.directory / "lab-control.lock"):
            manifest = self.manifest()
            server = self.owned("server", manifest["run_id"])
            if server is None or not server["State"]["Running"]:
                raise LabError("owned server is not running")
            self.remove_owned("publisher", manifest["run_id"])
            self._publisher(manifest)

    def status(self):
        manifest = self.manifest()
        state = (
            read_json(self.directory / "lab-state.json")
            if (self.directory / "lab-state.json").exists()
            else {}
        )
        if state and state.get("run_id") != manifest["run_id"]:
            raise LabError("lab state identity mismatch")
        result = {"simulation": True, "stream_url": state.get("stream_url")}
        for role in ("server", "publisher"):
            container = self.owned(role, manifest["run_id"])
            result[role] = bool(container and container["State"]["Running"])
        return result

    def pause_publisher(self):
        from storage import FileLease

        with FileLease(self.directory / "lab-control.lock"):
            manifest = self.manifest()
            self.remove_owned("publisher", manifest["run_id"])
