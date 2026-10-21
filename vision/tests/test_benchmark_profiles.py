# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json

import pytest

from configuration import ConfigurationError
from benchmark_profiles import load_profiles, artifact, JsonLines


def test_profiles_are_explicit_local_and_redacted_from_metadata(tmp_path):
    model = tmp_path / "private.onnx"
    model.write_bytes(b"model")
    path = tmp_path / "profiles.json"
    path.write_text(
        json.dumps(
            {
                "version": 1,
                "profiles": {
                    "baseline": {
                        "adapter": "rtdetr",
                        "path": "private.onnx",
                        "device": "cpu",
                    }
                },
            }
        )
    )
    profiles = load_profiles(path)
    assert profiles[0].profile_id == "baseline"
    data = artifact(profiles[0].path)
    assert len(data["sha256"]) == 64
    assert str(tmp_path) not in json.dumps(data)
    for change in (
        {"adapter": "unapproved_plugin"},
        {"device": "automatic"},
        {"path": "https://model.invalid/a.onnx"},
    ):
        value = json.loads(path.read_text())
        value["profiles"]["baseline"].update(change)
        path.write_text(json.dumps(value))
        with pytest.raises(ConfigurationError):
            load_profiles(path)
        path.write_text(
            json.dumps(
                {
                    "version": 1,
                    "profiles": {
                        "baseline": {
                            "adapter": "rtdetr",
                            "path": "private.onnx",
                            "device": "cpu",
                        }
                    },
                }
            )
        )


def test_json_lines_has_hard_byte_limit_and_refuses_overwrite(tmp_path):
    path = tmp_path / "rows.jsonl"
    with JsonLines(path, max_bytes=32) as output:
        output.write({"a": 1})
        with pytest.raises(BufferError):
            output.write({"long": "x" * 100})
    assert len(path.read_bytes()) <= 32
    with pytest.raises(FileExistsError):
        with JsonLines(path):
            pass
