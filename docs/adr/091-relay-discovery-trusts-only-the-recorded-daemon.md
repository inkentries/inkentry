# ADR-091: The relay path hands the team bearer only to the daemon this CLI recorded

**Date:** 2026-08-20
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** settles the question
[ADR-085](085-lifecycle-commands-signal-only-a-recorded-process.md) left open in
its *Future work*, and supersedes that paragraph alone. Operates inside
ADR-037 P2's relay contract without changing what the relay sends or where it
sends it. Takes [ADR-056](056-oss-server-tenancy-model.md)'s tenancy model as
given, which is why the check is recorded local state rather than a key: on a
loopback bind there is no key to check with.

## Context

`probe_local_relay_port` (`cli/cmd/server.rs:447`) reads the port out of
`server.port`, gets `/v1/health` on it, and returns the port if anything
answered. Nothing checks that the responder is the daemon this CLI started.

Both callers live in `cli/cmd/memory/outbox.rs`: `nudge_after_write` (`:141`)
after every `local_first` write, and `poll_and_apply` (`:235`), reached from
`inkentry status` and from `memory list`/`search`/`show`/`timeline` and
`inkentry context`. Both call `register_and_push`, which POSTs to whoever
answered (`:203`):

- the `title` and `body` of every unsynced, unarchived row, up to 200; and
- `bearer` from `ensure_fresh_server_key` (`:184`).

That bearer is a self-hosted server key returned verbatim, or a WorkOS access
token refreshed first if it had expired. Under ADR-056 a self-hosted key is not
one user's credential; it is the tenancy boundary for the whole instance.

**The recorded port is guessable.** It is `DEFAULT_SERVER_PORT` on any ordinary
install, because that is what both callers of `ensure_server_running` pass
(`init.rs:125`, `outbox.rs:138`), and `find_available_port` takes the preferred
port when it is free. An attacker does not need to read the `0700` state
directory. They need the documented default free, which every unclean daemon
exit leaves it: every `cleanup_state_files` call site is in-process.

**Why now.** ADR-085 declined to verify this path because the CLI had no
recorded fact to verify against. It now has three: `server.pid`, `server.port`
and `server.instance_id`, written at start into an `0700` directory, plus a
`process_matches_server` predicate. The reason for parking is the thing that
changed.

**What decides the severity.** Disclosed source chunks are recoverable and the
response is scoped to the affected material. A disclosed bearer is not: until
someone notices and rotates, it grants read and write on the team corpus over
the network, and rotating a shared self-hosted key is an event for every member
of the instance.

## Decision

**A local responder may receive a memory body or the team bearer only when it
matches what this CLI recorded about the daemon it started. Where it does not,
the CLI sends nothing and does not look elsewhere.**

### D1 - verify before returning a port

`probe_local_relay_port` returns a port only when both hold:

1. `server.pid` names a process that passes `process_matches_server`; and
2. the `instance_id` in the `/v1/health` body equals the recorded
   `server.instance_id`.

A missing state directory, a missing pid or recorded id, a pid that no longer
reads as an `inkentry-server`, or a reported id that differs are each a
refusal. The instance id is the load-bearing half: `process_matches_server` is
a substring match over argv that both false-negatives and false-positives.

### D2 - one predicate, not two spellings

The inference path, `probe_loopback`, applies the same pair. Both must call one
shared predicate rather than each carrying its own two steps. Two spellings of
"is this our daemon" is how a later fix lands on one path and not the other,
which is the state this record exists to end.

A change to what identity means, a start time, an executable inode, a real
local-caller identity, changes both paths or is not made. The paths may keep
different *consequences* for a refusal, because their callers differ, but not
different *criteria*.

### D3 - a refusal returns `None` and nothing falls through

No fallback to another port. A process holding the recorded port is usually
holding the default too, so a fall-through re-finds the same responder through
a step that asks it nothing, and the check becomes decoration.

Both callers already handle `None`. `nudge_after_write` returns early: rows
stay in the outbox with `remote_id` unset and are offered again on the next
nudge, poll, or explicit `inkentry sync`, so the sync is late, not skipped.
`poll_and_apply` returns `None`, so `status` prints no pending line. The cost
of a refusal is a delay; the cost of the alternative is a credential.

A refusal must not be silent. It names which check failed and that the remedy
is `inkentry server stop && inkentry server start`.

### D4 - `outbox.rs` uses the constant, not the literal

`outbox.rs:138` passes a literal `4655` where `init.rs:125` passes
`DEFAULT_SERVER_PORT`. ADR-089 D1 gave that number one home because a default
drifts between the code that binds it and the code that goes looking for it,
and here the drift sits inside the path being hardened.

## Alternatives considered

1. **Defer past v1.** Rested on the recorded port being unguessable. For a
   default install it is the number in the documentation.
2. **Strip the bearer and let the daemon resolve it.** Structurally impossible:
   the detached daemon never opens the OS keychain, enforced by a source-level
   CI scan, so the bearer must arrive in the request.
3. **Verify the write nudge only.** Both callers reach `register_and_push`, so
   the credential would stay reachable through `inkentry status`, the command a
   user runs when something already looks wrong.
4. **Instance id only, no pid.** Nearly sufficient, since the id is the strong
   half. Rejected on D2: one path running one check is a second definition of
   the trusted daemon.

## Consequences

- **A daemon started by an earlier build is not relayed to until restarted.**
  It recorded no instance id, so it fails check 2. The inference path reaches
  the same users at the same moment with the same one-line remedy.
- **A stranded `server.port` costs a delay instead of a credential.** The
  honest cost is that a healthy daemon whose state files were disturbed is no
  longer relayed to.
- **Tests.** A recorded daemon that still matches is still used (the regression
  guard that stops the verification being satisfied by refusing everything); a
  squatter on the recorded port receives neither entries nor bearer, and no
  fallback re-finds it; a responder reporting a different id is refused. The
  squatter test must assert that no request carrying the credential reached it,
  not merely that no port was returned.
- **Relay tests that fabricate a `server.port`** (`outbox.rs:437`,
  `status.rs:1097`/`:1211`, `tests/e2e_tests/adr037_p2_auto_start_scope.rs:313`)
  must record what a real start records, or move to a documented override.
- **Documentation.** `docs/security/THREAT-MODEL.md`'s relay section gains the
  responder-identity threat as closed; its "no route returns the bearer"
  sentence is a claim about the daemon and should say so, since a squatter is
  not a route but the whole listener.
- **A later deferral on this surface has to answer this record**, rather than
  restating that the signal does not exist.

## What would falsify this

The port is reachable in the ordinary case: falsified by an auto-started daemon
that does not use the documented default, or by the port file being removed on
an unclean exit.

The recorded instance id is a sound signal: falsified by any route that returns
it to an unauthenticated caller, or any derivation of it from public facts.

## Acceptance criteria

1. `probe_local_relay_port` returns `Some(port)` only after both D1 checks
   pass, and otherwise returns `None` with a message naming the failed check
   and the restart remedy.
2. One definition of the pid-plus-instance-id test exists in the tree, called by
   both discovery paths.
3. No path reached from a refusal probes another port or another responder.
4. `register_and_push` is unreachable with a bearer unless D1 passed, from both
   callers.
5. `outbox.rs` passes `DEFAULT_SERVER_PORT`, and no non-test caller of
   `ensure_server_running` under `crates/inkentry-cli/src/` passes a literal.
6. The relay's wire shapes, routes and session semantics are unchanged. A diff
   touching `crates/inkentry-server/src/relay/` means this record was read
   wrongly.
