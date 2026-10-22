# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
import pytest
from benchmark_evaluation import evaluate
from test_benchmark_corpus import corpus as corpus


def predictions(tmp_path, data):
    directory = tmp_path / "comparison"
    directory.mkdir()
    report = {
        "schema_version": 2,
        "completed": True,
        "same_frames_verified": True,
        "video": data["video"],
        "mapping_sha256": data["mapping_sha256"],
        "profiles": ["test"],
        "profile_parameters": {
            "test": {
                "confidence": 0.3,
                "gamma": 1.0,
                "tile_size": 512,
                "tile_overlap": 0.25,
            }
        },
        "stride": 2,
        "warmup_frames": 1,
        "requested_frames": 9,
    }
    (directory / "comparison.json").write_text(json.dumps(report))
    hashes = {f["frame_index"]: f["frame_sha256"] for f in data["frames"]}
    rows = []
    for i in range(2, 20, 2):
        status = "occupied" if i <= 10 else "free"
        rows.append(
            {
                "frame_index": i,
                "frame_sha256": hashes.get(i, "a" * 64),
                "spots": [
                    {
                        "id": "A",
                        "status": status,
                        "ratio": 0.8 if status == "occupied" else 0.0,
                    }
                ],
            }
        )
    (directory / "test.frames.jsonl").write_text(
        "".join(json.dumps(r) + "\n" for r in rows)
    )
    return directory, rows


def test_uncertain_labels_are_excluded_and_draft_is_not_quality_validation(
    corpus, tmp_path
):
    path, data, *_ = corpus
    for frame, label in zip(data["frames"], ["occupied", "free", "uncertain"]):
        frame["labels"]["A"] = label
    path.write_text(json.dumps(data))
    directory, _ = predictions(tmp_path, data)
    result = evaluate(path, directory, tmp_path / "evaluation")
    assert result["reference_reviewed"] is False
    assert result["quality_validated"] is False
    row = result["profiles"]["test"]
    assert row["confusion"] == {
        "true_occupied": 1,
        "true_free": 0,
        "false_occupied": 1,
        "missed_occupied": 0,
    }
    assert row["coverage"] == {"scored": 2, "uncertain": 1, "total": 3}
    assert row["precision_occupied"] == 0.5
    assert row["recall_occupied"] == 1.0
    assert row["confirmed_transitions"] == {"A": 1}


@pytest.mark.parametrize(
    "change", ["hash", "missing", "duplicate", "unknown_spot", "mapping", "sampling"]
)
def test_evaluation_rejects_incomparable_or_incomplete_predictions(
    corpus, tmp_path, change
):
    path, data, *_ = corpus
    directory, rows = predictions(tmp_path, data)
    report = json.loads((directory / "comparison.json").read_text())
    if change == "hash":
        rows[0]["frame_sha256"] = "b" * 64
    elif change == "missing":
        rows.pop(0)
    elif change == "duplicate":
        rows.append(rows[-1])
    elif change == "unknown_spot":
        rows[0]["spots"][0]["id"] = "other"
    elif change == "mapping":
        report["mapping_sha256"] = "b" * 64
    elif change == "sampling":
        report["stride"] = 3
    (directory / "comparison.json").write_text(json.dumps(report))
    (directory / "test.frames.jsonl").write_text(
        "".join(json.dumps(r) + "\n" for r in rows)
    )
    with pytest.raises(ValueError):
        evaluate(path, directory, tmp_path / "bad")


def test_zero_reviewed_labels_cannot_produce_accuracy(corpus, tmp_path):
    path, data, *_ = corpus
    data["reviewed"] = True
    path.write_text(json.dumps(data))
    directory, _ = predictions(tmp_path, data)
    result = evaluate(path, directory, tmp_path / "empty")
    assert not result["quality_validated"]
    assert result["profiles"]["test"]["precision_occupied"] is None
    assert result["profiles"]["test"]["recall_occupied"] is None


@pytest.mark.parametrize(
    "change",
    [
        "label",
        "missing_label",
        "review_flag",
        "duplicate_frame",
        "invalid_spot_id",
        "empty_frames",
        "nan_fps",
    ],
)
def test_bad_reference_is_rejected(corpus, tmp_path, change):
    path, data, *_ = corpus
    directory, _ = predictions(tmp_path, data)
    if change == "label":
        data["frames"][0]["labels"]["A"] = "automatic_guess"
    elif change == "missing_label":
        data["frames"][0]["labels"] = {}
    elif change == "review_flag":
        data["reviewed"] = "true"
    elif change == "duplicate_frame":
        data["frames"].append(data["frames"][0])
    elif change == "invalid_spot_id":
        data["spot_ids"] = [["A"]]
    elif change == "empty_frames":
        data["frames"] = []
    elif change == "nan_fps":
        data["fps"] = float("nan")
    path.write_text(json.dumps(data))
    with pytest.raises(ValueError):
        evaluate(path, directory, tmp_path / "invalid")
