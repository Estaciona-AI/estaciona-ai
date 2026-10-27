# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import copy
import json
from pathlib import Path

import pytest

import contract_compat

SHARED = Path(__file__).resolve().parents[1]


def write(directory, version, schemas, operations=("current_user",)):
    (directory / "v1").mkdir(parents=True)
    messages = {"x-contract-version": version, "anyOf": []}
    (directory / "messages.json").write_text(json.dumps(messages))
    for name, schema in schemas.items():
        (directory / "v1" / name).write_text(json.dumps(schema))
    catalog = {"http": [{"id": operation} for operation in operations]}
    (directory / "v1" / "catalog.json").write_text(json.dumps(catalog))
    return directory


SPOT = {
    "type": "object",
    "properties": {
        "spot_id": {"type": "string"},
        "status": {"type": "string", "enum": ["free", "occupied"]},
        "note": {"type": "string"},
    },
    "required": ["spot_id"],
}


def check(tmp_path, head_version, head, operations=("current_user",)):
    base = write(tmp_path / "base", "1.4.0", {"spot.schema.json": SPOT})
    changed = write(tmp_path / "head", head_version, head, operations)
    return contract_compat.check(base, changed)


def mutated(change):
    schema = copy.deepcopy(SPOT)
    change(schema)
    return {"spot.schema.json": schema}


@pytest.mark.parametrize(
    "change, expected",
    [
        (lambda s: s["properties"].pop("note"), "property 'note' removed"),
        (lambda s: s["required"].append("note"), "property 'note' became required"),
        (
            lambda s: s["properties"]["status"]["enum"].append("unknown"),
            "enum changed",
        ),
        (
            lambda s: s["properties"]["status"]["enum"].remove("occupied"),
            "enum changed",
        ),
    ],
    ids=["removed-property", "newly-required", "enum-added", "enum-removed"],
)
def test_incompatible_changes_need_a_new_major_version(tmp_path, change, expected):
    problems = check(tmp_path, "1.5.0", mutated(change))
    assert len(problems) == 1 and expected in problems[0], problems
    assert check(tmp_path / "major", "2.0.0", mutated(change)) == []


def test_removed_schemas_and_operations_need_a_new_major_version(tmp_path):
    problems = check(tmp_path, "1.5.0", {}, operations=())
    assert any("spot.schema.json: schema removed" in p for p in problems)
    assert any("'current_user' removed" in p for p in problems)


def test_compatible_additions_keep_the_major_version(tmp_path):
    def optional_property(schema):
        schema["properties"]["level"] = {"type": "integer"}
        schema["description"] = "Reworded"

    head = mutated(optional_property)
    head["new.schema.json"] = {"type": "object"}
    assert check(tmp_path, "1.5.0", head, ("current_user", "new_operation")) == []


def test_a_version_older_than_the_release_is_refused(tmp_path):
    problems = check(tmp_path, "0.9.0", {"spot.schema.json": SPOT})
    assert problems == ["version 0.9.0 is older than released 1.4.0"]


def test_the_published_bundle_is_compatible_with_itself():
    assert contract_compat.check(SHARED, SHARED) == []


def test_command_line_reports_problems_and_exit_status(tmp_path, capsys):
    base = write(tmp_path / "base", "1.4.0", {"spot.schema.json": SPOT})
    head = write(tmp_path / "head", "1.4.1", {})
    assert contract_compat.main(["contract_compat.py", str(base), str(head)]) == 1
    assert "schema removed" in capsys.readouterr().out
    assert contract_compat.main(["contract_compat.py", str(base), str(base)]) == 0
