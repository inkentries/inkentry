# ADR-091: The relay path hands the team bearer only to the daemon this CLI recorded

**Date:** 2026-08-20
**Deciders:** founder (Johan); architect (this record)
**Relationship to prior ADRs:** settles the one question
[ADR-085](085-lifecycle-commands-signal-only-a-recorded-process.md) explicitly
left open in its *Future work*, and supersedes that paragraph alone. ADR-085's
Decision, its may/may-not lists and its refusal to signal a discovered process
are untouched, and the rule this record adopts is the same rule read in the
other direction. Operates inside ADR-037 P2's local-relay contract without
changing what the relay sends or where it sends it; see *Reading against
ADR-037* below. Takes [ADR-056](056-oss-server-tenancy-model.md)'s tenancy
model as given, which is why the check here is recorded local state and not a
key: on a loopback bind there is no key to check with. Implemented strictly
after [PR #174](https://github.com/inkentries/inkentry/pull/174), which creates
the recorded facts this record compares against.

## Context

### The path

`probe_local_relay_port` (`crates/inkentry-cli/src/cli/cmd/server.rs:447`) is
three statements: read the recorded port out of `server.port`, `GET
/v1/health` on it, return the port if anything answered. There is no check that
the responder is the daemon this CLI spawned. Its two callers both live in
`crates/inkentry-cli/src/cli/cmd/memory/outbox.rs`:

- `nudge_after_write` (`:141`), after every `local_first` `memory
  add`/`archive`/`supersede`;
- `poll_and_apply` (`:235`), reached from `inkentry status` and from `memory
  list`/`search`/`show`/`timeline` and `inkentry context`.

Both then call `register_and_push`, which POSTs to
`http://127.0.0.1:{port}/local/relay/push` (`:203`) a body carrying:

- the `title` and `body` of every unsynced, unarchived memory row, up to 200
  (`:170-181`); and
- `bearer`, from `ensure_fresh_server_key` (`:184`).

That bearer is whichever credential this machine holds for the configured team
`server_url`: a self-hosted server key returned verbatim, or a WorkOS cloud
access token, refreshed first if it had expired. Under ADR-056 the self-hosted
key is not one user's credential. It is the tenancy boundary for the whole
instance.

So the second caller matters as much as the first: a read command that nudges
the relay hands over the same credential as a write does.

### What PR #174 hardened, and what it left

PR #174 closed the same class of defect on the discovery path that picks the
embedding backend. A local process that held the recorded port became the
embedder and received every source chunk `inkentry index` produced. The fix
records the daemon's identity at start (`server.instance_id` beside
`server.pid`/`server.port`/`server.db-path`) and refuses a responder that does
not match it.

PR #174 names this record's subject in its own *What this does not do*:
`probe_local_relay_port` still trusts the port alone, and it hands that
responder memory bodies and the team bearer.

The asymmetry is the whole reason this is not a second-tier follow-up. The path
that was hardened leaked indexed source. The path left alone leaks a
credential.

### The reachability argument that would justify deferring does not survive

The obvious defence for leaving this path alone is that its port is not
guessable. `create_state_dir` sets `0700` on Unix, so another local user cannot
read `server.port`, whereas PR #174's step 3b fallback probes a fixed,
documented port that anybody can target.

That defence is wrong about the ordinary case, because the recorded port *is*
the documented default. Both real callers of `ensure_server_running` ask for
4655: `init.rs:125` passes `DEFAULT_SERVER_PORT` and `outbox.rs:138` passes the
literal. `find_available_port` (`server.rs:732`) binds the requested port when
it is free and only then falls back to an ephemeral one, which is
[ADR-089](089-default-port-range.md) D2's rule and which ADR-089's own
consequences state plainly: an upgrading user's daemon binds 4655 and discovery
looks there. For every user whose 4655 was free at start time, and that is the
default experience, the contents of the unreadable file are the number printed
in the docs.

The recorded port is also not self-cleaning. Every `cleanup_state_files` call
site is an in-process path inside `cmd_start`, `ensure_server_running` or
`cmd_stop` (`server.rs:481`, `:485`, `:588`, `:594`, `:599`, `:967`, `:996`).
A daemon that is killed, panics, or dies with the machine leaves `server.port`
behind naming a port it no longer holds.

Which reduces the attacker's task to: guess the documented default, and hold it
at a moment the daemon does not. That is the same reach PR #174 closed, against
a worse payload.

The one case where the `0700` argument does hold is a daemon that started under
contention and drifted to an ephemeral port. That case is real and this record
does not pretend otherwise. It is not the case the defence has to cover.

### What changed since ADR-085, and why its parking is retired

ADR-085 did not decide that this path is fine. It declined to build on the
signals that existed when it was written, and said so:

> Whether loopback discovery should be trusted at all for anything beyond
> "reuse this relay" is open and tracked separately [...] This record
> deliberately does not settle that.

The reason it was parked was that the tool had nothing sound to compare a
responder against. ADR-085's own falsification clause names what would change
that: an identity signal that ties a discovered process to a record this tool
wrote, and that an unrelated process cannot forge by choosing its own argv or
binding a port.

PR #174 builds precisely that signal. The `instance_id` is generated by the
daemon from its own database (`get_or_create_instance_id`), recorded by this
CLI into a `0700` directory when the daemon first answered, and compared
against what a responder reports. A process that squats the port cannot produce
it without already being able to read the state directory, at which point it is
the user.

The stated reason for parking is the thing that changed. That is what retires
the parking, not a change of appetite.

### Do not overstate the pid check

The pair is not two equal halves. `process_matches_server` is a substring match
over argv (`server.rs:209-235`), and ADR-085 documents it failing in both
directions: a pre-rename `spelunk-server` daemon is rejected though it is ours,
and any process whose argv contains `inkentry-server` is accepted though it is
not. The recorded instance id is the load-bearing half. The pid check is
retained because it is cheap, because it subtracts rather than adds, and
because using it here rather than a second variant of it keeps one notion of
identity in the tree.

## Decision

**A local responder may receive a memory body or the team bearer only when it
matches what this CLI recorded about the daemon it started. Where it does not,
the CLI sends nothing and does not look elsewhere.**

### D1 - `probe_local_relay_port` verifies before it returns a port

It runs the same two checks PR #174 applies to the embedding backend, in the
same order:

1. `server.pid` must still name a process that passes
   `process_matches_server`; and
2. the `instance_id` in the `/v1/health` body must equal the recorded
   `server.instance_id`.

A missing state directory, a missing pid, a missing recorded instance id, a
pid that no longer reads as an `inkentry-server`, or a reported id that differs
are each a refusal.

### D2 - the checks are reused, never reimplemented

PR #174 deliberately reused `process_matches_server` from
`cli/cmd/server.rs` rather than writing a second argv test, and reads both
state files through the module that writes them. This record extends that
constraint to the check as a unit: `probe_local_relay_port` must call the same
predicate PR #174 calls (`untrusted_responder` in `capability/probe.rs`, lifted
to a location both callers can reach), not a copy of its two steps.

Two spellings of "is this our daemon" is how a later fix lands on one path and
not the other, which is exactly the state this record exists to end.

### D3 - a refusal returns `None`, and nothing falls through

PR #174 chose to return nothing rather than continue to its fixed-port
fallback, on the grounds that a process holding the recorded port is usually
holding the default port too, so a fall-through hands the same responder the
work through a step that asks it nothing. That reasoning is adopted here
unchanged, and this record must not be read as licensing any fallback on this
path.

`probe_local_relay_port` already has exactly one failure value, `None`, and
both callers already handle it:

- `nudge_after_write` returns early. The rows stay unpushed in the local
  outbox with `remote_id` still unset, which is the same state they were in
  before the write, and `rows_for_sync` will offer them again on the next
  nudge, the next `poll_and_apply`, or an explicit `inkentry sync`. Nothing is
  lost and nothing is duplicated; the sync is late, not skipped. This is the
  contract the nudge already advertises: best effort, never a write error,
  never a non-zero exit.
- `poll_and_apply` returns `None`, so `inkentry status` prints no
  pending/last-synced line and the read commands apply nothing this run.

The user-visible cost of a refusal is therefore a delay, and the cost of the
alternative is a credential. The one thing a refusal must not do is go quiet:
it must say which check failed and that the remedy is `inkentry server stop &&
inkentry server start`, in the same shape PR #174's refusals print.

### D4 - one notion of "the trusted daemon", not two

After this change there are two discovery paths in the CLI, `probe_loopback`
for inference and `probe_local_relay_port` for the relay, and they answer one
question: is the thing on this port the daemon we started. They must answer it
with the same code and the same recorded facts. A future change to what
identity means (a start time, an executable inode, a real local-caller
identity) changes both, or it is not made.

This is also the boundary condition on D2. The two paths may keep different
*consequences* for a refusal, because their callers differ, but not different
*criteria*.

### D5 - the severity framing that decided the call

Content disclosure is recoverable. A team bearer is not.

An attacker who receives indexed source chunks has a copy of code they may well
be able to obtain another way, and the response is scoped to the affected
material. An attacker who receives the team bearer has, until somebody notices
and rotates, the team's memory corpus on the real server for reading and for
writing, over the network, from wherever they are. Under ADR-056 a self-hosted
key is shared, so rotating it is an event for every member of the instance, not
a password reset for one. A cloud access token is a live identity credential
that was deliberately refreshed to be valid before it was handed over.

This is the reasoning that made the same-verification ruling obvious rather
than proportionate, and it is recorded here because a later reader weighing
another deferral needs it.

### D6 - the `4655` literal folds into this change

`outbox.rs:138` passes the literal `4655` to `ensure_server_running` where
`init.rs:125` passes `DEFAULT_SERVER_PORT`. ADR-089 D1 gave that number a
single home precisely because a default drifts between the code that binds it
and the code that goes looking for it, and here the drift is inside the path
this record is hardening: the argument that the recorded port is the documented
default is one the reader has to reconstruct from a bare number.

It is a one-line substitution in a file the implementation is already editing,
so it is folded in rather than raised separately. A second home for a
deliberately single-homed constant is not worth its own record, its own branch,
or its own review.

## Reading against ADR-037

ADR-037 P2 owns the relay contract: what the CLI hands the daemon, what the
daemon does with it, and the fact that the remote hop outlives the CLI process
that queued it. This record was read against that contract and does not touch
it. The request body is unchanged, the routes are unchanged, the session
semantics and the peek/ack discipline are unchanged, and the destination of the
remote hop is unchanged. What changes is only which local listener the CLI is
willing to treat as the daemon.

The relay module has already answered the mirror-image question, which is worth
naming because it makes this record's shape obvious rather than novel. Its
module docs record that a `server_url` taken from a request body would let any
local process turn the daemon into an egress proxy, so every destination comes
from `RelayPolicy` and a request may only select among locally declared pairs.
That is the daemon refusing to trust its caller. This record is the caller
refusing to trust its daemon. The same surface, hardened from the other end,
and the second half of a symmetry the first half already assumed.

The relay's threat-model section carries three deliberate residual risks and
notes that "no route returns the bearer". That statement is true of the daemon
and was never a statement about the CLI. A squatter is not a route; it is the
whole listener. Recording this record's decision there is a documentation
consequence, listed below.

## Non-goals

- **Not a change to `process_matches_server`.** Its weakness is context here,
  as it was in ADR-085. It is used to withhold, never to source.
- **Not a reopening of ADR-085's Decision.** Nothing here authorises a
  lifecycle command to signal a process it did not record. The two records
  govern different verbs: ADR-085 constrains acting *against* another process,
  this one constrains *disclosing to* one. ADR-085's rule that corroboration
  may only subtract from the set of processes a command will touch is the
  identical rule applied here to the set of responders the CLI will talk to.
- **Not a local-caller identity for the relay's own routes.** The three
  residual risks in the relay's threat-model section need a local caller
  identity the loopback posture does not provide, and they are unaffected by
  this record either way.
- **Not a change to `ensure_server_running`'s own trust.** It already takes its
  pid from `server.pid` and classifies it, which sits inside ADR-085 and inside
  this record.
- **Not a redesign of the state-file lifecycle.** Making `server.port` harder to
  strand would shrink the window described above and is compatible with this
  decision. It is separate work.

## Alternatives considered

1. **Leave it parked until after v1 (rejected).** This is the option the
   founder ruled against, and the reachability finding is why: the deferral
   rested on the recorded port being unguessable, and for the default install
   it is the number in the documentation. Deferring would ship v1 with the
   hardened path protecting source chunks and the unhardened path exposing the
   credential that guards the corpus.
2. **Strip the bearer from the nudge and let the daemon resolve it (rejected).**
   It would end the disclosure without any identity check. It cannot be done:
   the detached daemon deliberately never opens the OS keychain, which is a
   structural property enforced by a source-level CI scan, and the bearer must
   therefore arrive in the request. Closing the disclosure this way means
   reintroducing a keychain prompt from a background daemon.
3. **Verify only for the write nudge, not the read poll (rejected).** Both
   callers reach `register_and_push`, so both send the bearer. Verifying one
   would leave the credential reachable through `inkentry status`, which is the
   command a user runs when something already looks wrong.
4. **A weaker check: instance id only, no pid (rejected).** It would very
   nearly work, since the instance id is the strong half. It is rejected on D4
   rather than on strength: the embedding path runs both, and a relay path
   running one is a second definition of the trusted daemon, which is the
   failure mode this record is closing.
5. **Refuse, then fall back to the default port (rejected).** Covered by D3.
   A squatter holding the recorded port is usually holding the default too, so
   the fallback re-finds the same responder through a step that verifies
   nothing, and the check becomes decoration.

## Consequences

- **Sequencing: this is implemented strictly after PR #174 merges, and that is
  a hard dependency rather than a preference.** The file the instance-id check
  compares against, `server.instance_id`, does not exist until PR #174's
  `record_instance_id` writes it, and the shared predicate the check must reuse
  is introduced by that PR. Implemented first, the check would compare against
  nothing, refuse every responder, and take the relay permanently offline.
  There is no partial ordering that helps here: this work starts when PR #174
  is on `main`.
- **The implementation is small, and should be.** After PR #174,
  `probe_local_relay_port` already receives the reported `instance_id` from
  `probe_health` and discards it, and it already sits in the module that owns
  `read_pid`, `read_instance_id` and `process_matches_server`. The work is to
  bind that value, call the shared predicate, and return `None` with a message
  when it refuses. A large diff here is a sign the checks were reimplemented,
  which D2 forbids.
- **A daemon started by an earlier build is not relayed to until it is
  restarted.** It recorded no instance id, so it fails check 2. This is the
  same wall PR #174 documents for the embedding path, reached by the same
  users at the same moment, and the remedy is the same one line. Because the
  two paths land in sequence, a user who restarts once for PR #174 satisfies
  both.
- **A stranded `server.port` now costs a delay instead of a credential.** The
  crash-then-squat window described above stops being exploitable, and the
  honest cost is that a genuinely healthy daemon whose state files were
  disturbed is no longer relayed to. Under D5 that trade is not close.
- **Tests.** The relay path needs the equivalents of PR #174's three unit
  tests: a recorded daemon that still matches is still used (the regression
  guard that stops the verification being satisfied by refusing everything), a
  squatter on the recorded port receives neither entries nor bearer and no
  fallback re-finds it, and a responder reporting a different instance id is
  refused. The bearer assertion is the point of the second one: it must assert
  that no request carrying the credential reached the squatter, not merely that
  the port was not returned.
- **Existing relay tests that fabricate a `server.port`.** PR #174 lists
  `outbox.rs:437`, `status.rs:1097`/`:1211` and
  `tests/e2e_tests/adr037_p2_auto_start_scope.rs:313` as writers it left alone
  precisely because they feed this path. Each of them now has to record what a
  real start records, or move to a documented override. The count is small and
  known, and the sweep belongs to the implementation.
- **Documentation.** The relay section of `docs/security/THREAT-MODEL.md` gains
  the responder-identity threat as closed, alongside the two closed rows it
  already carries; its "no route returns the bearer" sentence is accurate about
  the daemon and should say so explicitly rather than being read as covering
  the CLI. ADR-085's *Future work* paragraph gains a pointer here.
- **CHANGELOG.** One entry under `Fixed`, saying that the local relay no longer
  hands memory entries or the team credential to an unverified local listener,
  and that a daemon started by an earlier build must be restarted.
- **Future deferrals on this surface have a decision to cite.** The next
  proposal to park a trust question on the grounds that the signal does not
  exist has to say why the signal PR #174 built does not apply.

## Verification (what would falsify this)

This record rests on two claims.

The first is that the recorded port is reachable by an attacker in the ordinary
case. It would be falsified by an auto-started daemon that does not use the
documented default, or by the port file being removed when a daemon dies
uncleanly. Neither holds today: `find_available_port` takes the preferred port
when free, both callers prefer 4655, and every `cleanup_state_files` call site
is in-process.

The second is that the recorded instance id is a sound signal rather than
merely a better one. It would be falsified by a way for a process that cannot
read the `0700` state directory to learn or predict a daemon's `instance_id`.
Any route that returns it to an unauthenticated caller, or any derivation of it
from public facts, would reopen this.

## Acceptance criteria

1. `probe_local_relay_port` returns `Some(port)` only after both checks in D1
   pass, and returns `None` otherwise with a message naming which check failed
   and the restart remedy.
2. The checks are invoked through the same predicate `probe_loopback` uses.
   A review grep finds one definition of the pid-plus-instance-id test in the
   tree, not two.
3. No path reached from a `probe_local_relay_port` refusal probes another port
   or another responder.
4. `register_and_push` is unreachable with a bearer unless D1 passed, from both
   `nudge_after_write` and `poll_and_apply`.
5. `outbox.rs` passes `DEFAULT_SERVER_PORT`, and no non-test caller of
   `ensure_server_running` anywhere in `crates/inkentry-cli/src/` passes a port
   literal.
6. The relay's wire shapes, routes and session semantics are byte-identical to
   what ADR-037 P2 specifies. A diff touching `crates/inkentry-server/src/relay/`
   means this record was read wrongly.
