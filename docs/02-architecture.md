# 2. Architecture

## 2.1 System context

```text
 cameras ──RTSP──▶ vision ──LOCAL_EVENT (loopback)──▶ gateway ──EDGE_EVENT (TLS)──▶ server ◀──▶ PostgreSQL
                     │                                   │                           ▲  │
                SQLite outbox                       SQLite outbox                    │  ├─ REST /api/v1
                                                                                     │  ├─ app / dashboard WebSockets
              forecast worker ──read-only SQL──▶ PostgreSQL                          │  └─ OIDC ──▶ identity provider
                     └──────────────TREND_PREDICTION_V2 (TLS)────────────────────────┘
                                                                     client apps (outside this repository)
```

## 2.2 Data flows

**Occupancy.** For each input, the vision service decodes frames, runs the
detector (RT-DETR through ONNX Runtime), rasterizes vehicle detections and
computes the fraction of each spot polygon they cover. A per-spot state machine
with hysteresis (a higher threshold to become occupied than to stay occupied)
and temporal debounce (consecutive frames and a minimum interval) turns these
ratios into confirmed transitions (`vision/src/occupancy.py`). Transitions and
periodic observations of the confirmed state become events; the latter keep
unchanged spots observable (`vision/src/worker.py`).

**Delivery.** Each hop keeps an event in a durable outbox until the next hop
acknowledges it, and every event keeps one UUID across retries.

1. Vision writes the event to a local SQLite outbox and sends it to the gateway
   as `LOCAL_EVENT`. The gateway answers `LOCAL_ACK` only after its own SQLite
   commit; a saturated vision outbox pauses capture instead of dropping
   transitions.
2. The gateway sends `EDGE_EVENT` frames to the server's `/ws/edge` socket with
   up to eight unacknowledged events in flight (`EDGE_MAX_IN_FLIGHT`). The
   server applies each event through an idempotent receipt and acknowledges only
   after the PostgreSQL commit.
3. Event types the server does not announce stay queued at the gateway until a
   compatible server is available.

The semantics are *at-least-once delivery with idempotent application* for
persisted, supported events. They rely on retained durable storage and receiver
availability; configured storage bounds can pause producers. A local ACK proves
gateway persistence, not the server's commit. A cloud ACK records an applied,
ignored or rejected outcome; it does not mean every submitted observation
changed a spot. The system does not claim exactly-once delivery.

**Freshness.** A spot whose latest observation is older than
`OBSERVATION_STALE_AFTER_SECS` (default 120 s), or that no camera has reported,
is `unknown`: it is neither reservable nor recommended until observed again.

**Forecasting.** The forecast worker reads the per-spot hourly occupancy history
through a read-only database role, forecasts the next 24 hours of occupancy rate
every hour, retrains daily, and publishes `TREND_PREDICTION_V2` through
`/ws/edge` with its own edge credential (§[Methodology](05-methodology.md#3-forecast-evaluation)).

**Navigation and recommendation.** The server builds a weighted graph from the
site map and computes least-cost routes by uniform-cost (Dijkstra) search over
edge costs; blocked edges are skipped. Recommendations start at the nearest
entrance of the relevant parking lot. An arrival with no valid route produces
`NAVIGATION_UNAVAILABLE` rather than a fabricated route.

**Reservations.** Creation, cancellation and completion are atomic and check
ownership; lifetimes come from the installation's reservation policy
(`CHANGELOG.md`, `v0.9.0`).

## 2.3 Interfaces

| Interface | Consumers | Definition |
|---|---|---|
| REST API under `/api/v1` | Client applications | [Catalog](../shared/v1/catalog.json), `/api/v1/capabilities`, `/api/v1/contracts/v1/manifest.json` |
| App and dashboard WebSockets | Client applications | Schemas in `shared/v1/`; opened with a single-use ticket |
| Edge WebSocket `/ws/edge` | Gateway, forecast worker | `gateway-to-server` and `edge-events` schemas |
| Local vision transport | Vision ↔ gateway | [Producer envelopes](../shared/v1/vision-to-gateway.schema.json) and [persistence ACKs](../shared/v1/gateway-to-vision.schema.json) |
| Liveness and readiness | Orchestrators | `/health` (process alive), `/ready` (database reachable and bound to this installation) |

The JSON Schemas are generated from the server's Rust types and exported with
`--export-contracts`. The schema bundle is versioned independently of the
release (2.0.0 on `develop`); continuous integration refuses an incompatible
change published under the same major version.

Three version identifiers have different scopes: the release tag identifies the
software; the manifest's version identifies the schema bundle; and an envelope's
`version` identifies its delivery protocol. The `v1` directory and HTTP prefix
are locators, not the schema bundle's major version. Consumers discover the
manifest and capabilities rather than infer compatibility from a path name
([manifest](../shared/v1/manifest.json), [capabilities](../server/src/api_contract.rs)).

The identity callback is outside the versioned REST API. Browser applications
enter through `/auth/login`, then obtain the signed-in account through
`/api/v1/me`; passwords are not sent to the installation server. Client code
must handle `unknown` occupancy and unavailable forecast evaluation explicitly.

## 2.4 Persistence

| Store | Owner | Content |
|---|---|---|
| PostgreSQL (one per installation) | Server | Spots, observations, reservations, sessions, accounts, receipts, hourly occupancy history, audit trail |
| Gateway SQLite | Gateway | Pending and recently confirmed events; delivery metrics |
| Vision SQLite | Vision | Pending local events |
| Forecast artifacts and ledger | Forecast worker | Model, manifest and the forecasts as issued |

Schema changes are applied by an explicit migration step with an owner role;
the server runs with a restricted role and refuses a schema that is behind or
ahead of its release.

Physical observations, reservations and forecasts have different authority.
Vision supplies physical occupancy; the server applies reservation rules and
freshness to the status it serves. The forecast reads aggregated physical
history rather than treating a reservation as a parked vehicle. A forecast
never writes current spot occupancy. See [edge application](../server/src/edge_delivery.rs)
and [forecast history](../gateway/ml/history.py).

The PostgreSQL binding defines the installation served by this process. IDs in
an event select a granted camera or spot; they do not switch databases or
establish a tenant. In-memory publication is local to one server process, so
the current design does not imply coordination between replicas (D-08).

## 2.5 Trust boundaries

1. **Camera network → vision.** Streams are untrusted input.
2. **Vision → gateway.** Loopback only, authenticated with a local key that is
   distinct from the gateway's cloud credential.
3. **Gateway and forecast worker → server.** Each edge has its own revocable
   credential with explicit grants checked on every event.
4. **Browser → server.** Authenticated sessions established through the identity
   provider; origins are restricted to the installation's applications.
5. **Server → database.** A role that reads and writes rows but cannot change
   the schema.
6. **Server → identity provider.** OpenID Connect with a confidential client per
   installation.

Controls at each boundary are summarized in
[Security and privacy](10-security-and-privacy.md).
