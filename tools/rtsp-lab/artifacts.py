# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Small, bounded lab artifacts and streaming checksums."""

import hashlib
import json
from pathlib import Path


class LabError(ValueError):
    """Invalid lab input or failed disposable service operation."""


def read_json(path):
    def pairs(items):
        value = {}
        for key, item in items:
            if key in value:
                raise LabError("duplicate JSON key")
            value[key] = item
        return value

    try:
        with Path(path).open("rb") as stream:
            raw = stream.read(2 * 1024 * 1024 + 1)
        if len(raw) > 2 * 1024 * 1024:
            raise LabError("lab JSON is too large")
        return json.loads(raw, object_pairs_hook=pairs)
    except OSError, UnicodeError, json.JSONDecodeError:
        raise LabError("cannot read lab JSON") from None


def write_json(path, value):
    path = Path(path)
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("x") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")
    temporary.replace(path)


def checksum(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()
