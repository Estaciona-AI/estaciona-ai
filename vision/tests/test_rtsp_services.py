# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib
import json
from pathlib import Path
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/rtsp-lab"))


def test_stop_refuses_to_remove_container_with_foreign_labels(tmp_path, monkeypatch):
    module = importlib.import_module("services")
    session = module.LabSession(tmp_path)
    calls = []

    def fake_docker(*args, **kwargs):
        calls.append(args)
        if args[0] == "ps":
            return "a" * 64
        if args[0] == "inspect":
            return json.dumps(
                [{"Config": {"Labels": {"tech.estaciona.rtsp.run": "foreign"}}}]
            )
        pytest.fail("unowned resource mutation")

    monkeypatch.setattr(module, "docker", fake_docker)
    with pytest.raises(module.LabError, match="ownership"):
        session.remove_owned("server", "b" * 32)
    assert not any(call[0] == "rm" for call in calls)


def test_service_configuration_only_enables_tcp_rtsp():
    module = importlib.import_module("services")
    settings = module.server_config()
    assert settings["rtspTransports"] == ["tcp"]
    assert settings["paths"] == {"showcase": {"source": "publisher"}}
    assert all(
        settings[name] is False
        for name in ("api", "metrics", "rtmp", "hls", "webrtc", "srt", "moq")
    )


def test_stop_preserves_unowned_environment_without_session(tmp_path, monkeypatch):
    module = importlib.import_module("services")
    manifest = {
        "version": 1,
        "run_id": "b" * 32,
        "simulation": True,
        "image": module.IMAGE,
    }
    (tmp_path / "scenario.json").write_text(json.dumps(manifest))
    env = tmp_path / "stream.env"
    env.write_text("USER_VALUE=keep\n")
    monkeypatch.setattr(module, "docker", lambda *args, **kwargs: "")
    module.LabSession(tmp_path).stop()
    assert env.read_text() == "USER_VALUE=keep\n"


def test_start_rejects_existing_environment_before_creating_services(
    tmp_path, monkeypatch
):
    module = importlib.import_module("services")
    manifest = {
        "version": 1,
        "run_id": "b" * 32,
        "simulation": True,
        "image": module.IMAGE,
    }
    (tmp_path / "scenario.json").write_text(json.dumps(manifest))
    env = tmp_path / "stream.env"
    env.write_text("USER_VALUE=keep\n")

    def fake(*args, **kwargs):
        if args[0] == "ps":
            return ""
        raise module.LabError("simulated startup failure")

    monkeypatch.setattr(module, "docker", fake)
    with pytest.raises(module.LabError):
        module.LabSession(tmp_path).start()
    assert env.read_text() == "USER_VALUE=keep\n"


def test_publisher_cannot_report_ready_if_container_exits(tmp_path, monkeypatch):
    module = importlib.import_module("services")
    video = tmp_path / "video.mp4"
    video.write_bytes(b"video")
    run_id = "b" * 32
    manifest = {
        "run_id": run_id,
        "video": str(video),
        "video_sha256": module.checksum(video),
    }
    (tmp_path / "lab-state.json").write_text(
        json.dumps({"run_id": run_id, "stream_url": "rtsp://127.0.0.1:8554/showcase"})
    )
    session = module.LabSession(tmp_path)
    monkeypatch.setattr(module, "docker", lambda *args, **kwargs: "container-id")
    monkeypatch.setattr(session, "owned", lambda *args: {"State": {"Running": False}})
    with pytest.raises(module.LabError, match="publisher"):
        session._publisher(manifest)
