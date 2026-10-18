# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""CI must exercise the real server instead of silently skipping integration."""

import os
from pathlib import Path

import pytest


def pytest_sessionstart(session):
    if os.environ.get("ESTACIONA_REQUIRE_INTEGRATION") != "1":
        return
    binary = os.environ.get("ESTACIONA_TEST_SERVER", "")
    if not binary or not Path(binary).is_file() or not os.access(binary, os.X_OK):
        raise pytest.UsageError(
            "Integration requires an executable ESTACIONA_TEST_SERVER"
        )
    if not os.environ.get("DATABASE_URL"):
        raise pytest.UsageError("Integration requires DATABASE_URL")


def pytest_sessionfinish(session, exitstatus):
    if os.environ.get("ESTACIONA_REQUIRE_INTEGRATION") != "1":
        return
    reporter = session.config.pluginmanager.get_plugin("terminalreporter")
    if reporter and reporter.stats.get("skipped"):
        session.exitstatus = pytest.ExitCode.TESTS_FAILED
        reporter.write_sep(
            "=", "Required integration run contains skipped tests", red=True
        )
