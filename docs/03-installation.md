# 3. Installation

This chapter describes how to build and verify the core for development and the
order in which an installation is brought up. The exact settings are defined in
[`.env.example`](../.env.example), [`infra/env/`](../infra/env) and the header of
[`infra/docker-compose.yml`](../infra/docker-compose.yml); upgrade steps between
releases are in [`CHANGELOG.md`](../CHANGELOG.md) under *Upgrade notes*. Those
files take precedence over this summary.

## 3.1 Prerequisites

| Requirement | Used by | Notes |
|---|---|---|
| Rust stable toolchain (edition 2024) | Server | `cargo` with the committed `Cargo.lock` (`--locked`) |
| [uv](https://docs.astral.sh/uv/) | All Python packages | Installs the pinned interpreter: Python 3.14, or 3.11 for `vision/export/` |
| PostgreSQL 15 | Server, server tests, forecast worker | Docker image pinned by digest in `infra/docker-compose.yml` |
| Docker Engine with Compose | Deployment, identity provider, RTSP laboratory | |
| NVIDIA GPU with CUDA | Vision (optional) | CPU execution must be selected explicitly; the `tensorrt` extra installs proprietary NVIDIA components that this repository does not redistribute |

## 3.2 Development setup

Each Python package has its own `pyproject.toml` and `uv.lock`; run commands
from the package directory. The commands below are those enforced by continuous
integration (`.github/workflows/ci.yml`).

| Component | Lint and format | Tests |
|---|---|---|
| Server (`server/`) | `cargo fmt -- --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings` | `cargo test --locked --all-features` with `DATABASE_URL` pointing to a disposable test database |
| Gateway and forecast (`gateway/`) | `uv run --locked ruff check`; `uv run --locked ruff format --check` | `uv run --locked pytest` (integration tests need a server binary and a database and are skipped otherwise) |
| Vision (`vision/`) | as above, also over `../tools/rtsp-lab` and `../tools/identity` | `uv run --locked pytest --ignore=tests/test_rtsp_lab_integration.py` |
| Contracts (`shared/`) | as above; `python ../scripts/check_licenses.py` | `uv run --locked pytest` |
| Identity tools (`tools/identity/`) | — | `python3 -m unittest -v test_realm test_provision` |

GPU libraries for vision are installed with `uv sync --extra cuda`. Tests never
require real cameras, real credentials or personal data.

## 3.3 Bringing up an installation

The sequence below is the dependency order; each step is detailed in the
referenced file.

1. **Identity provider.** Start the central provider from `infra/identity/` and
   import the realm defined there. Register the installation as a client with
   `tools/identity/provision.py installation-create`, which writes the client
   secret to a private file.
2. **Secrets.** Create every secret as a private file owned by the service user,
   as listed in the header of `infra/docker-compose.yml`. Secrets are never
   placed in shared environment files, command-line arguments or Git.
3. **Installation package.** Provide `SITE_DATA_DIR` with the installation
   descriptor, site maps and optional reservation policy, and validate it
   offline with `--installation-validate --manifest PATH`.
4. **Database.** Initialize a new database with `--installation-init` (or adopt
   an existing one with `--installation-adopt --confirm-database`), apply the
   release's migrations with `--migrate` using the owner role, then run
   `infra/app-role.sql` to create the restricted runtime role.
5. **Server.** Start the server with the runtime role. It validates its
   configuration, secrets, database role and schema version before serving, and
   `/ready` reports when it is bound and reachable.
6. **Edge credentials.** Issue one credential per gateway and one for the
   forecast worker with `--edge-create`, and restrict each with `--edge-policy`.
7. **Gateway.** Start the gateway on the site host with its cloud credential
   and a separate local key for producers.
8. **Vision.** From `vision/`, export the detector with
   `uv run --project export --locked python tools/export_rtdetr.py` (the runtime
   refuses a model without matching provenance), optionally prebuild TensorRT
   engines on the target host with `tools/prebuild_engines.py`, validate the
   input configuration with `python src/runtime.py CONFIG --validate`, then run
   it.
9. **Forecast worker.** Create its read-only role with
   `infra/forecast-reader.sql` and run `python -m ml.worker` in `gateway/`.

When upgrading, roll out in the order server, gateway, vision, and follow the
release's upgrade notes; several migrations deliberately refuse automatic
downgrade.

## 3.4 Verification after installation

- `/health` answers while the process runs; `/ready` answers 200 only when the
  database is reachable and bound to this installation.
- `python src/runtime.py CONFIG --validate` checks vision configuration without
  loading models.
- The optional local inspector (`--inspect`, bound to `127.0.0.1`) shows frames,
  detections and spot polygons for calibration.
- The RTSP laboratory in `tools/rtsp-lab/` reproduces multi-input scenarios with
  synthetic streams for capacity checks before field deployment.
