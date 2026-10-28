# 4. Research

## 4.1 Research questions

| ID | Question | Status (2026-10-27) | Evidence |
|---|---|---|---|
| RQ-1 | Can one GPU host process 20 inputs of 20 spots each at no less than 2.5 frames per second per input? | Open. Ten inputs reached a minimum of 2.48 FPS per input on a desktop proxy GPU; the 20-input test on the target datacenter GPU is pending. | Measurement, `CHANGELOG.md` `v0.9.0` *Performance* (#152, #153, #155, #156) |
| RQ-2 | Does detector-derived spot occupancy agree with a reference? | Partially answered. On the laboratory corpus, detections agreed with a larger reference detector in 924 of 924 comparisons; the reference is provisional and not ground truth. | Measurement, `CHANGELOG.md` `v0.9.0` *Known limitations* |
| RQ-3 | Can 24-hour occupancy be forecast better than naive baselines when little real history exists? | Open. The method and its acceptance gate are implemented and checked offline on simulated campuses; no real-site result exists yet. | Code and test, `gateway/ml/` |
| RQ-4 | Can persisted site observations be replayed idempotently and confirmed promptly after a link interruption? | Partially answered for transport in staging: confirmation throughput rose from 6.1 to 15.7 events/s and p95 latency fell from 91.9 s to 0.68 s with 400 synthetic spot IDs and 20 cameras. This does not establish field behaviour or unlimited outage tolerance. | Measurement, #154 |

## 4.2 Design decisions

Each decision records the alternatives considered and the evidence available
when it was taken. Consequences are summarized in [Trade-offs](09-trade-offs.md).

### D-01 Detector: RT-DETR exported from the official implementation

*Context.* The detector must be licensed for commercial use, run in real time on
a single GPU across many inputs, and remain reproducible.
*Alternatives.* YOLOv10-X was catalogued as Apache-licensed, but its official
implementation is AGPL and it was discarded in the license review; Ultralytics
packaging was removed for the same reason.
*Decision.* RT-DETR (HGNetv2-L by default; R18 as an opt-in profile) [1],
exported from the official Paddle implementation to ONNX with pinned weights
digests and a provenance file (`LICENSING.md`).
*Evidence.* In an eight-second stress run of 25 instances on an RTX 5060 Ti
16 GB, RT-DETR-L reached 4.21 FPS per camera (105.2 FPS aggregate, 6,840 MB
VRAM) through the earlier Ultralytics TensorRT FP16 path; this does not certify
the current runtime or occupancy accuracy (`LICENSING.md`).

### D-02 Inference runtime: ONNX Runtime, TensorRT opt-in

*Alternatives.* PyTorch with Ultralytics (removed); TensorRT by default.
*Decision.* ONNX Runtime 1.31.0 with the CUDA execution provider; the TensorRT
FP16 R18 profile with spot-only tiles stays opt-in until it passes the 20-input
test (RQ-1). Engines are hardware-specific and are built on the target host.

### D-03 Occupancy from detections, not per-spot classification

*Decision.* Occupancy is the fraction of a spot polygon covered by vehicle
detections, stabilized by hysteresis and temporal debounce, with overlapping
tiles to keep small vehicles detectable and optional inference only on tiles
that intersect configured spots.
*Rationale.* It reuses a general detector without per-site training data and
exposes interpretable, calibratable parameters per detector profile.

### D-04 Forecast model: calendar-aware gradient boosting with a simulated prior

*Alternatives.* An attention-based sequence model (removed in the unreleased
changes, together with `torch` in the forecast); naive seasonal baselines only.
*Decision.* LightGBM [2] on calendar and lag features, trained on simulated
campuses around the site profile pooled with real history (real hours weighted
more). An evaluated rolling backtest selects the lowest-error method among the
model and naive baselines; the manifest records when evaluation is unavailable
([Methodology](05-methodology.md#3-forecast-evaluation)).
*Rationale.* A simulated prior supplies training examples before substantial
real history accrues. Serving still requires recent observed history. Neither
the prior nor historical selection proves forecast skill at a new site; that
requires prospective evaluation (G-05).

### D-05 Delivery semantics: durable outboxes and idempotent application

*Alternatives.* Fire-and-forget WebSocket messages (the earlier behaviour);
exactly-once protocols.
*Decision.* At-least-once delivery with per-event UUIDs, acknowledgement after
commit at every hop and idempotent receipts at the server, with a bounded
in-flight window (§[Architecture](02-architecture.md#22-data-flows)).

### D-06 Routing: uniform-cost search over explicit edge costs

*Decision.* Least-cost routes by Dijkstra's algorithm [3] over edge costs
declared in the site map, rather than a geometric heuristic; tests confirm that
misleading coordinates do not change the route (`server/src/pathfinding.rs`).

### D-07 Identity: central provider instead of local accounts

*Alternatives.* Local registration and password login in each installation
(removed in the unreleased changes).
*Decision.* Keycloak as a central OpenID Connect provider [4]; each installation
is a confidential client using the authorization code flow with PKCE [5]. The
installation database keeps no e-mail address or password.

### D-08 Per-installation backend

*Alternatives.* One multi-tenant backend for all sites.
*Decision.* One server process and one database per installation, with
installation binding checked at start-up and readiness. Multi-tenancy remains
out of scope until a concrete need justifies its isolation cost.

### D-09 Data minimization: no vehicle plates in the first deployment

*Decision.* Plate collection was removed; a production installation refuses to
start with the license plate module enabled. The dormant module is kept for a
later, separately justified release.

### D-10 Licensing: Apache-2.0

*Decision.* Project code is licensed under Apache-2.0; rights granted under AGPL
for earlier revisions are not revoked; third-party components keep their own
licenses (`LICENSING.md`, `NOTICE`).
