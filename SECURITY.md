# Security policy

## Scope and supported versions

This policy covers the Estaciona AI core: server, gateway, vision, model export,
forecasting, contracts and repository automation. Customer deployments and the
web hub have separate configurations and must be assessed independently.

Security fixes target the current `main` release line and `develop`. The current
release line is `0.9.x` (pilot); earlier `alpha-*` and `beta-*` releases are not
maintained. A pilot release does not certify an installation as production ready.

## Report a vulnerability privately

Email **guilhermebarb0sa@proton.me**, the maintainer's published contact, with
"Estaciona AI security" in the subject. Do not disclose exploit details in a
public issue or pull request. GitHub private vulnerability reporting is not
assumed to be enabled; use email until that channel is available.

Include the affected version or commit, component, prerequisites, expected and
observed behavior, impact, and minimal reproduction using synthetic data. Never
send credentials, personal data, production database dumps or camera recordings
containing identifiable people or vehicles.

Test only systems you own or have permission to assess. Do not probe university
or customer installations, interfere with reservations, or change live occupancy
when preparing a report. The maintainer will coordinate investigation and a fix;
there is no guaranteed response time or bug bounty program.

## Dependency review

Keep application lockfiles committed and audit them against current advisory
sources. A successful scan detects known advisories; it is not a security
certification. Review findings against the enabled features and actual call paths.
An exception must identify the advisory, affected component, rationale and review
date. It must be revisited when dependencies, algorithms or deployment paths change.
Never suppress all findings for a package or mark an unreviewed failure successful.

Application secrets belong outside Git, logs and diagnostic attachments. Rotate
an exposed credential through its owning service; deleting it from Git alone does
not revoke it.
