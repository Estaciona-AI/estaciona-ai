# 8. Goals and roadmap

Status as of 2026-10-27. A goal is *met* only when its criterion is satisfied by
evidence of the class stated; partial progress is reported as such.

## 8.1 Goals

| ID | Goal | Success criterion | Evidence class required | Status |
|---|---|---|---|---|
| G-01 | Run supervised pilot installations | A pilot installation operates on a `0.9.x` release with the documented installation procedure | Field operation | Release published; field pilot pending |
| G-02 | Size one installation at 400 spots | 20 inputs × 20 spots at a minimum of 2.5 FPS per input on the target GPU (RQ-1) | Measurement on target hardware | Open: 10 inputs at a minimum of 2.48 FPS on a desktop proxy |
| G-03 | Validate end to end with real cameras | 20 real cameras and 400 spots delivering confirmed observations to the server | Field measurement | Open |
| G-04 | Quantify occupancy accuracy | An agreed metric on labelled ground truth meets an agreed threshold (Q-03) | Measurement | Open: only reference-detector agreement exists |
| G-05 | Serve forecasts better than naive baselines | The acceptance gate selects the model on real history and prospective scores confirm it (RQ-3) | Measurement | Open: requires four weeks of real history |
| G-06 | Move consumers to contract 2.0.0 | The hub and installation panels pass the consumer tests against 2.0.0 | Test | Open (outside this repository) |
| G-07 | Define production readiness | Written criteria covering G-02 to G-05, security review and operations (Q-04) | Review | Open |
| G-08 | Rewrite operator documentation | Installation, operation and upgrade procedures documented and kept current | Review | In progress: this directory |
| G-09 | Release the unreleased changes | `develop` promoted to `main` with a signed tag and release notes | Code | Pending |

## 8.2 Non-goals for the current release line

- Multi-tenancy within one backend, and horizontal replicas of the server.
- License plate recognition (no OCR pipeline; the dormant module stays disabled).
- Client applications in this repository.

## 8.3 Dependencies

G-02 decides Q-01 (default detector profile). G-03 and G-04 depend on access to
a pilot site and on a labelling protocol. G-05 depends on G-03, because the
forecast needs real occupancy history. G-07 depends on G-02 to G-05.
