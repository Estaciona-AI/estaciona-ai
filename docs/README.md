# Estaciona AI core documentation

This directory is the technical record of the Estaciona AI core: what the system
is, how it is installed and verified, which decisions shaped it, what is known
and what is still assumed. It complements, and does not replace, the
authoritative operational sources listed below.

## Document control

| Field | Value |
|---|---|
| Scope | Core repository: server, gateway, vision, forecast worker, contracts, infrastructure and tools |
| Release baseline | `v0.9.0` (pilot, 2026-10-23) plus the unreleased changes on `develop` |
| Reviewed against | `develop` at `9036d10` on 2026-10-27 |
| Language and style | English; concise academic register; claims carry evidence or are marked as assumptions |
| Audience | Engineers, reviewers and partners evaluating the core; public repository |

The reviewed `develop` includes changes not present in `v0.9.0`. Descriptions of
the current code do not imply that those changes were included in that release.
The dated [verification snapshot](07-testing.md#74-verification-snapshot)
identifies the executions checked for this revision.

## Contents

| No. | Document | Purpose |
|---|---|---|
| 1 | [Overview](01-overview.md) | Problem, scope, components and terminology |
| 2 | [Architecture](02-architecture.md) | Components, data flows, interfaces and trust boundaries |
| 3 | [Installation](03-installation.md) | Development setup, deployment sequence and verification |
| 4 | [Research](04-research.md) | Research questions and design decisions with alternatives |
| 5 | [Methodology](05-methodology.md) | Evidence standards, experimental and engineering methods |
| 6 | [Evidence register](06-evidence-register.md) | Verified facts, assumptions and open questions |
| 7 | [Testing](07-testing.md) | Test strategy, continuous integration and verification snapshot |
| 8 | [Goals and roadmap](08-goals-and-roadmap.md) | Objectives, success criteria and status |
| 9 | [Trade-offs](09-trade-offs.md) | Gains and losses of each major decision |
| 10 | [Security and privacy](10-security-and-privacy.md) | Principles, controls and data protection |
| — | [References](references.md) | Cited literature and standards |

## Authoritative sources

When this directory and one of the following disagree, the source below is
correct and this directory must be fixed.

- Source code and tests: behaviour.
- [`CHANGELOG.md`](../CHANGELOG.md): released and unreleased changes, upgrade notes
  and known limitations.
- [`.env.example`](../.env.example) and [`infra/docker-compose.yml`](../infra/docker-compose.yml):
  configuration.
- [`SECURITY.md`](../SECURITY.md): vulnerability reporting.
- [`LICENSING.md`](../LICENSING.md), [`LICENSE`](../LICENSE) and [`NOTICE`](../NOTICE):
  licensing.
- Published JSON Schemas (`shared/v1/`): wire contracts.

## Conventions

- **Evidence classes.** Every quantitative or behavioural claim states its
  class: *code* (read in the repository), *test* (reproduced by an automated
  test), *measurement* (obtained on named hardware under a named protocol) or
  *assumption* (not yet verified). See [Methodology](05-methodology.md#1-evidence-standard).
- **Identifiers.** Facts (`F-nn`), assumptions (`A-nn`), open questions
  (`Q-nn`), research questions (`RQ-n`), decisions (`D-nn`) and goals (`G-nn`)
  keep their identifiers for traceability. A retired item is marked, not
  renumbered.
- **Dates and versions.** Measurements carry their date, commit or release.
  Relative expressions such as "recently" are not used.
- **Pull requests.** `#nnn` refers to a pull request of this repository.

## Maintenance

Update this directory in the same pull request as any major change, and at
least before each release. A change is major when it alters one of the
following: a component or its runtime; a wire contract or its version; the
installation or upgrade procedure; a security or privacy control; a measured
result, goal or known limitation; or a design decision recorded here. When
updating, revise the *Reviewed against* row above.

This directory is public. It must not contain secrets, credentials, personal
data, customer-specific deployment details, internal host names, unresolved
vulnerability details or any other information that would help an attacker.
Security findings are reported privately as described in
[`SECURITY.md`](../SECURITY.md) and are documented here only after resolution,
at the level of the control that addresses them.
