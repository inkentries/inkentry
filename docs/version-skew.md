# Version skew

[Stability contract](stability.md) says what a surface promises within one
version. This document says what happens when the two ends are *different*
versions, which is the normal case rather than the exception.

The CLI talks to three server-side peers, and they drift independently:

| Peer | How it is reached | Why it drifts |
|---|---|---|
| **Loopback server** | auto-discovered on `127.0.0.1:7777` | The CLI starts and manages it, so it is normally the same version. It can go stale when a long-running daemon outlives an upgrade. |
| **Team `spelunk-server`** | explicit `server_url` | Upgraded on someone else's schedule. Skew is guaranteed, in both directions. |
| **cloud-api** | explicit `server_url` | Deployed continuously, so it is effectively always newer than any released CLI. |

Drift can enter from any of the three independently. A newer CLI meets an older
server, and an older CLI meets a newer server, in the same week.

## The support window

| Pairing | Supported range |
|---|---|
| CLI *n* to team server | *n-1*, *n*, *n+1* |
| CLI to loopback server | same version |
| CLI to cloud-api | any; cloud-api evolves additively within `/v1/` |

The team-server window is one minor version in each direction. It is *not* a
promise that wider gaps fail, and in practice they often work: it is the range
that is tested, and therefore the range a break is treated as a bug in.

`GET /v1/health` carries the peer's real version in its `version` field, which
is how any of this is observable at runtime. `info.version` inside the OpenAPI
spec is a placeholder and must not be used for this.

### Outside the window

Outside the window the CLI keeps working on a best-effort basis rather than
refusing to run. That is a deliberate choice and worth stating, because the
alternative reads as safer than it is: a hard version gate turns every
"upgrade the CLI before the server" ordering into a total outage, including for
the person who is upgrading the server. A soft failure that names the versions
is more recoverable than a hard one that is correct in principle.

What is guaranteed instead:

- Absent optional fields fall back to a documented conservative default, never
  to an optimistic one. An older peer that omits `limits` is treated as
  enforcing the legacy budget, never as having no limit.
- Unknown fields, and unknown values in an open enum, are ignored rather than
  failing the request that carried them.
- A genuinely incompatible response produces a diagnostic naming the peer URL,
  not a panic or a silent empty result.

## What evolution is allowed

The `/v1/` rules in [Stability contract](stability.md) already cover this:
additive only within a major version. Version skew is what makes that rule
load-bearing rather than tidy, so it is worth restating the direction each side
has to tolerate:

- **A newer peer sends fields the CLI has never seen.** The CLI must ignore
  them. This includes new values in an existing enum field, which is the case
  most likely to be mistaken for a parse error: an unrecognised value must
  degrade that one field, never the whole response.
- **An older peer omits fields the CLI expects.** The CLI must supply the
  documented default. Every optional in the health body already does this.

## A live cross-peer divergence

The two peers publish incompatible types for the same conceptual Project
resource, both as documented contracts:

| Peer | Field | Type |
|---|---|---|
| cloud-api | `ProjectItem.id` | `string`, format `uuid` |
| `spelunk-server` | `Project.id` | `integer`, format `int64` |

This is live today, not a hypothetical. The CLI is unaffected for exactly one
reason: it never holds a typed project id. It carries the identifier as an
opaque string and spends it as a single percent-encoded path segment, so both
peers' shapes pass through untouched.

That immunity is load-bearing and invisible in the type signature, so it is
pinned by `project_id_stays_opaque_across_both_peers_id_types` in
`crates/spelunk-cli/src/server_client.rs`. Narrowing the project id to an `i64`
or a `Uuid` would make the CLI incompatible with one peer or the other; the
test exists to make that a loud failure rather than a discovery in production.

Reconciling the two peers is out of scope for this repository, which owns only
one of them.

## Enforcement, and what it is worth

| Promise | Enforced by | Against what |
|---|---|---|
| Absent optionals degrade to documented defaults | `recorded_legacy_peers_degrade_to_documented_defaults` | Real recorded peer responses |
| Present optionals are actually read | `recorded_current_peers_parse_their_optional_objects` | Real recorded peer responses |
| Unknown fields and enum values are ignored | `unknown_fields_from_a_newer_peer_are_ignored` | Real recorded response, unknown fields added |
| The project id stays opaque | `project_id_stays_opaque_across_both_peers_id_types` | Two peers' published id shapes |
| Two real binaries complete the memory flow | `scripts/skew-smoke.sh`, run both ways by `.github/workflows/version-skew.yml` | Real released binaries |
| `/v1/` matches `docs/openapi.json` | `openapi-snapshot` job in `.github/workflows/ci.yml` | The running binary |

### Provenance of the fixtures

This matters more than it usually would. Almost every peer in this repository's
tests is a mock written to the shape we *believe* that peer has, which means
almost nothing here can falsify a premise about a real peer. Where a fixture is
real, that is worth knowing; where it is not, that is worth knowing more.

**Recorded from a real released binary** (`crates/spelunk-cli/tests/fixtures/skew/`):

| File | Source |
|---|---|
| `health-v0.8.0.json` | `GET /v1/health` from the released v0.8.0 `spelunk-server` |
| `health-v0.9.0.json` | `GET /v1/health` from the released v0.9.0 `spelunk-server` |
| `health-v0.9.4-loading.json` | released v0.9.4 `spelunk-server`, embedder still loading |
| `health-v0.9.4-ready.json` | released v0.9.4 `spelunk-server`, embedder ready |
| `health-v0.9.5-loading.json` | current build, embedder still loading |
| `health-v0.9.5-ready.json` | current build, embedder ready |
| `openapi-v0.9.4.json` | `spelunk-server --print-openapi` from the released v0.9.4 binary |

The v0.8.0 and v0.9.0 bodies are the interesting ones: they genuinely omit
`embedder`, `embedding_dim`, and `limits`, so the absent-optional path is
exercised by a peer that really did behave that way rather than by a synthetic
body asserting our belief about one.

**Hand-written to our belief:** the `health_body()` helper in
`crates/spelunk-cli/src/capability/probe.rs`, which predates this document, and
the unknown-field additions grafted onto the recorded v0.9.5 body (no peer
sends those fields yet, by construction: they stand in for a future one).

**Not represented at all:** cloud-api. Its schema lives in another repository
and is not vendored here, so no test in this repo validates anything against
it. The divergence table above is transcribed from its published schema, and
will go stale silently. Treat it as a note, not as a check.

### What these tests cannot tell you

The smoke test is the only part that puts two independently built artifacts on
a socket together, and it is therefore the only part that can contradict our
model of a peer rather than confirm it. Everything else, including the recorded
fixtures, is a replay: a recorded response proves what a peer *did* send once,
not what it will send under a different configuration, a different embedder
state, or a different deployment.

Two specific limits worth naming:

- The smoke test's search step depends on the server-side embedder having
  loaded a model, which is not a wire-contract property. It waits for the
  embedder to settle and, if it never does, accepts the documented
  not-ready refusal instead of a result. An earlier draft that did not wait
  produced a convincing false positive: an old CLI appearing to fail against a
  new server, purely because that server was a debug build still warming up.
- The smoke test refuses to run two identical versions against each other,
  because a skew test that is not skewed passes while proving nothing.

## What's next

- [Stability contract](stability.md): what each surface promises within a version
- [Server setup](server-setup.md): running a team server
- [Releasing](releasing.md): how a version is cut
