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
