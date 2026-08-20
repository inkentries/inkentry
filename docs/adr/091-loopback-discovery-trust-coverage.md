# ADR-091: Loopback discovery trust checks, their coverage, and the accepted Windows gap

**Date:** 2026-08-20
**Deciders:** founder (Johan); architect (this record)
**Relationship to prior ADRs:** settles the `probe_local_relay_port` question
[ADR-085](085-lifecycle-commands-signal-only-a-recorded-process.md) left open in
its *Future work* section, by extending that gate with the same recorded-pid and
recorded-`instance_id` check step 3a uses. It does not reopen ADR-085's ruling
that a lifecycle command may not *signal* a process on the strength of a port or
an argv match: this record is about which responder receives *data*, not which
process receives a signal. It builds directly on the loopback-discovery
verification added for a responder before it becomes the embedding backend, and
records the test consequences of that change rather than altering its behaviour.

## Context

Loopback auto-discovery now verifies a responder before handing it indexed
source. Step 3a reads `server.port`, probes `GET /v1/health`, and uses the
responder only when the recorded `server.pid` still names an `inkentry-server`
process (`process_matches_server`) and the reported `instance_id` equals the one
recorded at start. The verification is exercised by three tests in
`crates/inkentry-cli/src/capability/probe.rs`
(`a_recorded_daemon_that_still_matches_is_discovered`,
`a_squatter_on_the_recorded_port_is_not_the_embedding_backend`,
`a_responder_with_a_different_instance_id_is_not_the_recorded_daemon`).

All three are `#[cfg(unix)]`. The positive pid case needs a live process whose
argv reads as `inkentry-server`, staged with a symlink to `/bin/sh`. Windows'
`tasklist` matches an image name, which a test cannot fabricate without shipping
a real binary under that name. So on Windows, which this project runs in CI, the
verification had no automated coverage at all, and the happy path that every
command travels when a daemon is running had exactly one test, on one platform.

Separately, roughly twenty integration helpers that used to fabricate a
`server.port` file to point discovery at a mock were moved to the pre-existing
`INKENTRY_TEST_DISCOVERY_PORT` override. That move was correct on its own terms
(their subject is embedding-tier routing, not the state-file path), but it left
step 3a exercised by one unix-only unit test rather than by the integration
suite.

This record settles three questions that follow, and one related one.

## Decision

### 1. The trust policy is split from the OS query, and tested on every platform

`untrusted_responder` in `capability/probe.rs` now gathers the recorded facts
(the recorded pid, whether that pid still names a server, the recorded
`instance_id`) and hands the decision to a pure `classify_responder`. The OS
process query is supplied to `classify_responder` as a bool rather than run
inside it. The policy, including the happy path (`None`, trust the responder),
is unit-tested on every platform by passing that bool directly, which is the one
thing a Windows test cannot otherwise stage. The live-process tests stay
unix-only and now read as what they are: an end-to-end drive of `probe_loopback`
against a real recorded process, complementing the platform-independent policy
tests rather than being the only coverage.

This is a testability seam, not a change to what the check decides. The order of
refusals and the trusted case are identical to before.

### 2. `process_matches_server`'s exact semantics are pinned

The substring test at the heart of `process_matches_server` is extracted to
`listing_names_server`, so its exact, deliberately weak behaviour can be pinned
without spawning a process. ADR-085 records that this check is wrong in both
directions; unit tests now hold that baseline: a pre-rename `spelunk-server`
fails it (the documented false negative), an unrelated process whose argv
contains the string passes it (the documented false positive), and the Windows
image-name match folds case while the `ps` argv match does not. Pinning the
current rule is a precondition for any future attempt to strengthen it (resolved
executable path, start time, inode), so that such an attempt starts from a known
baseline rather than a guess about today's behaviour. ADR-085's *Non-goals* still
hold: this does not change `process_matches_server`, and it remains used only to
withhold a signal, never to source one.

### 3. Step 3a has integration-level coverage that does not depend on the host OS

A single subprocess test
(`crates/inkentry-cli/tests/security_tests/loopback_discovery_trust.rs`) drives
the real binary: it records a daemon (port, pid, `instance_id`) beside a mock
and asserts `inkentry status` discovers it and reports it as the auto-discovered
loopback server. It runs on every platform. Rather than converting one of the
twenty routing helpers back to step 3a, which would mix "a local server exists"
with "the state-file path works" in one test, this dedicated test's subject *is*
the state-file path, end to end through the binary.

Its cross-platform reach depends on the seam below.

### 4. A discovery-trust test seam, confined to discovery and off by default

`INKENTRY_TEST_TRUST_RECORDED_RESPONDER=1` makes the discovery-trust path treat
the recorded pid as a live server without running the OS query. It exists for
two reasons the query itself creates: the subprocess and relay tests stand up an
in-process server, which has no separate `inkentry-server` process to point a
real pid at; and the query's positive case cannot be staged on Windows at all.
It relaxes only that one un-fakeable signal: a pid must still be recorded, and
the recorded `instance_id` must still match what the responder reports, so a
test using it still exercises every check it can. It is modelled on the existing
`INKENTRY_TEST_DISCOVERY_PORT` override: documented, test-only, unset for every
real user, and fail-safe (any value other than `1`/`true` runs the real query).

Crucially, the seam is read only on the discovery-trust path and never by
`classify_running_server`. No value of it can widen the set of processes a
lifecycle command will signal, so it cannot undermine ADR-085.

### 5. `probe_local_relay_port` is tightened to the same check

ADR-085's *Future work* left `probe_local_relay_port` trusting the recorded port
alone: it returned the port to callers when anything answered `/v1/health` there,
with no proof the responder was the daemon this CLI spawned. That gate feeds the
memory-outbox relay, which then hands memory to the responder, so it routes
data, not merely the CLI's own next step. It now applies the identical
`untrusted_responder` check step 3a does, and returns `None` for a healthy
responder it cannot verify.

This is consistent with ADR-085, not a reversal of it. ADR-085 permits port
trust for deciding "what the CLI itself does next"; tightening only narrows what
gets reused, and narrowing is always safe under that ruling. What ADR-085
forbids, signalling a process on the strength of a port or an argv, is untouched.

## Consequences

- **Windows gains real coverage of the verification's policy and of step 3a end
  to end**, through the pure `classify_responder` tests, the `listing_names_server`
  semantics tests, and the cross-platform subprocess test.
- **The live-process pid query stays unix-only, and that is accepted.** A test
  cannot fabricate a Windows process whose image name is `inkentry-server`
  without shipping a second binary, which is not worth its weight. The
  cross-platform coverage above is the substitute, and the `listing_names_server`
  tests cover the Windows substring rule directly.
- **A second test-only environment variable now exists** alongside
  `INKENTRY_TEST_DISCOVERY_PORT`. Both are read by production code and inert
  outside the harness. The seam is deliberately confined so it can never affect a
  kill decision.
- **The relay-reuse gate now refuses an unverifiable responder.** A user who
  upgrades but does not restart their daemon records no `instance_id`, so the
  relay is not reused until they restart, matching the guidance already given for
  step 3a. `ensure_server_running` is unaffected: it takes its pid from
  `server.pid` and reclaims a wedged daemon as before.
- **The `adr037_p2` relay test keeps exercising its gate.** Without the seam and
  the recorded pid/`instance_id`, the tightened gate would refuse the in-process
  relay and the nudge under test would never be reached, so the test was updated
  to stay meaningful rather than pass vacuously.

## Verification (what would falsify this)

The claim is that the trust policy is fully covered off Unix and that the seam
cannot affect a signal. It would be falsified by a `classify_responder` outcome
reachable in production but not asserted by a platform-independent test, or by
any read of `INKENTRY_TEST_TRUST_RECORDED_RESPONDER` on a code path that
classifies a pid for termination. A grep for the variable shows a single reader,
on the discovery-trust path.

## Acceptance criteria

1. `classify_responder` has a platform-independent test for each refusal reason
   and for the trusted case.
2. `listing_names_server` has tests pinning both ADR-085 error directions and the
   per-platform case rule.
3. A cross-platform subprocess test discovers a recorded daemon through step 3a.
4. `probe_local_relay_port` refuses a healthy but unverifiable responder, and the
   seam is read only on the discovery-trust path.
