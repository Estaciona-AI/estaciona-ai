# 5. Methodology

## 1. Evidence standard

Claims in this repository are classified by the strongest evidence that supports
them:

| Class | Meaning | Example |
|---|---|---|
| Code | Behaviour read in the source at a stated commit | The server refuses a superuser database role in production |
| Test | Behaviour reproduced by an automated test | A replayed event is applied once |
| Measurement | Quantity obtained on named hardware with a recorded protocol | Minimum per-input FPS at 10 inputs |
| Assumption | Plausible but not yet verified | Field cameras behave like the laboratory streams |

Rules: a passing linter or test suite is not evidence of capacity, security or
production readiness; a measurement on a proxy is not a measurement on the
target; a missing scanner finding is not proof of absence. Results are reported
with their limitations, including negative results.

## 2. Vision capacity and quality

**Laboratory.** `tools/rtsp-lab/` starts pinned MediaMTX and FFmpeg containers
that publish synthetic RTSP scenarios of 20 to 35 inputs mapping exactly 400
spot identifiers. Scenarios are derived from a single recorded video, which
limits their visual diversity.

**Capacity ramps.** Benchmarks in `vision/src/benchmark*.py` increase the number
of inputs step by step and record per-input frame rate, processor time and
resident memory, together with provenance (model digest, runtime and
hardware). Reports use `schema_version: 2`. The acceptance criterion for RQ-1 is
the *minimum* per-input rate, not the mean, because the slowest input bounds the
freshness of its spots.

To reproduce a ramp, first validate the runtime configuration, start the
laboratory publishers, and record the source video and mapping digests, input
IDs, model provenance, calibration, execution provider and hardware. From
`vision/`, an example protocol is:

```sh
uv run --locked python src/runtime.py CONFIG --validate
uv run --locked python src/benchmark.py capacity CONFIG --output NEW_RESULTS \
  --counts 1,5,10,20 --seconds 60 --warmup 10 --delivery shadow
```

`CONFIG` and `NEW_RESULTS` are placeholders; the results directory must be new
and outside Git. These durations are example settings, not a reported
experiment or a soak test. Record warm-up and measured duration for each run;
do not compare a cold start with a warmed model or claim independent scenes
from replicas of the same video. A simultaneous model comparison measures
contention; use sequential runs for an isolated comparison.

Keep the complete `capacity.json`, per-input metrics and resource samples.
Report incomplete runs and restarts, minimum FPS, frame age, delivery backlog,
CPU, RAM and GPU resources. The tool checks each input's configured FPS,
frame-age p95 upper bound at most 2 s and, in gateway mode, an empty pending
queue ([capacity implementation](../vision/src/benchmark_capacity.py)). RQ-1
additionally requires the stated target hardware and 20 inputs at 2.5 FPS;
a report that passes a lower configured target does not meet RQ-1.

| Delivery mode | What the capacity report observes | What it does not establish |
|---|---|---|
| `shadow` | Vision processing without delivery | Gateway or backend persistence |
| `gateway` | Vision through the local gateway's persistence ACK | Remote server commit or total gateway and publisher resource budget |

A complete-chain experiment also checks receiver receipts, expected IDs,
replay after interruption and drain time at the server. A local ACK alone does
not establish completion of that experiment (F-04). Keep the backend's location
and the processes included in each resource budget explicit, without publishing
deployment addresses.

**Occupancy evaluation.** The corpus tooling freezes source frame indices,
hashes, mapping and sampling cadence. A reviewer labels each spot as
`occupied`, `free` or `uncertain`; generated drafts start uncertain and must be
reviewed before scoring. Use the same video, input, warm-up, frame count and
stride in `corpus` and `compare`. The evaluator refuses mismatched artifacts
([corpus](../vision/src/benchmark_corpus.py),
[comparison](../vision/src/benchmark_comparison.py),
[evaluation](../vision/src/benchmark_evaluation.py)).

Report occupied spots missed, false occupied spots, precision, recall and
accuracy on scored labels, together with the number of uncertain and scored
labels. An undefined metric is unavailable, not zero. Include small vehicles,
shadows and manoeuvres in the sampling plan; retain difficult frames rather
than selecting only favourable examples. Freeze calibration before the final
evaluation and keep calibration frames separate from evaluation frames.
Agreement with another detector, including the earlier reference (F-07), is
reported as agreement, not ground-truth accuracy. Repeated frames of one scene
do not establish generalization to field cameras. Evaluation images and
artifacts remain private and outside Git.

## 3. Forecast evaluation

**Target.** The observed physical occupancy rate for each of the next 24 hours,
issued hourly. For an eligible historical hour, the training target is occupied
spots divided by observed spots. Predicted rates are clipped to [0, 1] and
scaled to the current active capacity for the published counts
([history](../gateway/ml/history.py), [inference](../gateway/ml/inference.py)).

**Data quality.** An hour counts only when at least 90% of the spots of the site
map in force at that hour reported. The historical whole-site count remains
unavailable under partial coverage, even when the observed rate is usable.
At this revision, inference requires at least one valid hour in the previous
24 hours and at least 84 in the previous 168; it does not require a contiguous
week. Insufficient recent history prevents inference. A simulated training
prior therefore does not make an unobserved site eligible for a forecast
([admission](../gateway/ml/train.py)).

**Baselines.** A seasonal naive forecast (the same hours one week earlier) and a
weekly profile (the mean of the same hours over the previous four weeks)
[[6]](references.md).

**Acceptance gate.** Training attempts the gate after at least 672 valid real
hours have accrued. Evaluation also needs at least 168 valid training hours
before the final 28-day holdout and eligible origins with complete forecast
horizons; four calendar weeks alone do not ensure an evaluated gate. The
candidate is refitted excluding holdout targets and evaluated at six-hour
rolling origins [[7]](references.md). Normalized mean absolute error, root mean
squared error and bias, in percentage points of the rate, use the common finite
points forecast by every method. Report that point count as well as the scores.

The serving choice minimizes the reported normalized MAE among the model and
baselines; a tie can retain the model. If evaluation is unavailable, the
manifest records `not_evaluated` and the model remains the default serving
choice, subject to inference admission. The final model is fitted using the
available training history. The gate is a selection rule on a historical
sample, not proof of future superiority
([training](../gateway/ml/train.py), [backtest](../gateway/ml/backtest.py)).

**Prospective scoring.** The worker records the first forecast issued for an
origin in a local ledger; a restart cannot rewrite that forecast. After its
24-hour horizon, it is scored against the later observations. A missing issued
forecast or invalid observation window yields unavailable evaluation, not zero
error. Reconstructing a past forecast with today's model is not prospective
evidence. Record the artifact digest, serving method, history coverage and
evaluation status alongside each result ([ledger and scoring](../gateway/ml/inference.py)).

**Simulation.** The campus simulator samples plausible sites around the
configured profile, censors demand at capacity and adds detection noise and
camera outages. Simulated data is used only as a training prior and for offline
checks; it is never stored or reported as an observation.

## 4. Engineering method

**Change control.** Work starts from a written plan that names the objective,
the files in scope, the contracts affected, the acceptance criteria and the
tests. Changes outside the approved scope require a revised plan.

**Contracts first.** Wire changes start from the Rust types that generate the
JSON Schemas; consumer tests validate real responses and frames against the
published schemas, and continuous integration rejects an incompatible change
within one major version.

**Test design.** Tests assert observable invariants (state, persisted rows,
emitted frames, refusal codes) rather than implementation details, including
negative cases: other users, other roles, other installations, expired or
revoked sessions, missing or short secrets and disallowed origins.

**Test adequacy.** Mutation testing [[8, 9]](references.md) audits whether tests
detect deliberate faults: `mutmut` for Python and `cargo-mutants` for Rust.
Each surviving mutant is classified as a real gap, which receives a test, or as
equivalent, which is recorded with its reason. Mutation runs are scoped per
module when a full suite per mutant would be prohibitively long.

**Releases.** Versions follow Semantic Versioning from `v0.9.0`; changes are
recorded in `CHANGELOG.md` following Keep a Changelog [[10, 11]](references.md).
