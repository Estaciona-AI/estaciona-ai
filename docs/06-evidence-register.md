# 6. Evidence register

Status as of `develop` at `e8a1a7b` (2026-11-01). Classes follow
[Methodology §1](05-methodology.md#1-evidence-standard).

## 6.1 Verified facts

| ID | Fact | Class | Source |
|---|---|---|---|
| F-01 | `v0.9.0` is the first non-pre-release build, published on 2026-10-23 as a pilot release. | Code | Tag `v0.9.0`; `CHANGELOG.md` |
| F-02 | Each installation runs its own server and PostgreSQL database, bound to explicit organization, installation and environment identifiers. | Code, test | `server/src/installation/`; isolation tests (#133–#135) |
| F-03 | The published contract bundle in `v0.10.0` is version 2.0.0, under the HTTP prefix `/api/v1`; `v0.11.0` publishes 2.1.0, which only adds to it. | Code | `shared/v1/manifest.json` |
| F-04 | Persisted supported events keep one UUID across retries and are acknowledged only after the receiver's commit, at both delivery hops. A local persistence ACK is distinct from a server receipt. | Code, test | `vision/src/delivery.py`, `gateway/gateway.py`, `server/src/edge_delivery.rs`; [delivery semantics](02-architecture.md#22-data-flows) |
| F-05 | In a transport-only staging test with 400 synthetic spot IDs and 20 cameras, an in-flight window of eight raised confirmation throughput from 6.1 to 15.7 events/s and lowered p95 latency from 91.9 s to 0.68 s. | Measurement | #154 |
| F-06 | With spot-only tiles and TensorRT FP16 RT-DETR-R18, the minimum per-input rate at 10 inputs rose from 0.53 to 2.48 FPS on a desktop RTX 5060 Ti used as a proxy; resident memory per process fell from 3.7 GB to 1.35 GB. | Measurement | #152, #153, #155, #156 |
| F-07 | On the laboratory corpus, detections agreed with a larger reference detector in 924 of 924 comparisons. | Measurement | `CHANGELOG.md` `v0.9.0` |
| F-08 | The installation database stores no e-mail address or password hash; sign-in is delegated to the identity provider. | Code | `CHANGELOG.md` `v0.10.0`; `server/src/identity/` |
| F-09 | Personal fields kept by the server are encrypted at rest with AES-256-GCM under keys read from a file the database never receives. | Code, test | `server/src/personal_data/` |
| F-10 | Historical local snapshot at `78a14eb`: server 390 passed against a disposable database; vision 407 passed; gateway 179 passed and 170 integration tests skipped without a running server. Superseded as the current verification snapshot by F-11; the skipped tests were not local passes. | Test | Initial documentation in #218; [Testing §7.4](07-testing.md#74-verification-snapshot) |
| F-11 | At `9036d10` on 2026-10-27, CI passed: server 390; vision 407; gateway and forecast 383, including required server integration; contracts 36; identity tools 17; RTSP laboratory 1. Superseded as the current verification snapshot by F-12. | Test | [CI run 40567080230](https://github.com/Estaciona-AI/estaciona-ai/actions/runs/40567080230) |
| F-12 | At `7bf6c6e` on 2026-10-28, CI passed: server 407; vision 407; gateway and forecast 395, including required server integration; contracts 36; identity tools 17; RTSP laboratory 1. | Test | [CI run 40738777401](https://github.com/Estaciona-AI/estaciona-ai/actions/runs/40738777401); [conditions](07-testing.md#74-verification-snapshot) |
| F-13 | `v0.10.0` was published on 2026-11-01 as a pilot release, with a signed tag on `c25db55`. CI passed at that commit; since F-12 the code changed only in component version fields, so F-12 remains the current verification snapshot. | Code, test | Tag `v0.10.0`; #229, #230; [CI run 41262972618](https://github.com/Estaciona-AI/estaciona-ai/actions/runs/41262972618) |
| F-14 | On 2026-11-14, at the revision of #242, a walk-forward comparison with the production training configuration (35 days, hourly origins admitted by the 84-hour rule, daily refits, 733 origins) gave, in points of the rate: on a 44-spot history built from the two states (43 and 44 occupied) that a 69-second demonstration recording yields when replayed in a loop, 2.6 to 3.0 normalized MAE for the model against 1.1 for persistence and 1.1 for the mean of the history; on a simulated campus of the family of the training prior, 6.9 for the model against 28.6 for persistence and, where every baseline was available, 6.3 against 8.8 for the best baseline. Per day of origin, the model's error exceeded that of persistence by 1.4 to 1.8 points on the replayed history and was lower on 31 of 32 days on the simulated campus. Both histories were constructed; neither is a field observation. | Measurement on constructed histories | #242; scripts, recording and results are kept outside the repository; [method](05-methodology.md#3-forecast-evaluation) |

## 6.2 Assumptions

| ID | Assumption | Risk if false | Validation plan |
|---|---|---|---|
| A-01 | Field cameras resemble the laboratory streams in viewpoint, resolution and lighting. | Lower detection quality and capacity than measured. | Pilot with real cameras (G-03). |
| A-02 | The target datacenter GPU sustains at least the throughput measured on the desktop proxy. | RQ-1 not met; more hosts per site. | 20-input capacity ramp on the target GPU (G-02). |
| A-03 | Agreement with the reference detector approximates spot-occupancy accuracy. | Unknown error rate in production. | Labelled ground truth and an accuracy metric (G-04). |
| A-04 | Cameras are fixed, so spot polygons remain valid after calibration. | Silent misattribution of occupancy. | Periodic inspection with the local inspector; recalibration procedure. |
| A-05 | The simulated campus prior improves early forecasts once recent observed history permits inference. This remains unverified at a real site. | Poor early forecasts despite a valid artifact. | Prospective scoring and a backtest with sufficient pre-holdout history and comparable points (G-05); simulation alone does not validate this assumption. It did not hold on a history shaped like a looped recording (F-14). |
| A-06 | Outbox bounds and retention defaults absorb the longest expected link outage at a site. | Producers pause capture during long outages. | Outage drills during the pilot. |

## 6.3 Open questions

| ID | Question | Decided by |
|---|---|---|
| Q-01 | Should the TensorRT FP16 R18 profile become the default detector profile? | Outcome of the 20-input test (RQ-1) |
| Q-02 | How long does TensorRT engine generation take on the target host? | Measurement on the target hardware |
| Q-03 | Which occupancy accuracy metric and labelling protocol define acceptance for production? | Pilot planning (G-04) |
| Q-04 | Which criteria move an installation from pilot to production? | Goals review after the pilot (G-07) |
| Q-05 | Which method should be served before the gate can evaluate the model at a site? The model remains the default: persistence would have been better on a looped recording and far worse on a simulated campus (F-14). | Prospective comparison at the first site with field history (G-05) |

## 6.4 Known limitations

The current limitations are listed in `CHANGELOG.md` under *Known limitations*
of the latest release; the items that are goals are tracked in
[Goals and roadmap](08-goals-and-roadmap.md).
