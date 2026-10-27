# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
from datetime import date

import numpy as np
import pandas as pd
import pytest

from ml import simulate, site_calendar


def write_profile(tmp_path, data):
    path = tmp_path / "forecast_profile.json"
    path.write_text(json.dumps(data))
    return path


def test_national_holidays_follow_easter():
    holidays = site_calendar.national_holidays(2026)
    for day in ("2026-02-16", "2026-02-17", "2026-04-03", "2026-06-04", "2026-11-20"):
        assert date.fromisoformat(day) in holidays
    assert date(2025, 4, 18) in site_calendar.national_holidays(2025)


def test_calendar_uses_the_site_local_time():
    profile = site_calendar.default_profile(10)
    index = pd.DatetimeIndex(["2026-10-24T02:00:00Z"])
    row = site_calendar.calendar_frame(profile, index).iloc[0]
    assert (row["hour"], row["weekday"]) == (23, 4)  # Friday 23:00 in Sao Paulo


def test_profile_periods_form_terms(tmp_path):
    path = write_profile(
        tmp_path,
        {
            "periods": [
                {"start": "2026-03-01", "end": "2026-06-20", "kind": "classes"},
                {"start": "2026-06-21", "end": "2026-06-30", "kind": "exams"},
            ],
            "holidays": ["2026-05-02"],
        },
    )
    days = site_calendar.DayCalendar(site_calendar.load_profile(path, 10))
    classes, exams, vacation = (
        days.day(date(2026, 4, 1)),
        days.day(date(2026, 6, 25)),
        days.day(date(2026, 7, 15)),
    )
    assert [classes[0], exams[0], vacation[0]] == [1, 2, 0]
    assert 0 < classes[1] < exams[1] <= 1
    assert days.day(date(2026, 5, 2))[2]


@pytest.mark.parametrize(
    "data",
    [
        {"timezone": "Mars/Base"},
        {"shifts": [{"start": "19:00", "end": "07:00", "cars": [1] * 6}]},
        {"shifts": [{"start": "07:00", "end": "11:00", "cars": [1] * 5}]},
        {"shifts": [{"start": "7h", "end": "11:00", "cars": [1] * 6}]},
        {"staff": -1},
        {"capacity": 40},
        {
            "periods": [
                {"start": "2026-03-01", "end": "2026-06-20", "kind": "classes"},
                {"start": "2026-06-01", "end": "2026-06-30", "kind": "exams"},
            ]
        },
        {"periods": [{"start": "2026-03-01", "end": "2026-06-20", "kind": "party"}]},
    ],
)
def test_profile_rejects_invalid_site_facts(tmp_path, data):
    with pytest.raises(ValueError):
        site_calendar.load_profile(write_profile(tmp_path, data), 10)


def shift_hours(tmp_path, start, end):
    data = {"shifts": [{"start": start, "end": end, "cars": [1] * 6}]}
    shift = site_calendar.load_profile(write_profile(tmp_path, data), 10).shifts[0]
    return shift.start, shift.end


def test_shift_times_are_fractional_hours_across_the_whole_day(tmp_path):
    assert shift_hours(tmp_path, "07:30", "11:45") == (7.5, 11.75)
    assert shift_hours(tmp_path, "00:00", "23:59") == (0.0, 23 + 59 / 60)
    assert shift_hours(tmp_path, "23:00", "23:30") == (23.0, 23.5)


@pytest.mark.parametrize(
    "start,end",
    [
        ("24:00", "23:00"),
        ("07:00", "24:00"),
        ("07:00", "12:60"),
        ("-1:00", "07:00"),
        ("07", "11:00"),
        ("07:00:00", "11:00"),
    ],
)
def test_shift_times_outside_hh_mm_are_refused(tmp_path, start, end):
    with pytest.raises(ValueError, match="must be HH:MM"):
        shift_hours(tmp_path, start, end)


def hourly_index(days, start="2026-01-05"):
    return pd.date_range(start, periods=days * 24, freq="h", tz="UTC")


def test_simulation_is_reproducible_and_bounded():
    profile = site_calendar.default_profile(40)
    index = hourly_index(21)
    first = simulate.synthetic_rates(profile, 40, index, variants=2, seed=7)
    second = simulate.synthetic_rates(profile, 40, index, variants=2, seed=7)
    for a, b in zip(first, second):
        np.testing.assert_array_equal(a, b)
        observed = a[np.isfinite(a)]
        assert observed.size and observed.min() >= 0 and observed.max() <= 1


def test_simulation_censors_demand_at_capacity():
    profile = site_calendar.default_profile(20)
    flooded = site_calendar.Profile(
        timezone=profile.timezone,
        shifts=(site_calendar.Shift(8.0, 18.0, (500,) * 6),),
        staff=0,
        visitors_per_hour=0,
    )
    counts = simulate.simulate(
        flooded, simulate.Behavior(), 20, hourly_index(7), np.random.default_rng(1)
    )
    assert counts.max() == 20


def test_vacations_and_sundays_are_quieter_than_class_days():
    profile = site_calendar.default_profile(100)
    index = pd.date_range("2026-01-01", "2026-04-30", freq="h", tz="UTC")
    counts = simulate.simulate(
        profile, simulate.Behavior(), 100, index, np.random.default_rng(3)
    )
    calendar = site_calendar.calendar_frame(profile, index)
    weekday = calendar["weekday"].lt(5) & calendar["holiday"].eq(0)
    classes = counts[(calendar["period"] == 1) & weekday].mean()
    vacation = counts[(calendar["period"] == 0) & weekday].mean()
    sunday = counts[calendar["weekday"] == 6].mean()
    assert classes > 2 * vacation > 0
    assert sunday < vacation
