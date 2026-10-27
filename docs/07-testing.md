# 7. Testing

## 7.1 Strategy

Tests are the primary evidence of behaviour (see
[Methodology](05-methodology.md#4-engineering-method)). They are organized in
four levels:

| Level | Purpose | Examples |
|---|---|---|
| Unit | Invariants of one module | Occupancy hysteresis, route costs, settings validation, secret files |
| Database | Behaviour against a real PostgreSQL, one isolated database per test | Reservations under concurrency, role restrictions, migrations, privacy rights |
| Integration | Real processes communicating over real sockets | Gateway against a server binary; identity tools against Keycloak; vision against the RTSP laboratory |
| Contract | Every published schema, fixture and response | Consumer tests that discover the API through `/api/v1/capabilities` |

Tests use synthetic data only. They must not depend on wall-clock timing beyond
bounded waits, on network access other than local services, or on the host time
zone.

## 7.2 Continuous integration

Every push and pull request runs the following workflows; the dependency
advisories also run on a schedule.

| Workflow | Gates |
|---|---|
| Rust CI | `cargo fmt --check`; `cargo clippy -D warnings`; `cargo test` with a PostgreSQL service; `cargo build --locked` |
| Python Gateway CI | Ruff lint and format; `pytest` including integration tests against the server binary built by Rust CI |
| Python Vision CI | Ruff lint and format (also over `tools/`); `pytest` |
| Identity tools CI | Realm and provisioning unit tests |
| RTSP lab integration | Vision against pinned MediaMTX and FFmpeg containers |
| Contract compatibility CI | License check; schema compatibility within the major version; contract tests |
| Rust and Python dependency security | RustSec and Python advisory audits of the committed lockfiles |

Third-party actions are pinned by commit digest, lockfiles are used with
`--locked`, and Dependabot proposes dependency updates
(`.github/dependabot.yml`).

## 7.3 Mutation testing

Mutation testing audits the strength of the suites by introducing small faults
and checking that a test fails [8, 9]. The campaign started on 2026-10-27
covered the vision service, the gateway and the server's route planner; the
server's security modules were in progress. Surviving mutants were triaged:
real gaps received tests (#215, #216 and follow-ups) and equivalent mutants
were recorded with their reason. Mutation results are a measure of test adequacy at a commit, not a
coverage guarantee.

## 7.4 Verification snapshot

Local results on 2026-10-27 at `develop` `78a14eb`:

| Suite | Result | Conditions |
|---|---|---|
| Server (`cargo test --all-features`) | 390 passed | Disposable PostgreSQL test database |
| Vision (`pytest`, without the RTSP integration test) | 407 passed | As in Python Vision CI |
| Gateway and forecast (`pytest`) | 179 passed, 170 skipped | No server binary or database: integration tests skipped |

For the same commit, the CI, Rust dependency security and Python dependency
security workflows passed on GitHub Actions, including the gateway integration
tests and the RTSP laboratory. Update this table
with each release.
