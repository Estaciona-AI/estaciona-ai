# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Installation profile: local time, demand blocks, academic periods, holidays."""

import hashlib
import json
import os
from dataclasses import asdict, dataclass, field
from datetime import date, timedelta
from zoneinfo import ZoneInfo

import numpy as np
import pandas as pd

VACATION, CLASSES, EXAMS = 0, 1, 2
PERIOD_KINDS = {"classes": CLASSES, "exams": EXAMS}


@dataclass(frozen=True)
class Shift:
    start: float  # local decimal hour
    end: float
    cars: tuple  # expected arrivals, Monday..Saturday


@dataclass(frozen=True)
class Profile:
    timezone: str
    shifts: tuple
    staff: float
    visitors_per_hour: float
    periods: tuple = ()  # (start, end, kind); empty means generic semesters
    holidays: frozenset = frozenset()  # added to national holidays
    specified: bool = False  # False when built from defaults, not site facts
    extra_uncertainty: float = field(default=1.0)

    def digest(self):
        payload = asdict(self)
        payload["holidays"] = sorted(str(day) for day in self.holidays)
        text = json.dumps(payload, sort_keys=True, default=str)
        return hashlib.sha256(text.encode()).hexdigest()


def default_profile(capacity):
    """Generic campus with day and evening classes; wide uncertainty."""
    weekdays = (1.0, 1.0, 1.0, 0.95, 0.8, 0.0)
    return Profile(
        timezone="America/Sao_Paulo",
        shifts=(
            Shift(
                7.0,
                11.7,
                tuple(0.75 * capacity * w for w in weekdays[:5]) + (0.3 * capacity,),
            ),
            Shift(13.0, 17.5, tuple(0.35 * capacity * w for w in weekdays)),
            Shift(19.0, 22.7, tuple(1.1 * capacity * w for w in weekdays)),
        ),
        staff=0.12 * capacity,
        visitors_per_hour=0.02 * capacity,
        extra_uncertainty=2.0,
    )


def _hour(text, name):
    try:
        hours, minutes = (int(part) for part in str(text).split(":"))
    except ValueError:
        raise ValueError(f"{name} must be HH:MM") from None
    if not (0 <= hours <= 23 and 0 <= minutes <= 59):
        raise ValueError(f"{name} must be HH:MM")
    return hours + minutes / 60


def _number(value, name):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name} must be a number")
    if not np.isfinite(value) or value < 0:
        raise ValueError(f"{name} must be finite and non-negative")
    return float(value)


def load_profile(path, capacity):
    """Parse the site profile JSON; missing fields fall back to defaults."""
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
    if not isinstance(data, dict):
        raise ValueError("forecast profile must be a JSON object")
    unknown = set(data) - {
        "timezone",
        "shifts",
        "staff",
        "visitors_per_hour",
        "periods",
        "holidays",
    }
    if unknown:
        raise ValueError(f"unknown forecast profile fields: {sorted(unknown)}")
    base = default_profile(capacity)
    timezone = data.get("timezone", base.timezone)
    try:
        ZoneInfo(timezone)
    except Exception:
        raise ValueError("timezone must be an IANA zone") from None

    shifts = base.shifts
    if "shifts" in data:
        shifts = []
        for index, item in enumerate(data["shifts"]):
            start, end = (
                _hour(item["start"], "shift start"),
                _hour(item["end"], "shift end"),
            )
            if end <= start:
                raise ValueError(f"shift {index} must end after it starts")
            cars = item["cars"]
            if not isinstance(cars, list) or len(cars) != 6:
                raise ValueError(f"shift {index} cars must list Monday..Saturday")
            shifts.append(Shift(start, end, tuple(_number(c, "cars") for c in cars)))
        shifts = tuple(shifts)

    periods = []
    for item in data.get("periods", []):
        start, end = date.fromisoformat(item["start"]), date.fromisoformat(item["end"])
        if end < start or item.get("kind") not in PERIOD_KINDS:
            raise ValueError("periods need start <= end and kind classes|exams")
        periods.append((start, end, PERIOD_KINDS[item["kind"]]))
    periods.sort()
    if any(a[1] >= b[0] for a, b in zip(periods, periods[1:])):
        raise ValueError("periods must not overlap")

    return Profile(
        timezone=timezone,
        shifts=shifts,
        staff=_number(data.get("staff", base.staff), "staff"),
        visitors_per_hour=_number(
            data.get("visitors_per_hour", base.visitors_per_hour), "visitors_per_hour"
        ),
        periods=tuple(periods),
        holidays=frozenset(date.fromisoformat(day) for day in data.get("holidays", [])),
        specified=True,
        extra_uncertainty=1.0,
    )


def easter(year):
    # Anonymous Gregorian algorithm.
    a, b, c = year % 19, year // 100, year % 100
    d, e = divmod(b, 4)
    g = (8 * b + 13) // 25
    h = (19 * a + b - d - g + 15) % 30
    i, k = divmod(c, 4)
    l = (32 + 2 * e + 2 * i - h - k) % 7
    m = (a + 11 * h + 19 * l) // 433
    month = (h + l - 7 * m + 90) // 25
    return date(year, month, (h + l - 7 * m + 33 * month + 19) % 32)


def national_holidays(year):
    sunday = easter(year)
    fixed = [
        (1, 1),
        (4, 21),
        (5, 1),
        (9, 7),
        (10, 12),
        (11, 2),
        (11, 15),
        (11, 20),
        (12, 25),
    ]
    movable = [-48, -47, -2, 60]  # Carnival Monday/Tuesday, Good Friday, Corpus Christi
    return {date(year, m, d) for m, d in fixed} | {
        sunday + timedelta(days=n) for n in movable
    }


def generic_periods(year):
    return [
        (date(year, 2, 5), date(year, 6, 14), CLASSES),
        (date(year, 6, 15), date(year, 6, 28), EXAMS),
        (date(year, 8, 1), date(year, 11, 29), CLASSES),
        (date(year, 11, 30), date(year, 12, 13), EXAMS),
    ]


class DayCalendar:
    """Per-day period, semester progress and holiday flags for a profile."""

    def __init__(self, profile):
        self.profile = profile
        self._cache = {}

    def _periods(self, year):
        if self.profile.periods:
            return list(self.profile.periods)
        return generic_periods(year)

    def day(self, day):
        if day not in self._cache:
            periods = self._periods(day.year)
            period = next((k for s, e, k in periods if s <= day <= e), VACATION)
            progress = 0.0
            for first, last in _terms(periods):
                if first <= day <= last:
                    progress = (day - first).days / max((last - first).days, 1)
            holiday = day in self.profile.holidays or day in national_holidays(day.year)
            self._cache[day] = (period, progress, holiday)
        return self._cache[day]


def _terms(periods):
    """Merge contiguous class and exam periods into academic terms."""
    terms = []
    for start, end, _ in sorted(periods):
        if terms and start <= terms[-1][1] + timedelta(days=1):
            terms[-1][1] = max(terms[-1][1], end)
        else:
            terms.append([start, end])
    return terms


def calendar_frame(profile, index):
    """Calendar features for a UTC hourly index, in the site's local time."""
    local = pd.DatetimeIndex(index).tz_convert(profile.timezone)
    days = DayCalendar(profile)
    dates = local.normalize()
    lookup = {d: days.day(d.date()) for d in dates.unique()}
    rows = np.array([lookup[d] for d in dates], dtype=np.float64).reshape(-1, 3)
    return pd.DataFrame(
        {
            "hour": local.hour.to_numpy(),
            "weekday": local.weekday.to_numpy(),
            "period": rows[:, 0].astype(int),
            "sem_progress": rows[:, 1],
            "holiday": rows[:, 2].astype(int),
        }
    )


def profile_from_env(capacity):
    """Site profile from FORECAST_PROFILE, or the generic campus when unset."""
    path = os.environ.get("FORECAST_PROFILE")
    return load_profile(path, capacity) if path else default_profile(capacity)
