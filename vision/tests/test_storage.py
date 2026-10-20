# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib

import pytest


def test_leases_reject_concurrent_writers_and_release_after_error(tmp_path):
    module = importlib.import_module("storage")
    path = tmp_path / "lease.lock"
    with module.FileLease(path):
        with pytest.raises(module.LeaseConflict):
            with module.FileLease(path):
                pytest.fail("two owners admitted")
    with pytest.raises(RuntimeError):
        with module.FileLease(path):
            raise RuntimeError("worker failed")
    with module.FileLease(path):
        pass


def test_storage_is_scoped_by_installation_and_input(tmp_path):
    module = importlib.import_module("storage")
    first = module.input_directory(tmp_path, "org/site/test", "camera-A")
    assert first == module.input_directory(tmp_path, "org/site/test", "camera-A")
    assert first != module.input_directory(tmp_path, "org/site/test", "camera-B")
    assert first != module.input_directory(tmp_path, "org/other/test", "camera-A")
    assert first.is_relative_to(tmp_path)
