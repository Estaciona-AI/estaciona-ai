# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import pytest

import benchmark
from test_worker import config


@pytest.mark.parametrize(
    "arguments",
    [
        ["capacity", "--counts", "1", "--seconds", "nan"],
        ["capacity", "--counts", "2,1"],
        ["corpus", "--video", "missing", "--input-id", "cam-A", "--frames", "0"],
        [
            "compare",
            "--profiles",
            "missing",
            "--video",
            "missing",
            "--input-id",
            "cam-A",
            "--frames",
            "0",
        ],
    ],
)
def test_invalid_limits_cannot_start_experiments(tmp_path, monkeypatch, arguments):
    monkeypatch.setattr(benchmark, "load_configuration", lambda _: config(tmp_path))
    monkeypatch.setattr(
        benchmark,
        "capacity_trial",
        lambda *args, **kwargs: pytest.fail("invalid capacity started"),
    )
    monkeypatch.setattr(
        benchmark,
        "compare",
        lambda *args, **kwargs: pytest.fail("invalid comparison started"),
    )
    assert (
        benchmark.main(
            [
                arguments[0],
                "configuration",
                "--output",
                str(tmp_path / "results"),
                *arguments[1:],
            ]
        )
        == 2
    )
    assert not (tmp_path / "results").exists()


def test_evaluation_does_not_load_runtime_or_start_inference(tmp_path, monkeypatch):
    seen = []
    monkeypatch.setattr(
        benchmark,
        "load_configuration",
        lambda _: pytest.fail("runtime configuration loaded"),
    )
    monkeypatch.setattr(
        benchmark, "evaluate", lambda *args: seen.append(args) or {"completed": True}
    )
    assert (
        benchmark.main(
            [
                "evaluate",
                "--corpus",
                "reference.json",
                "--comparison",
                "comparison",
                "--output",
                str(tmp_path / "out"),
            ]
        )
        == 0
    )
    assert len(seen) == 1
