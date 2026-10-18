"""The offline contract command must not load credentials or connect to PostgreSQL."""

import json
import os
from pathlib import Path
import subprocess

import pytest


@pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"), reason="integration server required"
)
def test_contract_export_is_offline_and_matches_committed_artifacts(tmp_path):
    directory = tmp_path / "contracts"
    result = subprocess.run(
        [os.environ["ESTACIONA_TEST_SERVER"], "--export-contracts", str(directory)],
        env={"PATH": os.environ["PATH"], "DATABASE_URL": "not-a-database-url"},
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode == 0, result.stderr
    shared = Path(__file__).resolve().parents[2] / "shared"
    for source in [shared / "messages.json", *sorted((shared / "v1").glob("*.json"))]:
        exported = directory / source.relative_to(shared)
        assert json.loads(exported.read_text()) == json.loads(source.read_text())
