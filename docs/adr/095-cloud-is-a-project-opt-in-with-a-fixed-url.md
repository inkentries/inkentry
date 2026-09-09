# ADR-095: Cloud is a project opt-in with a fixed URL, and the access token is bound to its issuing origin

**Date:** 2026-09-02
**Deciders:** architect (this record); founder (Johan), direction to resolve in the 1.0.2 patch
**Relationship to prior ADRs:** amends
[ADR-071](071-per-server-client-bearer-scoping.md) D2. That decision decided the
credential kind from the resolved `server_url`'s origin matching the cloud
origin, where the cloud origin was `DEFAULT_CLOUD_URL` overridable by
`INKENTRY_CLOUD_URL`. This record separates the two: cloud is selected by a
project opt-in rather than by pointing `server_url` at the cloud address, and
the access token is released only to the origin it was issued for. The
per-origin server-key map from [ADR-088](088-retire-legacy-server-key-tiers.md)
is unchanged.

## Context

There is no setting that says "this project uses the hosted cloud." Using cloud
is expressed by pointing `server_url` at the cloud address, and `bearer_for`
(`crates/inkentry-core/src/config/server_keys.rs`) decides that address is cloud
by matching its origin against `cloud_origin()`, which reads `INKENTRY_CLOUD_URL`
at request time (falling back to the compile-time `DEFAULT_CLOUD_URL`).

Two consequences follow. First, selecting cloud requires supplying its URL,
through `server_url`, which is otherwise the setting for a self-hosted team
server. Second, the URL that counts as cloud is an environment variable a
caller can set. Point `INKENTRY_CLOUD_URL` and `INKENTRY_SERVER_URL` at one host
they control and a logged-in CLI treats that host as cloud and sends it the
WorkOS access token, with no code execution, process, listener, or state
directory involved. The path reaches logged-in cloud sessions only: a
self-hosted key is looked up per origin with no global fallback (ADR-088), so an
attacker origin resolves to no server key and the token is the only asset on
the path.

## Decision

### D1 - cloud is a project opt-in, not a URL the caller supplies

`.inkentry/config.toml` gains a `cloud` flag. When set, the CLI targets the
hosted cloud for the remote memory and inference it would otherwise send to a
`server_url`. `server_url` returns to meaning a self-hosted team server only,
and setting both `cloud` and `server_url` is a configuration error.

Because cloud is a flag rather than an address, `INKENTRY_SERVER_URL` no longer
selects the hosted cloud. Whether a request carries the access token is decided
by D3, the origin the token was issued for, not by pointing an address at the
cloud.

### D2 - the cloud URL is fixed, with a development override that is not a user setting

The hosted cloud URL is the compile-time `DEFAULT_CLOUD_URL`. `INKENTRY_CLOUD_URL`
remains, as the way an inkentry developer points a build at a development cloud,
and is not a documented user setting. It is honoured in shipped builds, which is
safe because of D3: it changes which cloud a request reaches, not which origin a
stored token may be sent to.

### D3 - the access token is released only to the origin it was issued for

The stored cloud credential records the origin it was authenticated against.
`inkentry login` and `inkentry org switch` set it to the normalized origin of
the cloud URL they authenticated to; a token rotation carries it forward, since
a refresh does not change the host. `bearer_for` releases the access token only
when the request origin equals that recorded origin; a credential that carries
no origin matches nothing and resolves to no bearer.

A token issued for production is therefore never sent to a host that
`INKENTRY_CLOUD_URL` points at, and a developer who logs in against a
development cloud is trusted with that cloud because they logged in against it.
This is the amendment to ADR-071 D2: the cloud kind is keyed on the credential's
issuing origin, not on an environment-derived cloud origin.

## Consequences

- **Security.** The environment-controlled disclosure of the access token is
  closed from both directions: `server_url` can no longer be turned into cloud,
  and `INKENTRY_CLOUD_URL` can move where a request goes but not which origin a
  stored token is sent to.
- **Documentation.** `docs/config-reference.md` documents the `cloud` flag and
  reframes `INKENTRY_CLOUD_URL` as a development override, not a user URL.
  `docs/commands.md` follows. `docs/security/THREAT-MODEL.md` stops listing this
  path as an open residual and records it as closed.
- **CHANGELOG.** An `### Added` entry for the `cloud` opt-in and a `### Security`
  entry for the token now being sent only to the host it was issued for.
- **ADR-071.** D2 gains a line recording that cloud is a project opt-in and the
  cloud kind is keyed on the credential's issuing origin.
- **Tests.** `bearer_for` gains coverage that a stored token is released only to
  its issuing origin and withheld for an origin an override points at; config
  resolution gains coverage that `cloud = true` targets the fixed cloud URL,
  that `cloud` with a `server_url` is refused, and that the login and rotation
  paths record and preserve the issuing origin.
