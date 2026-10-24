# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Score frozen visual references against existing same-frame comparison artifacts."""

import json
import math
from pathlib import Path

from benchmark_corpus import load_corpus, _hash
from benchmark_profiles import artifact, results_directory
from configuration import _fields, _id, _pairs, _read
from metrics import atomic_json


def _score(reference, path):
    anchors = {f["frame_index"]: f for f in reference["frames"]}
    ids = set(reference["spot_ids"])
    previous = {}
    changes = {k: 0 for k in ids}
    counts = {
        "true_occupied": 0,
        "true_free": 0,
        "false_occupied": 0,
        "missed_occupied": 0,
    }
    scored = uncertain = 0
    matched = set()
    seen = []
    sampling = reference["sampling"]
    if path.stat().st_size > 256 * 1024 * 1024:
        raise ValueError("prediction artifact too large")
    with path.open() as stream:
        for _ in range(sampling["measured_frames"] + 1):
            line = stream.readline(2 * 1024 * 1024 + 1)
            if not line:
                break
            if len(line) > 2 * 1024 * 1024:
                raise ValueError("prediction row too large")
            row = json.loads(line, object_pairs_hook=_pairs)
            _fields(row, ("frame_index", "frame_sha256", "spots"), ("detections",))
            index = row["frame_index"]
            _hash(row["frame_sha256"])
            expected = (sampling["warmup_frames"] + len(seen)) * sampling["stride"]
            if type(index) is not int or index != expected:
                raise ValueError("prediction frame schedule mismatch")
            seen.append(index)
            spots = row["spots"]
            if not isinstance(spots, list) or len(spots) != len(ids):
                raise ValueError("prediction spot count mismatch")
            states = {}
            for spot in spots:
                _fields(spot, ("id", "status", "ratio"))
                key = spot["id"]
                status = spot["status"]
                ratio = spot["ratio"]
                if (
                    not isinstance(key, str)
                    or key not in ids
                    or key in states
                    or status not in ("free", "occupied")
                    or type(ratio) not in (int, float)
                    or not math.isfinite(ratio)
                    or not 0 <= ratio <= 1
                ):
                    raise ValueError("invalid predicted spot")
                states[key] = status
                if key in previous and status != previous[key]:
                    changes[key] += 1
                previous[key] = status
            if index not in anchors:
                continue
            anchor = anchors[index]
            if row["frame_sha256"] != anchor["frame_sha256"]:
                raise ValueError("reference and prediction frame hash mismatch")
            matched.add(index)
            for key, label in anchor["labels"].items():
                if label == "uncertain":
                    uncertain += 1
                    continue
                scored += 1
                actual = states[key]
                metric = (
                    ("true_occupied" if actual == "occupied" else "missed_occupied")
                    if label == "occupied"
                    else ("false_occupied" if actual == "occupied" else "true_free")
                )
                counts[metric] += 1
    if len(seen) != sampling["measured_frames"] or matched != anchors.keys():
        raise ValueError("incomplete prediction sequence or missing reference frame")
    tp, fp, fn = (
        counts["true_occupied"],
        counts["false_occupied"],
        counts["missed_occupied"],
    )
    return {
        "confusion": counts,
        "coverage": {
            "scored": scored,
            "uncertain": uncertain,
            "total": scored + uncertain,
        },
        "precision_occupied": tp / (tp + fp) if tp + fp else None,
        "recall_occupied": tp / (tp + fn) if tp + fn else None,
        "accuracy_on_scored_labels": (tp + counts["true_free"]) / scored
        if scored
        else None,
        "confirmed_transitions": changes,
        "sequence_frames": len(seen),
    }


def evaluate(corpus, comparison, output):
    corpus = Path(corpus).resolve()
    comparison = Path(comparison).resolve()
    reference_artifact = artifact(corpus)
    reference = load_corpus(corpus)
    report_path = comparison / "comparison.json"
    comparison_artifact = artifact(report_path)
    report = _read(report_path, 2 * 1024 * 1024)
    if (
        not isinstance(report, dict)
        or report.get("schema_version") != 2
        or report.get("completed") is not True
        or report.get("same_frames_verified") is not True
        or report.get("video") != reference["video"]
        or report.get("mapping_sha256") != reference["mapping_sha256"]
    ):
        raise ValueError("comparison does not match corpus")
    for a, b in [
        ("stride", "stride"),
        ("warmup_frames", "warmup_frames"),
        ("requested_frames", "measured_frames"),
    ]:
        if report.get(a) != reference["sampling"][b]:
            raise ValueError("comparison sampling mismatch")
    profiles = report.get("profiles")
    if (
        not isinstance(profiles, list)
        or not 1 <= len(profiles) <= 8
        or any(not isinstance(p, str) for p in profiles)
        or len(set(profiles)) != len(profiles)
    ):
        raise ValueError("invalid comparison profiles")
    results = {}
    prediction_artifacts = {}
    for profile in profiles:
        _id(profile)
        path = comparison / f"{profile}.frames.jsonl"
        prediction_artifacts[profile] = artifact(path)
        results[profile] = _score(reference, path)
        if artifact(path) != prediction_artifacts[profile]:
            raise ValueError("predictions changed during evaluation")
    if (
        artifact(corpus) != reference_artifact
        or artifact(report_path) != comparison_artifact
    ):
        raise ValueError("reference or comparison changed during evaluation")
    result = {
        "schema_version": 1,
        "kind": "frozen_corpus_evaluation",
        "completed": True,
        "reference_reviewed": reference["reviewed"],
        "quality_validated": False,
        "quality_gate": "not_defined; metrics are evidence, not deployment validation",
        "reference": reference_artifact,
        "comparison": comparison_artifact,
        "predictions": prediction_artifacts,
        "profile_parameters": report.get("profile_parameters", {}),
        "profiles": results,
        "sampling": reference["sampling"],
        "fps": reference["fps"],
        "temporal_semantics": "Observed confirmed changes at the recorded cadence; no false-transition claim without interval annotations.",
        "performance_semantics": "Offline replay timing is not RTSP capacity. Use capacity trials for rate and resource claims.",
        "full_pc_validated": False,
    }
    output = results_directory(output)
    atomic_json(output / "evaluation.json", result)
    return result
