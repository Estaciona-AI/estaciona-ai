# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from ml.backtest import BASELINES, HORIZON
from ml.inference import ForecastLedger
from ml.prospective import prospective_report

from forecast_db import history_engine

START = datetime(2026, 10, 19, tzinfo=timezone.utc)  # a Monday, 00:00 UTC
SPOTS = ["a", "b", "c", "d"]


def hour(offset):
    return START + timedelta(hours=offset)


def lot(occupied_by_hour, environment="production"):
    """A four-spot lot; `occupied_by_hour` maps hour offsets to occupied spots.
    Hours left out were not observed at all."""
    rows = [
        (spot, hour(offset), "occupied" if index < occupied else "free")
        for offset, occupied in occupied_by_hour.items()
        for index, spot in enumerate(SPOTS)
    ]
    return history_engine(SPOTS, rows=rows, environment=environment)


def issue(ledger, offset, served, issued_after=timedelta(minutes=5), **methods):
    """Record a forecast for origin `offset`; each method holds one rate."""
    flat = {name: np.full(HORIZON, value) for name, value in methods.items()}
    ledger.record(
        hour(offset),
        np.full(HORIZON, served),
        hour(offset) + issued_after,
        "model",
        flat,
    )


def everything(model, held):
    """A value for the model and one shared by all the baselines."""
    return {"model": model, **{name: held for name in BASELINES}}


@pytest.fixture
def ledger(tmp_path):
    return ForecastLedger(tmp_path / "ledger.db")


def test_every_method_is_scored_on_the_same_issued_points(ledger):
    # Half the lot is occupied for two days.
    engine = lot({offset: 2 for offset in range(48)})
    issue(ledger, 0, served=0.5, **everything(model=0.5, held=0.75))
    issue(ledger, 24, served=0.25, **everything(model=0.25, held=0.75))
    report = prospective_report(ledger, engine, now=hour(48))

    assert report["status"] == "evaluated"
    assert report["issued_origins"] == 2
    assert report["excluded"] == {
        "issued_after_target": 0,
        "pending": 0,
        "unobserved": 0,
    }
    scores = report["scores"]
    assert scores["points"] == 48 and scores["target_hours"] == 48
    assert scores["covered_points"] == {
        name: 48 for name in ("served", "model") + BASELINES
    }
    # Exact on the first day and 25 points off on the second: 12.5 overall.
    assert scores["served"]["nmae_pct"] == pytest.approx(12.5)
    assert scores["persistence"]["nmae_pct"] == pytest.approx(25.0)
    assert len(scores["served"]["nmae_pct_by_horizon"]) == HORIZON
    # One day each way: two days are the evidence, not 48 points.
    assert report["daily"]["days"] == 2
    assert report["daily"]["served_better_days"]["persistence"] == 1
    engine.dispose()


def test_hours_that_have_not_closed_are_pending_not_scored(ledger):
    engine = lot({offset: 2 for offset in range(10)})
    issue(ledger, 0, served=0.5, **everything(model=0.5, held=0.5))
    # Ten hours after the origin; the eleventh is still in progress.
    report = prospective_report(ledger, engine, now=hour(10) + timedelta(minutes=40))
    assert report["scores"]["points"] == 10
    assert report["excluded"]["pending"] == 14
    assert report["scores"]["points_by_horizon"] == [1] * 10 + [0] * 14
    assert report["scores"]["served"]["nmae_pct_by_horizon"][10:] == [None] * 14
    engine.dispose()


def test_a_forecast_issued_this_hour_has_only_pending_hours(ledger):
    engine = lot({offset: 2 for offset in range(5)})
    issue(ledger, 5, served=0.5, **everything(model=0.5, held=0.5))
    # The origin hour itself is still open: not yet an outcome, nor a gap.
    report = prospective_report(ledger, engine, now=hour(5) + timedelta(minutes=5))
    assert report["status"] == "unavailable"
    assert report["reason"] == "no_comparable_points"
    assert report["excluded"] == {
        "issued_after_target": 0,
        "pending": 24,
        "unobserved": 0,
    }
    engine.dispose()


def test_an_unobserved_hour_is_excluded_instead_of_counted_as_empty(ledger):
    observed = {offset: 4 for offset in range(24) if offset not in (3, 4)}
    engine = lot(observed)
    # A forecast of an empty lot would be exact if absence meant zero.
    issue(ledger, 0, served=0.0, **everything(model=0.0, held=1.0))
    report = prospective_report(ledger, engine, now=hour(24))
    assert report["excluded"]["unobserved"] == 2
    assert report["scores"]["points"] == 22
    assert report["scores"]["served"]["nmae_pct"] == pytest.approx(100.0)
    assert report["scores"]["persistence"]["nmae_pct"] == pytest.approx(0.0)
    engine.dispose()


def test_a_forecast_recorded_after_its_hours_closed_is_not_prospective(ledger):
    engine = lot({offset: 2 for offset in range(24)})
    # Recorded three and a half hours after the origin: hours 0, 1 and 2 were
    # already over, so nothing recorded for them is a forecast.
    issue(
        ledger,
        0,
        served=0.5,
        issued_after=timedelta(hours=3, minutes=30),
        **everything(model=0.5, held=0.5),
    )
    report = prospective_report(ledger, engine, now=hour(24))
    assert report["excluded"]["issued_after_target"] == 3
    assert report["scores"]["points"] == 21
    assert report["scores"]["points_by_horizon"][:4] == [0, 0, 0, 1]
    engine.dispose()


def test_a_method_missing_at_an_origin_leaves_that_origin_out_for_everyone(ledger):
    engine = lot({offset: 2 for offset in range(48)})
    # The first origin predates the baselines: only the served forecast exists.
    issue(ledger, 0, served=0.5)
    issue(ledger, 24, served=0.25, **everything(model=0.25, held=0.5))
    report = prospective_report(ledger, engine, now=hour(48))
    scores = report["scores"]
    assert scores["covered_points"]["served"] == 48
    assert scores["covered_points"]["persistence"] == 24
    assert scores["points"] == 24 and scores["origins"] == 1
    # Scored where the baselines are too: the easy first day does not count.
    assert scores["served"]["nmae_pct"] == pytest.approx(25.0)
    assert scores["persistence"]["nmae_pct"] == pytest.approx(0.0)
    engine.dispose()


def test_overlapping_forecasts_are_not_reported_as_more_hours(ledger):
    engine = lot({offset: 2 for offset in range(30)})
    for offset in range(6):
        issue(ledger, offset, served=0.5, **everything(model=0.5, held=0.5))
    report = prospective_report(ledger, engine, now=hour(29))
    assert report["scores"]["points"] == 6 * 24
    assert report["scores"]["target_hours"] == 29
    assert report["daily"]["days"] == 1
    engine.dispose()


def test_nothing_issued_or_nothing_observed_is_unavailable_not_zero_error(ledger):
    engine = lot({})
    report = prospective_report(ledger, engine, now=hour(48))
    assert report["status"] == "unavailable"
    assert report["reason"] == "no_issued_forecasts"
    assert "scores" not in report

    issue(ledger, 0, served=0.5, **everything(model=0.5, held=0.5))
    report = prospective_report(ledger, engine, now=hour(48))
    assert report["status"] == "unavailable"
    assert report["reason"] == "no_comparable_points"
    assert report["excluded"]["unobserved"] == 24
    assert "served" not in report["scores"] and "daily" not in report
    engine.dispose()


@pytest.mark.parametrize(
    "environment, recorded_sources_allowed", [("production", False), ("demo", True)]
)
def test_the_report_says_whether_the_history_may_be_a_recording(
    ledger, environment, recorded_sources_allowed
):
    engine = lot({offset: 2 for offset in range(24)}, environment=environment)
    issue(ledger, 0, served=0.5, **everything(model=0.5, held=0.5))
    report = prospective_report(ledger, engine, now=hour(24))
    assert report["history"] == {
        "environment": environment,
        "recorded_sources_allowed": recorded_sources_allowed,
    }
    engine.dispose()
