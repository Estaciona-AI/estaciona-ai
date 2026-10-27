# 6. Evidence register

Status as of `develop` at `78a14eb` (2026-10-27). Classes follow
[Methodology §1](05-methodology.md#1-evidence-standard).

## 6.1 Verified facts

| ID | Fact | Class | Source |
|---|---|---|---|
| F-01 | `v0.9.0` is the first non-pre-release build, published on 2026-10-23 as a pilot release. | Code | Tag `v0.9.0`; `CHANGELOG.md` |
| F-02 | Each installation runs its own server and PostgreSQL database, bound to explicit organization, installation and environment identifiers. | Code, test | `server/src/installation/`; isolation tests (#133–#135) |
| F-03 | The published contract bundle on `develop` is version 2.0.0, under the HTTP prefix `/api/v1`. | Code | `shared/v1/manifest.json` |
| F-04 | Events keep one UUID across retries and are acknowledged only after the receiver's commit, at both delivery hops. | Code, test | `vision/src/delivery.py`, `gateway/gateway.py`, `server/src/edge_delivery.rs` |
| F-05 | In a transport-only staging test with 400 synthetic spot IDs and 20 cameras, an in-flight window of eight raised confirmation throughput from 6.1 to 15.7 events/s and lowered p95 latency from 91.9 s to 0.68 s. | Measurement | #154 |
| F-06 | With spot-only tiles and TensorRT FP16 RT-DETR-R18, the minimum per-input rate at 10 inputs rose from 0.53 to 2.48 FPS on a desktop RTX 5060 Ti used as a proxy; resident memory per process fell from 3.7 GB to 1.35 GB. | Measurement | #152, #153, #155, #156 |
| F-07 | On the laboratory corpus, detections agreed with a larger reference detector in 924 of 924 comparisons. | Measurement | `CHANGELOG.md` `v0.9.0` |
| F-08 | The installation database stores no e-mail address or password hash; sign-in is delegated to the identity provider. | Code | `CHANGELOG.md` *Unreleased*; `server/src/identity/` |
| F-09 | Personal fields kept by the server are encrypted at rest with AES-256-GCM under keys read from a file the database never receives. | Code, test | `server/src/personal_data/` |
| F-10 | At `78a14eb`, the local automated suites pass: server 390 tests against a disposable test database; vision 407 tests; gateway 179 tests, with its 170 integration tests skipped for lack of a running server. | Test | [Testing §7.4](07-testing.md#74-verification-snapshot) |

## 6.2 Assumptions

| ID | Assumption | Risk if false | Validation plan |
|---|---|---|---|
| A-01 | Field cameras resemble the laboratory streams in viewpoint, resolution and lighting. | Lower detection quality and capacity than measured. | Pilot with real cameras (G-03). |
| A-02 | The target datacenter GPU sustains at least the throughput measured on the desktop proxy. | RQ-1 not met; more hosts per site. | 20-input capacity ramp on the target GPU (G-02). |
| A-03 | Agreement with the reference detector approximates spot-occupancy accuracy. | Unknown error rate in production. | Labelled ground truth and an accuracy metric (G-04). |
| A-04 | Cameras are fixed, so spot polygons remain valid after calibration. | Silent misattribution of occupancy. | Periodic inspection with the local inspector; recalibration procedure. |
| A-05 | The simulated campus prior gives forecasts no worse than the naive baselines before real history accrues. | Poor early forecasts. | Prospective scoring and the acceptance gate once four weeks of real history exist (G-05). |
| A-06 | Outbox bounds and retention defaults absorb the longest expected link outage at a site. | Producers pause capture during long outages. | Outage drills during the pilot. |

## 6.3 Open questions

| ID | Question | Decided by |
|---|---|---|
| Q-01 | Should the TensorRT FP16 R18 profile become the default detector profile? | Outcome of the 20-input test (RQ-1) |
| Q-02 | How long does TensorRT engine generation take on the target host? | Measurement on the target hardware |
| Q-03 | Which occupancy accuracy metric and labelling protocol define acceptance for production? | Pilot planning (G-04) |
| Q-04 | Which criteria move an installation from pilot to production? | Goals review after the pilot (G-07) |

## 6.4 Known limitations

The current limitations are listed in `CHANGELOG.md` under *Known limitations*
of the latest release; the items that are goals are tracked in
[Goals and roadmap](08-goals-and-roadmap.md).
