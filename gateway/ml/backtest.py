# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Rolling-origin evaluation of occupancy-rate forecasts against naive baselines."""

import json
import warnings

import numpy as np
import pandas as pd

HORIZON = 24
WEEK = 168
BASELINES = ("seasonal_naive", "weekly_profile")


def seasonal_naive(rate, origins):
    return np.stack([rate[o - WEEK : o - WEEK + HORIZON] for o in origins])


def weekly_profile(rate, origins, weeks=4):
    windows = np.stack(
        [
            np.stack(
                [
                    rate[o - WEEK * k : o - WEEK * k + HORIZON]
                    for k in range(1, weeks + 1)
                ]
            )
            for o in origins
        ]
    )
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", RuntimeWarning)  # all-missing hours stay NaN
        return np.nanmean(windows, axis=1)


def actual_windows(rate, origins):
    return np.stack([rate[o : o + HORIZON] for o in origins])


def compare(actual, predictions):
    """Error of each method on the points every method could forecast."""
    mask = np.isfinite(actual)
    for values in predictions.values():
        mask &= np.isfinite(values)
    scores = {"points": int(mask.sum())}
    if not mask.any():
        return scores
    for name, values in predictions.items():
        error = values[mask] - actual[mask]
        scores[name] = {
            "nmae_pct": round(float(np.abs(error).mean() * 100), 3),
            "rmse_pct": round(float(np.sqrt((error**2).mean()) * 100), 3),
            "bias_pct": round(float(error.mean() * 100), 3),
        }
    return scores


def choose_serving(scores):
    """Serve the model only when it beats every baseline on real data."""
    ranked = [
        name for name in ("model",) + BASELINES if isinstance(scores.get(name), dict)
    ]
    if not ranked:
        return "model"
    return min(ranked, key=lambda name: scores[name]["nmae_pct"])


def origins_between(index, start, end, step=1):
    """Origins with a complete 24 h horizon and four weeks of lags in the index."""
    positions = np.arange(len(index))
    mask = (index >= start) & (index < end)
    origins = positions[mask][::step]
    return origins[(origins >= 4 * WEEK) & (origins + HORIZON <= len(index))]


def acceptance(seed=11, variants=16):
    """Offline check on a hidden simulated campus; not evidence for a real site."""
    if __package__:
        from . import train
        from .simulate import observe, randomize, simulate
        from .site_calendar import default_profile
    else:
        import train
        from simulate import observe, randomize, simulate
        from site_calendar import default_profile

    capacity = 400
    profile = default_profile(capacity)
    index = pd.date_range("2024-01-01", periods=730 * 24, freq="h", tz="UTC")
    rng = np.random.default_rng(seed)
    truth = simulate(*randomize(profile, rng), capacity, index, rng)
    split = pd.Timestamp("2025-07-01", tz="UTC")
    local_hour = index.tz_convert(profile.timezone).hour
    test = origins_between(index, split, index[-1])
    test = test[local_hour[test] == 6]
    sims = train.synthetic_training_rates(profile, capacity, index, variants)
    report = {}
    for missing in (0.0, 0.07):
        observed = observe(truth, capacity, np.random.default_rng(seed + 1), missing)
        rate = observed / capacity
        known = rate.copy()
        known[index >= split] = np.nan
        model = train.fit_model(profile, index, known, sims)
        predicted = train.predict_rates(
            model, rate, train.calendar_for(profile, index), test
        )
        truth_rate = truth / capacity
        scores = compare(
            actual_windows(truth_rate, test),
            {
                "model": predicted,
                "seasonal_naive": seasonal_naive(rate, test),
                "weekly_profile": weekly_profile(rate, test),
            },
        )
        scores["missing_hours_pct"] = round(float(np.isnan(rate).mean() * 100), 1)
        served = [train.has_recent_history(rate, origin) for origin in test]
        scores["days_with_forecast_pct"] = round(float(np.mean(served) * 100), 1)
        report[f"missing_{missing}"] = scores
    return report


if __name__ == "__main__":
    print(json.dumps(acceptance(), indent=2))
