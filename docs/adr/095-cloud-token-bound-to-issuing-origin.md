# ADR-095: The cloud access token is released only to the origin it was issued for

**Date:** 2026-09-02
**Deciders:** architect (this record); founder (Johan), direction to resolve in the 1.0.2 patch
**Relationship to prior ADRs:** amends
[ADR-071](071-per-server-client-bearer-scoping.md) D2. That decision keyed the
cloud-kind bearer on the resolved `server_url`'s origin matching the cloud
origin, where the cloud origin is `DEFAULT_CLOUD_URL` overridable by
`INKENTRY_CLOUD_URL`. This record narrows that: the cloud access token is
released only to the origin the token was actually issued for. It does not
reopen [ADR-088](088-retire-legacy-server-key-tiers.md)'s two-tier collapse or
the per-origin server-key map, which are unchanged.

## Context

`bearer_for` (`crates/inkentry-core/src/config/server_keys.rs`) resolves the
credential for an outbound request. For the cloud kind it returns the WorkOS
access token from `[auth]`, and it decides a request is cloud-kind when the
request origin equals `cloud_origin()`, which reads `INKENTRY_CLOUD_URL` at
request time (falling back to `DEFAULT_CLOUD_URL`).

The disclosure decision is therefore keyed on an environment variable rather
than on any property of the credential. A caller who can set `INKENTRY_CLOUD_URL`
and `INKENTRY_SERVER_URL` to one host they control makes a logged-in CLI treat
that host as cloud and send it the access token. The path needs no code
execution, no running process, and no writable state directory: two environment
variables are sufficient, which makes it the cheapest environment-controlled
disclosure of the access token. It reaches logged-in cloud sessions only, since
a self-hosted key is looked up per origin with no global fallback (ADR-088), so
an attacker origin resolves to no server key and the token is the only asset on
the path.

The threat model recorded this as a residual on the same footing as the local
relay residuals. That footing does not hold here. The relay residuals stay open
because closing them needs a local-caller identity the loopback posture does not
provide. This path needs no such identity: the token is minted against a
specific cloud host at login, so the information required to refuse the
disclosure already exists at the moment the token is stored. The environment
override participates in the trust decision only because that provenance is
discarded.

## Decision

### D1 - record the issuing origin on the stored credential

The persisted cloud credential records the origin it was authenticated against.
`inkentry login` and `inkentry org switch` set it to the normalized origin of
the cloud URL they authenticated to; a token rotation carries the existing value
forward, because a refresh does not change the host. The field is optional in
the persisted form so a credential written by an older client still loads.

### D2 - release the access token only to its issuing origin

The cloud kind in `bearer_for` fires when the request origin equals the
credential's recorded issuing origin, not when it equals the request-time
`INKENTRY_CLOUD_URL`. A stored token is therefore sent to the one host it was
issued for and to no other, whatever the environment says at request time.

A credential with no recorded issuing origin (one persisted before this change)
is treated as issued for the compile-time `DEFAULT_CLOUD_URL`. A production
login keeps working with no user action. A pre-existing login against a
non-default cloud host stops carrying the token until the next `inkentry login`,
which is a fail-closed outcome that discloses nothing and is recovered by
logging in again.

### D3 - `INKENTRY_CLOUD_URL` selects a cloud, it does not confer trust

`INKENTRY_CLOUD_URL` (and the `--cloud-url` flag) keep choosing which cloud API
`login` and `org` reach and which WorkOS client id is used. They no longer
decide which origin the stored access token may be sent to. Reaching a staging
or self-hosted cloud still works: logging in against that host records it under
D1, so requests to it carry the token, while an origin that only became "cloud"
through a request-time override does not. This is the amendment to ADR-071 D2:
the cloud kind is keyed on the credential's issuing origin, not on an
environment-derived cloud origin.

## Consequences

- **Security.** The environment-controlled disclosure of the access token is
  closed. The token's destination is a property of the credential, which a
  caller who controls only the environment cannot forge.
- **Workflows preserved.** Staging and self-hosted-cloud logins keep working,
  because the host they authenticate against is the host they are trusted with.
- **Backward compatibility.** A production login is unaffected. A pre-existing
  non-default-cloud login re-authenticates once; recorded here so the
  re-login is expected rather than read as a regression.
- **Documentation.** `docs/config-reference.md` and `docs/commands.md` stop
  describing `INKENTRY_CLOUD_URL` as only a `login` / `org` URL, since it also
  no longer governs token trust. `docs/security/THREAT-MODEL.md` stops listing
  this path as an open residual and records it as closed.
- **CHANGELOG.** A `### Security` entry naming the closed disclosure and the
  one-time re-login for a non-default-cloud session.
- **ADR-071.** D2 gains a line recording that the cloud kind is keyed on the
  credential's issuing origin.
- **Tests.** `bearer_for` gains coverage that a stored token is released only to
  its issuing origin and refused for an origin that only matches through a
  request-time override; the login and rotation paths gain coverage that the
  issuing origin is recorded and preserved.
