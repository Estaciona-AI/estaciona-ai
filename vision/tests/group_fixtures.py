# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Spawn-safe model/capture fixtures; grouping, transport and SQLite remain real."""


def measured_fixture_group(config, items, health, previews, measurement):
    import datetime
    import time
    import numpy as np
    import worker
    import process_groups
    from capture import CapturedFrame

    class Capture:
        ended = False
        error = None

        def __init__(self, *args, **kwargs):
            self.sequence = 0

        def start(self):
            pass

        def latest(self, **kwargs):
            self.sequence += 1
            return CapturedFrame(
                self.sequence,
                1,
                np.zeros((10, 10, 3), np.uint8),
                time.monotonic(),
                datetime.datetime.now(datetime.UTC).isoformat(),
            )

        def close(self):
            return True

    class Model:
        def detect(self, *args, **kwargs):
            return []

        def metadata(self):
            return {"adapter": "fixture", "providers": ["fixture"]}

    worker.LatestCapture = Capture
    worker.load_detector = lambda *args, **kwargs: Model()
    process_groups.group_entry(config, items, health, previews, measurement)
