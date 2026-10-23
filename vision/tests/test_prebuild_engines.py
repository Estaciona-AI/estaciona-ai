# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

SOURCE = Path(__file__).resolve().parents[1] / "tools" / "prebuild_engines.py"
spec = importlib.util.spec_from_file_location("prebuild_engines", SOURCE)
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)

SPOT = [(10, 10), (60, 10), (60, 60), (10, 60)]


def profile(cache, provider="tensorrt", precision="fp16", model="r18.onnx"):
    return SimpleNamespace(
        path=Path(model),
        device="cuda",
        confidence=0.3,
        tile_size=512,
        tile_overlap=0.25,
        tile_scope="spots",
        provider=provider,
        precision=precision,
        engine_cache=cache,
    )


def item(name, detector, spots=None, size=(1920, 1080)):
    mapping = SimpleNamespace(width=size[0], height=size[1], spots=spots or {"A": SPOT})
    return SimpleNamespace(input_id=name, detector=detector, mapping=mapping)


class Loader:
    """Writes an engine file the first time a (cache, batch) pair is used."""

    def __init__(self, fail=()):
        self.calls = []
        self.fail = fail

    def __call__(self, path, device, **kwargs):
        self.calls.append(kwargs)
        cache = Path(kwargs["engine_cache"])
        tiles = len(
            tool.spot_windows(
                1920,
                1080,
                kwargs["tile_size"],
                kwargs["tile_overlap"],
                kwargs["mapping"].spots.values(),
            )
        )
        if str(path) in self.fail:
            raise RuntimeError("engine build failed")

        class Detector:
            def detect(self, frame, confidence):
                assert frame.shape == (1080, 1920, 3) and confidence == 0.3
                (cache / f"rtdetr_b{tiles}.engine").write_bytes(b"engine")
                return []

            def metadata(self):
                return {"batch": tiles}

        return Detector()


def test_profiles_sharing_an_engine_are_built_once(tmp_path):
    shared = profile(tmp_path)
    inputs = [item(f"cam-{index}", shared) for index in range(4)]
    loader = Loader()
    results, skipped = tool.prebuild(inputs, loader=loader)
    assert len(loader.calls) == 1
    assert results[0]["inputs"] == ["cam-0", "cam-1", "cam-2", "cam-3"]
    assert results[0]["status"] == "built" and results[0]["batch"] == 1
    assert skipped == []


def test_each_tile_count_and_precision_gets_its_own_engine(tmp_path):
    wide = {"A": SPOT, "B": [(1500, 700), (1700, 700), (1700, 900), (1500, 900)]}
    inputs = [
        item("one", profile(tmp_path)),
        item("many", profile(tmp_path), spots=wide),
        item("fp32", profile(tmp_path, precision="fp32")),
    ]
    results, _ = tool.prebuild(inputs, loader=Loader())
    assert sorted((r["tile_count"], r["precision"]) for r in results) == [
        (1, "fp16"),
        (1, "fp32"),
        (5, "fp16"),
    ]


def test_second_run_finds_the_cache_warm(tmp_path):
    inputs = [item("cam", profile(tmp_path))]
    first, _ = tool.prebuild(inputs, loader=Loader())
    second, _ = tool.prebuild(inputs, loader=Loader())
    assert (first[0]["status"], second[0]["status"]) == ("built", "cached")


def test_inputs_without_tensorrt_are_skipped(tmp_path):
    inputs = [item("cpu", profile(None, provider="default", precision="fp32"))]
    loader = Loader()
    results, skipped = tool.prebuild(inputs, loader=loader)
    assert (results, skipped, loader.calls) == ([], ["cpu"], [])


def test_a_failing_engine_does_not_stop_the_others(tmp_path):
    inputs = [
        item("bad", profile(tmp_path, model="broken.onnx")),
        item("good", profile(tmp_path)),
    ]
    results, _ = tool.prebuild(inputs, loader=Loader(fail={"broken.onnx"}))
    by_input = {r["inputs"][0]: r for r in results}
    assert by_input["bad"]["status"] == "failed"
    assert "engine build failed" in by_input["bad"]["error"]
    assert by_input["good"]["status"] == "built"


@pytest.mark.parametrize(
    ("inputs", "code"),
    [
        (lambda cache: [item("ok", profile(cache))], 0),
        (lambda cache: [item("bad", profile(cache, model="broken.onnx"))], 1),
        (lambda cache: [item("cpu", profile(None, "default", "fp32"))], 1),
    ],
)
def test_exit_status_and_report(tmp_path, inputs, code, capsys):
    report = tmp_path / "report.json"
    status = tool.main(
        ["runtime.json", "--report", str(report)],
        configuration=lambda path: SimpleNamespace(inputs=inputs(tmp_path)),
        loader=Loader(fail={"broken.onnx"}),
    )
    assert status == code
    assert set(json.loads(report.read_text())) == {"engines", "skipped"}
    assert capsys.readouterr().out
