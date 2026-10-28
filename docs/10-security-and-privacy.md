# 10. Security and privacy

This chapter states the principles and the controls in place at the level a
public reader needs to assess the design. It deliberately omits deployment
specifics and any unresolved finding. Vulnerabilities are reported privately
as described in [`SECURITY.md`](../SECURITY.md).

## 10.1 Principles

1. **Minimize before protecting.** Personal data is collected only for a written
   purpose; data not collected cannot leak.
2. **Use mature primitives only.** Cryptography and authentication protocols
   come from maintained libraries; no custom ciphers, hashes or token formats.
3. **Separate custody.** Keys and secrets never live in the database, its
   backups, shared environment files, command-line arguments, logs or Git.
4. **Centralize identity.** Passwords and e-mail addresses exist only in the
   identity provider.
5. **Deny by default on the server.** Authorization is decided by the server
   from the session; client-supplied identifiers only select a resource.
6. **Isolate installations.** Each installation has its own process, database,
   data keys, identity client and edge credentials.

## 10.2 Controls by boundary

| Boundary ([Architecture §2.5](02-architecture.md#25-trust-boundaries)) | Controls |
|---|---|
| Browser → server | OpenID Connect authorization code flow with PKCE [4, 5]; host-only, `HttpOnly`, `Secure` session cookies; allowed-origin checks for cookie writes and CORS, against an exact list whose entries must be in the form a browser sends; single-use WebSocket tickets; role checks on the server |
| Gateway and worker → server | One revocable credential per edge, stored as a digest, with explicit grants checked on every event; admission limits and message size limits on sockets |
| Vision → gateway | Loopback-only listener; a local key distinct from the cloud credential, compared in constant time |
| Server and forecast worker → database | Runtime role without schema privileges; migrations applied separately by an owner role; refusal to start with an over-privileged role in production; a forecast worker that refuses to start if its role can modify any table; TLS with certificate and host-name verification for remote databases unless a private container network is declared, applied by the worker to every host its connection string names |
| Secrets | Read from private files owned by the service user, with minimum lengths enforced at start-up |
| Server container | Non-root user, read-only file system, all capabilities dropped, base images pinned by digest |
| Supply chain | Committed lockfiles used with `--locked`; actions pinned by digest; scheduled RustSec and Python advisory audits; license checks; dependency policy in `server/deny.toml` |

## 10.3 Personal data

| Category | Where it lives | Protection |
|---|---|---|
| Credentials, e-mail address | Identity provider only | Provider-managed password hashing; multi-factor sign-in for administrators |
| Account reference and generated username | Installation database | Pseudonymous references; still treated as personal data when linkable to a person |
| Display name and eligibility declarations | Installation database | AES-256-GCM [12] per field, bound to installation, column and row; keys outside the database |
| Reservations and parking history | Installation database | Access limited to the person and administrators; history deleted after 180 days |
| Vehicle plates | Not collected | The license plate module is disabled and refused in production |
| Camera images | Processed at the site | Not transmitted to the server; only spot status events leave the site. Frames are written to disk only by the evaluation tooling that builds frame corpora, and shown only by the opt-in local inspector |

Data subjects can export their data and delete their account through the API,
in support of the rights established by Brazil's General Data Protection Law
(LGPD) [13]. Administrative access to another person's data is recorded in an
append-only audit trail.

## 10.4 Assurance

Security claims follow the evidence classes of
[Methodology §1](05-methodology.md#1-evidence-standard): negative tests cover
other users, roles and installations, expired or revoked sessions, missing or
short secrets and disallowed origins. Automated scans detect known advisories
only; they are not a certification, and a pilot release does not certify an
installation as production ready.
