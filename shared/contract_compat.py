# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Refuse an incompatible contract change released under the same major version.

The base is the bundle last promoted to ``main``. Within one major version
every consumer of the previous bundle must keep working, so a change that
could break a strict consumer or producer needs a new major version:

* a schema file or an HTTP operation disappears;
* a property disappears or becomes required;
* an enum gains or loses a value, or a constant changes.

Usage: ``python contract_compat.py BASE_DIR HEAD_DIR`` with two directories
that each hold ``messages.json`` and ``v1/``.
"""

import json
import sys
from pathlib import Path


def bundle(directory):
    directory = Path(directory)
    messages = json.loads((directory / "messages.json").read_text())
    schemas = {
        path.name: json.loads(path.read_text())
        for path in sorted((directory / "v1").glob("*.schema.json"))
    }
    schemas["messages.json"] = messages
    catalog = json.loads((directory / "v1" / "catalog.json").read_text())
    return messages["x-contract-version"], schemas, catalog


def _schema_changes(base, head, where):
    if isinstance(base, list) and isinstance(head, list):
        for index, (old, new) in enumerate(zip(base, head)):
            yield from _schema_changes(old, new, f"{where}[{index}]")
        if len(head) < len(base):
            yield f"{where}: {len(base) - len(head)} alternative(s) removed"
        return
    if not (isinstance(base, dict) and isinstance(head, dict)):
        return
    if "enum" in base and base.get("enum") != head.get("enum"):
        removed = sorted(
            set(map(json.dumps, base["enum"]))
            - set(map(json.dumps, head.get("enum", [])))
        )
        added = sorted(
            set(map(json.dumps, head.get("enum", [])))
            - set(map(json.dumps, base["enum"]))
        )
        yield f"{where}: enum changed (removed {removed}, added {added})"
    if "const" in base and base.get("const") != head.get("const"):
        yield f"{where}: const {base['const']!r} became {head.get('const')!r}"
    for name in sorted(
        set(base.get("properties", {})) - set(head.get("properties", {}))
    ):
        yield f"{where}: property {name!r} removed"
    for name in sorted(set(head.get("required", [])) - set(base.get("required", []))):
        yield f"{where}: property {name!r} became required"
    for key in sorted(set(base) & set(head)):
        if key not in ("enum", "const", "required", "description", "title"):
            yield from _schema_changes(base[key], head[key], f"{where}/{key}")


def breaking_changes(base, head):
    """Human-readable incompatibilities from bundle `base` to bundle `head`."""
    _, base_schemas, base_catalog = base
    _, head_schemas, head_catalog = head
    for name in sorted(set(base_schemas) - set(head_schemas)):
        yield f"{name}: schema removed"
    for name in sorted(set(base_schemas) & set(head_schemas)):
        yield from _schema_changes(base_schemas[name], head_schemas[name], name)
    head_operations = {operation["id"] for operation in head_catalog.get("http", [])}
    for operation in base_catalog.get("http", []):
        if operation["id"] not in head_operations:
            yield f"catalog: HTTP operation {operation['id']!r} removed"


def check(base_dir, head_dir):
    """Problems that block the head bundle; empty when it may be released."""
    base, head = bundle(base_dir), bundle(head_dir)
    base_major = int(base[0].split(".")[0])
    head_major = int(head[0].split(".")[0])
    if head_major < base_major:
        return [f"version {head[0]} is older than released {base[0]}"]
    if head_major > base_major:
        return []
    return [
        f"{change} (requires a new major version after {base[0]})"
        for change in breaking_changes(base, head)
    ]


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    problems = check(argv[1], argv[2])
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
