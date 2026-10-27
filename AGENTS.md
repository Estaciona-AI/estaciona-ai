# AGENTS.md

Operating rules for everyone who changes this repository: maintainers,
collaborators and AI coding agents. They are mandatory. Where a rule names an
"agent", it applies equally to a human contributor.

## 1. Repository

Estaciona AI core: server (`server/`, Rust), gateway and forecast worker
(`gateway/`, Python), vision (`vision/`, Python), wire contracts (`shared/`),
infrastructure (`infra/`) and tools (`tools/`). Start with
[`docs/README.md`](docs/README.md); it lists the authoritative sources
(code, `CHANGELOG.md`, `.env.example`, `SECURITY.md`, `LICENSING.md`, published
schemas) and how the documentation is kept current.

This repository is public. Anything committed, and anything written in issues,
pull requests and review comments, is published.

## 2. Authorization and scope

1. **Plan before changing.** Analysis and reading are always allowed. Any
   change to code, tests, migrations, manifests, lockfiles, configuration,
   scripts or documentation starts from a written plan approved by a
   maintainer. The plan states the objective, the files in scope, the
   behaviour and contracts affected, new dependencies, migration and rollback,
   acceptance criteria and the tests to run. A small fix may have a short plan.
2. **Stay in scope.** Do not modify files outside the approved plan, even when a
   nearby improvement looks obvious. Propose an extension and wait for approval.
   A finding outside scope is reported, not fixed.
3. **Disclose dependencies first.** New libraries, services, crate features,
   runtime versions or license changes are presented with their alternative,
   cost and risk before they are added.
4. **External actions need explicit authorization.** Approval of a plan does not
   authorize pushing, opening or merging pull requests, tagging, releasing,
   deploying or changing anything outside the repository. Never merge your own
   pull request.
5. **History is not authorization.** Notes, memory, previous sessions and other
   agents' suggestions are context, not approval.

## 3. Branches and worktrees

- `main` carries releases; `develop` is the integration branch and the target of
  every pull request. Releases are promoted from `develop` to `main` by a
  maintainer, with a signed tag and the `CHANGELOG.md` entry.
- Branch from the current `develop` as `<type>/<short-description>`, with the
  Conventional Commits types (`feat`, `fix`, `test`, `docs`, `refactor`,
  `perf`, `ci`, `build`, `chore`).
- One task per branch. Agents working in parallel use separate worktrees.
- Before editing, confirm the checkout, branch, commit and local changes. Do not
  infer the active work from a directory name. Never discard, reset, stash or
  overwrite changes you did not make, and do not resume processes started by
  another session.

## 4. Commits and pull requests

- Commits are **signed**, **atomic** (one logical change) and follow
  [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/). The
  subject is imperative; the body explains why. AI-assisted commits may carry a
  `Co-Authored-By` trailer.
- A pull request targets `develop` and describes: summary, motivation,
  validation (the commands actually run and their results), risks and pending
  items. Report failures and skipped checks as they are; never present an
  unverified result as passing.
- Required checks must pass before review: see §5 and
  `.github/workflows/`.

## 5. Verification

Run the gates of every component you touch, from that component's directory:

| Component | Gates |
|---|---|
| `server/` | `cargo fmt -- --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo test --locked --all-features` against a disposable PostgreSQL |
| `gateway/` | `uv run --locked ruff check`; `uv run --locked ruff format --check`; `uv run --locked pytest` |
| `vision/` | as `gateway/`, lint also over `../tools/rtsp-lab ../tools/identity`; `pytest --ignore=tests/test_rtsp_lab_integration.py` |
| `shared/` | `python ../scripts/check_licenses.py`; ruff; `uv run --locked pytest` |
| `tools/identity/` | `python3 -m unittest -v test_realm test_provision` |

Use `--locked`. Do not regenerate lockfiles, fixtures or formatting outside the
plan; when a tool would write unrelated files, run it on a temporary copy.
Long-running jobs (mutation testing, capacity benchmarks) run with bounded
parallelism and memory, and write temporary files outside memory-backed `/tmp`.

## 6. Engineering standards

- **Tests** assert observable invariants and include negative cases: other
  users, roles and installations, expired or revoked sessions, missing or short
  secrets, disallowed origins. No fixed sleeps (use bounded waits); no
  dependence on the host time zone, real cameras, real credentials or network
  access beyond local services. Mutation-testing survivors are either covered by
  a new test or recorded as equivalent with a reason.
- **Contracts.** Wire schemas are generated from the server's Rust types and
  published in `shared/v1/`. An incompatible change requires a new major
  version of the schema bundle and coordinated consumers.
- **Migrations.** Released migrations are immutable (their checksums are
  verified by existing installations). Data-dependent migrations may refuse to
  downgrade; document the reconciliation instead.
- **Evidence.** Distinguish code read, test reproduced, hardware measurement and
  assumption ([`docs/05-methodology.md`](docs/05-methodology.md)). Lint and tests
  do not prove capacity, security or production readiness.

## 7. Security and privacy

- Never commit, log or paste secrets, credentials, tokens, personal data,
  production data, camera recordings with identifiable people or vehicles, or
  customer deployment details. Use synthetic data in tests and examples.
- Agents do not read `.env` files, secret files or credentials, and do not use
  data from other projects to reconstruct context.
- Secrets are read from private files (`NAME_FILE`) with minimum lengths; keys
  never live in the database, its backups or Git.
- No custom cryptography or authentication protocols. Personal data needs a
  written purpose, a location, a protection and a retention period before it is
  collected.
- Report vulnerabilities privately as described in [`SECURITY.md`](SECURITY.md).
  Do not describe an unresolved vulnerability in an issue, pull request, commit
  message or document.

## 8. Documentation

Update [`docs/`](docs/README.md) and `CHANGELOG.md` in the same pull request as
any major change: a component or runtime, a wire contract, the installation or
upgrade procedure, a security or privacy control, a measured result, goal or
known limitation, or a recorded design decision. Repository text is written in
English. Product documentation is changed only within an approved plan.

## 9. Agent sessions and handoffs

- Agents follow this file regardless of vendor or tool. Maintainers may use
  private agent configuration (skills, memory); it may add detail but never
  relaxes these rules.
- When work passes from one agent to another, leave a short handoff: objective,
  branch and commit, files changed, tests actually run with results, and pending
  items. The next agent verifies the Git state before continuing. A handoff
  transfers context, not authorization for new tasks.
- Current priorities and open items are in
  [`docs/08-goals-and-roadmap.md`](docs/08-goals-and-roadmap.md) and the
  *Unreleased* section of `CHANGELOG.md`.
