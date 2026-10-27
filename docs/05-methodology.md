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

**Occupancy agreement.** Frame corpora are evaluated against a reference
detector. Agreement with a reference is reported as agreement, not as accuracy,
until labelled ground truth exists.

## 3. Forecast evaluation

**Target.** The occupancy rate (occupied spots divided by capacity) for each of
the next 24 hours, issued hourly.

**Data quality.** An hour counts only when at least 90% of the spots of the site
map in force at that hour reported; inference needs 168 contiguous valid hours.

**Baselines.** A seasonal naive forecast (the same hours one week earlier) and a
weekly profile (the mean of the same hours over the previous four weeks) [6].

**Acceptance gate.** With at least four weeks of real history, the candidate is
refitted without the last 28 days and evaluated by rolling-origin backtesting
over that holdout [7]. Normalized mean absolute error, root mean squared error
and bias, in percentage points of the rate, are computed on the points every
method could forecast. The model is served only if it beats every baseline;
otherwise the best baseline is served (`gateway/ml/backtest.py`,
`gateway/ml/train.py`).

**Prospective scoring.** Each forecast is recorded when issued and scored against
what was later observed, so reported quality is out of sample; until a prior
forecast exists, the evaluation is reported as unavailable rather than as zero
error.

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

**Test adequacy.** Mutation testing [8, 9] is used to audit whether the tests
detect deliberate faults: `mutmut` for Python and `cargo-mutants` for Rust.
Each surviving mutant is classified as a real gap, which receives a test, or as
equivalent, which is recorded with its reason. Mutation runs are scoped per
module when a full suite per mutant would be prohibitively long.

**Releases.** Versions follow Semantic Versioning from `v0.9.0`; changes are
recorded in `CHANGELOG.md` following Keep a Changelog [10, 11].
