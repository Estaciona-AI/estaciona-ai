# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Synthetic occupancy used only as a training prior, never as an observation."""

import heapq
from dataclasses import dataclass, replace

import numpy as np
import pandas as pd

if __package__:
    from .site_calendar import EXAMS, VACATION, DayCalendar, Shift
else:
    from site_calendar import EXAMS, VACATION, DayCalendar, Shift


@dataclass(frozen=True)
class Behavior:
    lead_min: float = 15.0
    spread_min: float = 12.0
    early_leave: float = 0.15
    day_sigma: float = 0.10
    semester_decay: float = 0.15
    vacation_scale: float = 0.12
    exam_scale: float = 1.05
    sunday_scale: float = 0.02


def randomize(profile, rng):
    """Sample a plausible campus around the facts the profile states."""
    spread = profile.extra_uncertainty

    def jitter(value, sigma):
        return value * float(np.exp(rng.normal(0, sigma * spread)))

    shifts = []
    for shift in profile.shifts:
        level = jitter(1.0, 0.25)
        start = shift.start + rng.normal(0, 0.25)
        end = max(start + 0.5, shift.end + rng.normal(0, 0.25))
        shape = rng.normal(1, 0.08, 6).clip(0.5)
        shifts.append(
            Shift(start, end, tuple(c * level * s for c, s in zip(shift.cars, shape)))
        )
    sampled = replace(
        profile,
        shifts=tuple(shifts),
        staff=jitter(profile.staff, 0.3),
        visitors_per_hour=jitter(profile.visitors_per_hour, 0.4),
    )
    behavior = Behavior(
        lead_min=rng.uniform(5, 30),
        spread_min=rng.uniform(6, 25),
        early_leave=rng.uniform(0.05, 0.3),
        day_sigma=rng.uniform(0.05, 0.2),
        semester_decay=rng.uniform(0.0, 0.3),
        vacation_scale=rng.uniform(0.05, 0.25),
        exam_scale=rng.uniform(0.9, 1.2),
    )
    return sampled, behavior


def _day_cars(profile, behavior, day, calendar, rng):
    weekday = day.weekday()
    period, progress, holiday = calendar.day(day)
    if weekday == 6 or holiday:
        scale = behavior.sunday_scale
    else:
        scale = {VACATION: behavior.vacation_scale, EXAMS: behavior.exam_scale}.get(
            period, 1.0
        )
        scale *= 1 - behavior.semester_decay * progress
    scale *= float(np.exp(rng.normal(0, behavior.day_sigma)))
    arrivals, departures = [], []
    if weekday == 6:
        return arrivals, departures
    for shift in profile.shifts:
        n = rng.poisson(shift.cars[weekday] * scale)
        start = shift.start * 60 - rng.normal(behavior.lead_min, behavior.spread_min, n)
        end = shift.end * 60 + rng.normal(0, behavior.spread_min, n)
        early = rng.random(n) < behavior.early_leave
        end[early] = start[early] + rng.uniform(
            45, (shift.end - shift.start) * 60, early.sum()
        )
        arrivals.append(start)
        departures.append(end)
    staff_scale = 0.0 if holiday else (0.3 if weekday == 5 else 1.0)
    n = rng.poisson(profile.staff * staff_scale)
    start = rng.normal(7.6 * 60, 30, n)
    arrivals.append(start)
    departures.append(start + rng.normal(9 * 60, 45, n))
    hours = np.arange(8, 18)
    counts = rng.poisson(profile.visitors_per_hour * scale, len(hours))
    start = np.repeat(hours * 60, counts) + rng.uniform(0, 60, counts.sum())
    arrivals.append(start)
    departures.append(start + rng.exponential(60, counts.sum()) + 10)
    return arrivals, departures


def simulate(profile, behavior, capacity, index, rng):
    """Last-observed occupied count per UTC hour, censored at capacity."""
    local = pd.DatetimeIndex(index).tz_convert(profile.timezone).tz_localize(None)
    first = local.min().normalize() - pd.Timedelta(days=1)
    days = (local.max().normalize() - first).days + 2
    diff = np.zeros((days + 2) * 1440, dtype=np.int64)
    calendar = DayCalendar(profile)
    parked = []  # min-heap of departure minutes
    for offset in range(days):
        day = (first + pd.Timedelta(days=offset)).date()
        arrivals, departures = _day_cars(profile, behavior, day, calendar, rng)
        if not arrivals:
            continue
        start = np.concatenate(arrivals) + offset * 1440
        end = np.maximum(np.concatenate(departures) + offset * 1440, start + 10)
        order = np.argsort(start)
        for arrive, leave in zip(start[order].astype(int), end[order].astype(int)):
            arrive = max(arrive, 0)
            while parked and parked[0] <= arrive:
                heapq.heappop(parked)
            if len(parked) >= capacity:
                continue  # Lot full: this demand is never observed.
            heapq.heappush(parked, leave)
            diff[arrive] += 1
            diff[min(leave, len(diff) - 1)] -= 1
    occupied = np.cumsum(diff)
    minutes = ((local - first) // pd.Timedelta(minutes=1)).to_numpy() + 59
    return occupied[minutes].astype(np.float64)


def observe(counts, capacity, rng, missing_rate=0.03, outage_hours=3.0):
    """Detection noise plus coverage outages, so the model learns gaps."""
    occupied = counts.astype(np.int64)
    noisy = (
        occupied
        - rng.binomial(occupied, 0.01)
        + rng.binomial(capacity - occupied, 0.004)
    )
    noisy = noisy.astype(np.float64)
    outages = rng.poisson(missing_rate * len(noisy) / outage_hours)
    for start, length in zip(
        rng.integers(0, len(noisy), outages), rng.geometric(1 / outage_hours, outages)
    ):
        noisy[start : start + length] = np.nan
    return noisy


def synthetic_rates(profile, capacity, index, variants, seed):
    """Occupancy rates of randomized campuses sharing the site's calendar."""
    rng = np.random.default_rng(seed)
    rates = []
    for _ in range(variants):
        sampled, behavior = randomize(profile, rng)
        counts = simulate(sampled, behavior, capacity, index, rng)
        rates.append(observe(counts, capacity, rng) / capacity)
    return rates
