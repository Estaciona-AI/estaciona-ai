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

The main CI workflow runs on configured push branches and pull requests to
`develop` or `main`; dependency advisories have their own event filters and
scheduled runs. The workflow files in [`.github/workflows/`](../.github/workflows)
define the exact triggers.

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
and checking that a test fails [[8, 9]](references.md). The campaign started on
2026-10-27 produced focused regression tests for vision (#215), routing (#216),
and gateway and forecast (#217), then for the server's security-relevant
modules (#220): origin checks, public error codes, file size limits, the
personal data keyring, session expiry and revocation, ticket cleanup, edge
delivery and its command line, route costs and reservations. These merged tests are included in F-12.
The campaign was scoped by module; it does not establish that every module or
surviving mutant has been evaluated. Mutation results describe test adequacy
at a commit, not exhaustive coverage or an absence of defects.

## 7.4 Verification snapshot

The following executions were verified on GitHub Actions on 2026-10-28 at
`develop` `7bf6c6e` ([CI run 40738777401](https://github.com/Estaciona-AI/estaciona-ai/actions/runs/40738777401)).
They are CI results, not a new local execution or a capacity benchmark.

| Suite and package directory | Command | Result | Conditions |
|---|---|---|---|
| Server (`server/`) | `cargo test --locked --all-features` | 407 passed | PostgreSQL service; isolated test databases |
| Vision (`vision/`) | `uv run --locked pytest --ignore=tests/test_rtsp_lab_integration.py` | 407 passed | Dedicated RTSP case executed separately |
| Gateway and forecast (`gateway/`) | `uv run --locked pytest --junitxml=test-results/gateway.xml` | 395 passed | Server binary and PostgreSQL available; integration required |
| Contracts (`shared/`) | `uv run --locked pytest --junitxml=test-results/contracts.xml` | 36 passed | Exported schemas and compatibility checks |
| Identity tools (`tools/identity/`) | `python3 -m unittest -v test_realm test_provision` | 17 passed | Synthetic provisioning and realm fixtures |
| RTSP laboratory (`vision/`) | `uv run --locked pytest -q tests/test_rtsp_lab_integration.py` | 1 passed | Docker, pinned publishers and recovery scenario |

The main run also passed its formatting, lint, build, license and contract
compatibility gates. The separate
[Rust dependency security run](https://github.com/Estaciona-AI/estaciona-ai/actions/runs/40738777927)
and [Python dependency security run](https://github.com/Estaciona-AI/estaciona-ai/actions/runs/40738777549)
passed at the same commit. These results concern the committed dependencies
and checked paths; they do not certify an installation.

F-11 retains the previous CI snapshot at `9036d10`, and F-10 the earlier local
snapshot at `78a14eb`, including its integration skips. Neither is the current
suite count. Update this section before a release,
and whenever a changed verification result is used to support a claim.
