# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""POSIX leases and stable per-installation/input outbox namespaces."""

import fcntl
import hashlib
import os
from pathlib import Path


class LeaseConflict(RuntimeError):
    pass


class FileLease:
    def __init__(self, path):
        self.path = Path(path)
        self._fd = None

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd = os.open(
            self.path, os.O_CREAT | os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW, 0o600
        )
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BaseException:
            os.close(fd)
            raise LeaseConflict("input or installation already owned") from None
        self._fd = fd
        return self

    def __exit__(self, *_):
        os.close(self._fd)
        self._fd = None
        # Never unlink a lease: another process may already hold the inode.


def input_directory(root, scope, input_id):
    scope_key = hashlib.sha256(scope.encode()).hexdigest()
    input_key = hashlib.sha256(input_id.encode()).hexdigest()
    return Path(root) / scope_key / input_key
