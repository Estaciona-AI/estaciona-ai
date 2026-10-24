# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
from `v0.9.0` onwards. While the major version is `0`, a minor release may
contain breaking changes; they are always listed under **Upgrade notes**.

The earlier milestones were tagged `alpha-0.1` and `beta-1.0`, before
Semantic Versioning was adopted. Those tags are kept unchanged.

## [Unreleased]

## [0.9.0] - 2026-10-23

**Release stage: pilot.** This is the first build that is not a pre-release.
It turns the beta showcase into a product core for supervised pilot
installations: backend, edge gateway, computer vision and forecasting. Each
installation runs its own backend and database. Capacity on the target
inference hardware and a full set of real cameras have not been validated yet;
see [Known limitations](#known-limitations).

### Upgrade notes

This release breaks compatibility with `beta-1.0`. An existing installation
needs the following steps:

1. **Roll out in order.** Apply the server migrations and update the server
   first, then the edge gateway, then the vision service. The gateway keeps
   any event type the server does not announce queued until the server
   supports it, and it still accepts vision producers that use the legacy
   local protocol ([#106], [#110], [#119]).
2. **Resolve duplicate active reservations** before migrating. A partial unique
   index now allows one active reservation per spot, and the migration does
   not delete existing rows ([#88]).
3. **Provide an installation descriptor.** Every installation needs
   `SITE_DATA_DIR/installation.json` with explicit organization, installation
   and environment identifiers. Bind the database with `--installation-init`
   (empty database) or `--installation-adopt --confirm-database` (existing
   database) before starting the server or running any admin command.
   `--installation-validate --manifest PATH` checks a deployment package
   offline ([#129], [#130], [#131], [#132], [#136], [#137], [#138]).
4. **Set the new required environment variables.** The server refuses to start
   without `WS_ALLOWED_ORIGINS` (comma-separated, exact HTTP(S) origins) and
   `PLATE_SECRET_PEPPER`. It validates its configuration before connecting to
   the database ([#105], [#121], [#137]).
5. **Issue a credential for each gateway.** Gateways no longer share one key.
   Use `--edge-create`, `--edge-rotate`, `--edge-revoke` and `--edge-policy` to
   manage each gateway's credential and its grants for cameras, spots and
   operations ([#93]).
6. **Update WebSocket clients.** Browser sockets authenticate with a one-time
   ticket from `POST /auth/ws-ticket` (Bearer token) passed as `?ticket=`. The
   `?token=` query parameter and the cookie no longer authenticate sockets, and
   the `Origin` header must match `WS_ALLOWED_ORIGINS` ([#104], [#105]).
7. **Expect users to sign in again.** Sessions are now recorded and revocable.
   Tokens issued before the upgrade are rejected, and `POST /auth/logout` ends
   the current session ([#107]).
8. **Pass admin passwords through a file.** `--create-admin EMAIL PASSWORD` is
   replaced by `--create-admin EMAIL --password-file PATH`. The file must be a
   private regular file ([#124]).
9. **Configure reservation policy on the server.** Reservation lifetimes come
   from the optional `reservation_policy.json` in `SITE_DATA_DIR`. The defaults
   are a 15-minute hold, a 30-minute maximum lifetime and one active
   reservation per user. A client-supplied `expires_at` is still accepted but
   no longer sets the expiry ([#111], [#115]).
10. **Re-export vision models.** Vision now runs the official RT-DETR through
    ONNX Runtime. Export models with `vision/export`. PyTorch `.pt` weights are
    refused, and CPU execution must be selected explicitly ([#116]).
11. **Handle new client events.** Dashboards and apps should handle
    `TREND_PREDICTION_V2` and `NAVIGATION_UNAVAILABLE`. The original
    `TREND_PREDICTION` is still accepted ([#97], [#118]).

Several migrations refuse to downgrade while data depends on them, for example
revocable sessions, inactive spots and the installation binding. Rolling back
requires a planned reconciliation, not an automatic `down` migration.

### Added

#### Installations

- Validated installation descriptors, a database binding to an explicit
  organization, installation and environment, and provisioning commands that
  check pending migrations before applying them ([#129], [#130], [#131]).
- Deployment packages that load the descriptor, maps and reservation policy as
  one validated unit, plus an offline validation command ([#136], [#138]).

#### API and contracts

- Versioned REST API under `/api/v1`. Errors carry a stable category, a public
  message and a request ID. `/api/v1/capabilities` describes the available
  authentication methods and paths ([#115]).
- JSON Schema 2020-12 contracts generated from the Rust DTOs, published at
  `/api/v1/contracts` and exported offline with `--export-contracts`. Wire
  version 1 is preserved; the schema bundle is `1.3.0` ([#114], [#115], [#139]).
- Operation catalog discoverable through `v1/manifest.json`, covering 20 HTTP
  operations, the app, dashboard and edge sockets, and the local vision
  transport ([#139]).
- Consumer tests that discover the API through capabilities and validate every
  HTTP response and WebSocket frame against the published schemas ([#140],
  [#141]).

#### Authentication

- One-time WebSocket tickets: 256-bit, single use, valid for 30 seconds and
  bound to the session, role and target socket ([#104]).
- Revocable login sessions and `POST /auth/logout` ([#107]).

#### Edge delivery

- Durable gateway-to-server delivery for spot, route and forecast events. Each
  event keeps its UUID across retries, the server applies it with an idempotent
  receipt, and the ACK is sent only after the PostgreSQL commit ([#89], [#106]).
- Durable vision-to-gateway delivery through a local SQLite outbox and the
  `LOCAL_EVENT` / `LOCAL_ACK` protocol. The outbox is bounded and reports
  saturation instead of dropping events ([#110]).

#### Vision runtime

- Headless execution, with processing separated from presentation ([#142]).
- Multi-input supervisor. Each input runs in its own process with its own
  capture, model session, occupancy state and outbox, plus a watchdog and
  restart backoff ([#143]).
- Optional process groups (`inputs_per_process`, 1 to 4) that share a process
  while keeping input state independent ([#147]).
- Local read-only inspector (`--inspect`, bound to `127.0.0.1`) showing the
  analysed frame, boxes, polygons and pipeline metrics. It is disabled by
  default ([#145]).

#### Vision detection

- Per-profile detector calibration: confidence, gamma and overlapping tiles
  that keep small vehicles detectable ([#148]).
- Opt-in inference on the tiles that intersect configured spots, with an
  optional TensorRT FP16 profile for RT-DETR-R18 and a reproducible R18 export
  ([#155]).
- `vision/tools/prebuild_engines.py`, which builds TensorRT engines before
  operation instead of on the first frame ([#157]).

#### Tooling

- Reproducible RTSP lab built on pinned MediaMTX and FFmpeg images, with
  synthetic scenarios of 20 to 35 inputs mapping exactly 400 spot IDs ([#144]).
- Shadow benchmarks, capacity ramps with provenance, frame corpora and
  occupancy evaluation. Benchmark reports use `schema_version: 2` ([#146],
  [#149], [#151]).

#### Optional license plate module

- Vehicle claims, admin-approved vehicle bindings and an audit trail. Plates
  are stored only as HMAC. The module is off by default (`LPR_ENABLED`), and no
  OCR pipeline is included ([#103]).

### Changed

- Project code is now licensed under Apache-2.0, with `NOTICE` and
  `LICENSING.md` covering third-party components. Rights granted under AGPL for
  earlier revisions are not revoked ([#116]).
- The core no longer ships customer-facing applications. Installation maps load
  from `SITE_DATA_DIR`, and forecast capacity is derived from the registered
  spots ([#85]).
- Installation maps are reconciled in one transaction at startup. Removed spots
  are deactivated, not deleted, so their history is kept ([#98]).
- Server runtime resources are grouped per installation, and independent
  installations are covered by isolation tests ([#133], [#134], [#135]).
- Forecasting reads a per-spot, per-hour physical occupancy history kept
  separate from reservation history. It reports unavailable or partial metrics
  instead of zeros, requires 168 contiguous hours for inference, and retrains
  on the most recent contiguous segment of at least 193 hours ([#118], [#122],
  [#126], [#127]).
- Arrivals with no valid route now produce `NAVIGATION_UNAVAILABLE` instead of
  a fabricated route ([#97]).
- The server, gateway, vision and model export packages are versioned `0.9.0`,
  matching the release tag. The contract check package keeps its own version.
- `.env.example` lists every setting the server requires, including
  `WS_ALLOWED_ORIGINS`, together with the optional `LPR_ENABLED` and
  `EDGE_MAX_IN_FLIGHT`.

### Performance

- The gateway keeps up to eight unacknowledged events in flight per cloud
  connection (`EDGE_MAX_IN_FLIGHT`, 1 to 8, default 8; set it to 1 for the
  previous behavior). In a transport-only staging test with 400 synthetic spot
  IDs and 20 cameras, confirmation throughput rose from 6.1 to 15.7 events/s
  and p95 latency fell from 91.9 s to 0.68 s ([#154]).
- Spot-only tiles with TensorRT FP16 RT-DETR-R18, together with lower CPU cost
  per frame, raised the minimum per-input rate at 10 inputs from 0.53 to
  2.48 FPS on a desktop RTX 5060 Ti used as a proxy. Selective loading of the
  TensorRT libraries cut resident memory per process from 3.7 GB to 1.35 GB
  ([#152], [#153], [#155], [#156]).

### Fixed

- Reservations: creation, cancellation and completion are atomic and check
  ownership. Completion requires an occupied spot. Stale or future-dated
  observations can no longer overwrite newer state ([#88], [#108], [#112]).
- Recommendation queries return `503` on database failures instead of
  answering from partial data ([#99]).
- Slow WebSocket consumers are bounded and disconnected without affecting other
  clients, and a lagging stream reconnects to a fresh snapshot ([#100]).
- `/config` is written atomically without blocking async workers ([#101]).
- Navigation node names can no longer collide with spot names ([#102]).
- The gateway persists bounded batches during continuous ingestion and no
  longer blocks on event types the server does not support ([#109], [#119]).
- Vision timestamps keep microsecond precision, and a saturated outbox pauses
  capture instead of dropping transitions ([#117], [#120]).
- Edge failure logs keep the underlying error cause ([#94]).

### Security

- Each gateway has its own credential, stored as a SHA-256 digest, with
  explicit grants checked on every event and revalidated while connected
  ([#93]).
- JWTs expire strictly at `exp`. Role changes require a new login. Sockets are
  revalidated every 5 seconds and closed when the session ends ([#91], [#107]).
- Login and registration are rate limited, run Argon2 outside the async
  workers, and enforce body and field size limits ([#92]).
- WebSocket upgrades require single-use tickets and an allowed `Origin`. HTTP
  tracing no longer logs query strings ([#104], [#105]).
- Plate fingerprints in event receipts use versioned HMAC-SHA256 with a
  secret pepper ([#103], [#121]).
- Admin passwords are no longer passed as command-line arguments ([#124]).
- The `demo-token` authentication bypass is removed ([#85]).

### Removed

- Web frontend, 3D reconstruction, GitHub Pages publishing, research paper
  sources and outdated documentation, including the root and module READMEs.
  The READMEs will be rewritten ([#85], [#86]).
- Ultralytics and PyTorch as vision dependencies. PyTorch remains in the
  forecasting module ([#116]).
- Authentication of WebSockets through the `?token=` query parameter ([#104]).

### Known limitations

- **Capacity is not validated on the target hardware.** The sizing goal is
  20 inputs × 20 spots at 2.5 FPS per input (400 spots per installation). It
  has been measured only up to 10 inputs on a desktop GPU. A 20-input test on
  the target datacenter GPU is still pending.
- **TensorRT stays opt-in.** RT-DETR-L FP32 on CUDA remains the default
  profile until the RT-DETR-R18 TensorRT FP16 profile passes the 20-input test.
- **TensorRT engines are hardware-specific.** An engine is tied to the GPU,
  driver and TensorRT version, so it must be built on the target host with
  `vision/tools/prebuild_engines.py`. Build time on the target hardware has not
  been measured.
- **The `tensorrt` extra installs proprietary NVIDIA components.** They are
  not part of the default installation and are not redistributed by this
  repository.
- **No field validation with real cameras yet.** End-to-end validation with
  20 real cameras and 400 spots is pending. Lab results come from synthetic
  RTSP scenarios built from a single recorded video.
- **Detection quality has no production accuracy figure.** On the lab corpus,
  detections agree with a larger reference detector (924/924). The evaluation
  references are provisional and are not ground truth.
- **Forecast weights need re-evaluation.** They must be re-checked against the
  new physical occupancy history. Retraining is not automatic.
- **One installation per deployment.** Each installation runs its own backend
  and database. Multi-tenancy, SSO and horizontal replicas are not
  implemented.
- **License plate recognition is incomplete.** The optional module has no OCR,
  and `CAR_DETECTED` events are not delivered durably when it is enabled.
- **Client applications live outside this repository.** They must adopt
  WebSocket tickets, the `Origin` allowlist, `TREND_PREDICTION_V2` and
  `NAVIGATION_UNAVAILABLE`.
- **Operator documentation is incomplete.** The READMEs are being rewritten.
  Until then, `.env.example` and this changelog describe the required
  settings.
- **One gateway test is timing-sensitive.**
  `test_timeout_retry_preserves_uuid_and_payload` occasionally fails in CI and
  passes on retry.

## [beta-1.0] - 2026-09-26

Pre-release used for a public showcase.

### Added

- Refreshed interface with a dark theme ([#75]).
- Demo mode: a demo prompt replaces login, and dashboard saves are disabled
  ([#76], [#77], [#78], [#79], [#80]).

### Fixed

- Restored service readiness ([#74]).

## [alpha-0.1] - 2026-06-18

First prototype, published as a pre-release.

### Added

- Real-time messaging between the edge and the app over WebSockets.
- Reservations with atomic expiry, persisted in PostgreSQL.
- A* pathfinding for in-lot navigation.
- Server rewritten in Rust, with security, persistence and tests.
- YOLO-based vision client and edge gateway, with an occupancy forecasting
  model.
- Web app with role-based registration and an admin dashboard.

[Unreleased]: https://github.com/Estaciona-AI/estaciona-ai/compare/v0.9.0...HEAD
[0.9.0]: https://github.com/Estaciona-AI/estaciona-ai/compare/beta-1.0...v0.9.0
[beta-1.0]: https://github.com/Estaciona-AI/estaciona-ai/compare/alpha-0.1...beta-1.0
[alpha-0.1]: https://github.com/Estaciona-AI/estaciona-ai/releases/tag/alpha-0.1

[#74]: https://github.com/Estaciona-AI/estaciona-ai/pull/74
[#75]: https://github.com/Estaciona-AI/estaciona-ai/pull/75
[#76]: https://github.com/Estaciona-AI/estaciona-ai/pull/76
[#77]: https://github.com/Estaciona-AI/estaciona-ai/pull/77
[#78]: https://github.com/Estaciona-AI/estaciona-ai/pull/78
[#79]: https://github.com/Estaciona-AI/estaciona-ai/pull/79
[#80]: https://github.com/Estaciona-AI/estaciona-ai/pull/80
[#85]: https://github.com/Estaciona-AI/estaciona-ai/pull/85
[#86]: https://github.com/Estaciona-AI/estaciona-ai/pull/86
[#88]: https://github.com/Estaciona-AI/estaciona-ai/pull/88
[#89]: https://github.com/Estaciona-AI/estaciona-ai/pull/89
[#91]: https://github.com/Estaciona-AI/estaciona-ai/pull/91
[#92]: https://github.com/Estaciona-AI/estaciona-ai/pull/92
[#93]: https://github.com/Estaciona-AI/estaciona-ai/pull/93
[#94]: https://github.com/Estaciona-AI/estaciona-ai/pull/94
[#97]: https://github.com/Estaciona-AI/estaciona-ai/pull/97
[#98]: https://github.com/Estaciona-AI/estaciona-ai/pull/98
[#99]: https://github.com/Estaciona-AI/estaciona-ai/pull/99
[#100]: https://github.com/Estaciona-AI/estaciona-ai/pull/100
[#101]: https://github.com/Estaciona-AI/estaciona-ai/pull/101
[#102]: https://github.com/Estaciona-AI/estaciona-ai/pull/102
[#103]: https://github.com/Estaciona-AI/estaciona-ai/pull/103
[#104]: https://github.com/Estaciona-AI/estaciona-ai/pull/104
[#105]: https://github.com/Estaciona-AI/estaciona-ai/pull/105
[#106]: https://github.com/Estaciona-AI/estaciona-ai/pull/106
[#107]: https://github.com/Estaciona-AI/estaciona-ai/pull/107
[#108]: https://github.com/Estaciona-AI/estaciona-ai/pull/108
[#109]: https://github.com/Estaciona-AI/estaciona-ai/pull/109
[#110]: https://github.com/Estaciona-AI/estaciona-ai/pull/110
[#111]: https://github.com/Estaciona-AI/estaciona-ai/pull/111
[#112]: https://github.com/Estaciona-AI/estaciona-ai/pull/112
[#114]: https://github.com/Estaciona-AI/estaciona-ai/pull/114
[#115]: https://github.com/Estaciona-AI/estaciona-ai/pull/115
[#116]: https://github.com/Estaciona-AI/estaciona-ai/pull/116
[#117]: https://github.com/Estaciona-AI/estaciona-ai/pull/117
[#118]: https://github.com/Estaciona-AI/estaciona-ai/pull/118
[#119]: https://github.com/Estaciona-AI/estaciona-ai/pull/119
[#120]: https://github.com/Estaciona-AI/estaciona-ai/pull/120
[#121]: https://github.com/Estaciona-AI/estaciona-ai/pull/121
[#122]: https://github.com/Estaciona-AI/estaciona-ai/pull/122
[#124]: https://github.com/Estaciona-AI/estaciona-ai/pull/124
[#126]: https://github.com/Estaciona-AI/estaciona-ai/pull/126
[#127]: https://github.com/Estaciona-AI/estaciona-ai/pull/127
[#129]: https://github.com/Estaciona-AI/estaciona-ai/pull/129
[#130]: https://github.com/Estaciona-AI/estaciona-ai/pull/130
[#131]: https://github.com/Estaciona-AI/estaciona-ai/pull/131
[#132]: https://github.com/Estaciona-AI/estaciona-ai/pull/132
[#133]: https://github.com/Estaciona-AI/estaciona-ai/pull/133
[#134]: https://github.com/Estaciona-AI/estaciona-ai/pull/134
[#135]: https://github.com/Estaciona-AI/estaciona-ai/pull/135
[#136]: https://github.com/Estaciona-AI/estaciona-ai/pull/136
[#137]: https://github.com/Estaciona-AI/estaciona-ai/pull/137
[#138]: https://github.com/Estaciona-AI/estaciona-ai/pull/138
[#139]: https://github.com/Estaciona-AI/estaciona-ai/pull/139
[#140]: https://github.com/Estaciona-AI/estaciona-ai/pull/140
[#141]: https://github.com/Estaciona-AI/estaciona-ai/pull/141
[#142]: https://github.com/Estaciona-AI/estaciona-ai/pull/142
[#143]: https://github.com/Estaciona-AI/estaciona-ai/pull/143
[#144]: https://github.com/Estaciona-AI/estaciona-ai/pull/144
[#145]: https://github.com/Estaciona-AI/estaciona-ai/pull/145
[#146]: https://github.com/Estaciona-AI/estaciona-ai/pull/146
[#147]: https://github.com/Estaciona-AI/estaciona-ai/pull/147
[#148]: https://github.com/Estaciona-AI/estaciona-ai/pull/148
[#149]: https://github.com/Estaciona-AI/estaciona-ai/pull/149
[#151]: https://github.com/Estaciona-AI/estaciona-ai/pull/151
[#152]: https://github.com/Estaciona-AI/estaciona-ai/pull/152
[#153]: https://github.com/Estaciona-AI/estaciona-ai/pull/153
[#154]: https://github.com/Estaciona-AI/estaciona-ai/pull/154
[#155]: https://github.com/Estaciona-AI/estaciona-ai/pull/155
[#156]: https://github.com/Estaciona-AI/estaciona-ai/pull/156
[#157]: https://github.com/Estaciona-AI/estaciona-ai/pull/157
