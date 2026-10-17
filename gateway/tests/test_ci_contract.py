"""Exercise the integration gate through pytest's real exit status."""

import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest


@pytest.mark.parametrize(
    "scenario,expected",
    [
        ("missing_binary", 4),
        ("not_executable", 4),
        ("missing_database", 4),
        ("skipped", 1),
        ("passed", 0),
    ],
)
def test_required_integration_gate(tmp_path, scenario, expected):
    shutil.copyfile(Path(__file__).with_name("conftest.py"), tmp_path / "conftest.py")
    test = "def test_probe(): assert True\n"
    if scenario == "skipped":
        test = 'import pytest\n@pytest.mark.skip(reason="gate probe")\ndef test_probe(): pass\n'
    (tmp_path / "test_probe.py").write_text(test)
    env = {
        **os.environ,
        "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1",
        "ESTACIONA_REQUIRE_INTEGRATION": "1",
        "ESTACIONA_TEST_SERVER": sys.executable,
        "DATABASE_URL": "postgres://unused-gate-probe",
    }
    if scenario == "missing_binary":
        env.pop("ESTACIONA_TEST_SERVER")
    elif scenario == "not_executable":
        binary = tmp_path / "not-executable"
        binary.write_text("fixture")
        binary.chmod(0o600)
        env["ESTACIONA_TEST_SERVER"] = str(binary)
    elif scenario == "missing_database":
        env.pop("DATABASE_URL")
    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", str(tmp_path)],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == expected, result.stdout + result.stderr
