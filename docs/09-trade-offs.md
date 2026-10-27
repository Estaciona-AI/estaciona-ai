# 9. Trade-offs

Each major decision ([Research §4.2](04-research.md#42-design-decisions)) is
recorded with what it gained, what it cost, and how the cost is contained.
Quantities cite the [Evidence register](06-evidence-register.md).

| Decision | Gains | Losses and costs | Containment |
|---|---|---|---|
| D-01 RT-DETR from the official implementation | Commercially usable license; reproducible export with provenance | Earlier Ultralytics measurements no longer certify the runtime; a separate Python 3.11 export environment | Re-measure on the current runtime (G-02); export pinned in `vision/export/` |
| D-02 ONNX Runtime, TensorRT opt-in | Removed PyTorch and Ultralytics from vision; resident memory per process from 3.7 GB to 1.35 GB (F-06) | TensorRT engines are hardware-specific and proprietary; the faster profile is not yet the default | Engines built on the target host; default decided by Q-01 |
| D-03 Occupancy from detections | No per-site training data; interpretable, calibratable parameters | Sensitive to camera placement and occlusion; accuracy unknown (A-03, A-04) | Per-profile calibration; local inspector; G-04 |
| D-04 LightGBM with simulated prior and gate | Usable from day one; never serves a model worse than a naive baseline on real data | The attention model and the stay-duration model were removed; early forecasts rest on a simulation (A-05) | Prospective out-of-sample scoring; retraining daily |
| D-05 Durable delivery | No silent loss across restarts and outages; throughput 6.1 to 15.7 events/s, p95 91.9 s to 0.68 s in staging (F-05) | Local storage at each hop; at-least-once semantics require idempotent receivers | Bounded outboxes, retention and storage budget; idempotent receipts |
| D-06 Dijkstra over declared costs | Routes follow the site's real paths regardless of drawing coordinates | Costs must be maintained in the site map | Map validation on load; size limits on maps |
| D-07 Central identity provider | No passwords or e-mail addresses in installation databases; multi-factor sign-in for administrators | New sign-ins depend on the provider's availability; existing local accounts could not be migrated | Existing sessions keep working while the provider is unreachable |
| D-08 Per-installation backend | Strong isolation of data, keys and credentials per site | More processes and databases to operate; no shared analytics | Per-installation Compose stacks; isolation tests |
| D-09 No plates | Less personal data to protect | No vehicle-based features in the first deployment | Dormant module kept for a separately justified release |
| D-10 Apache-2.0 | Permits commercial and closed derivatives with an explicit patent grant | Third parties may also use the published code | Licensing decided by the rights holder; third-party components keep their own licenses (`NOTICE`) |
