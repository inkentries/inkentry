# ADR-085: Lifecycle commands signal only a process they recorded

**Date:** 2026-08-19
**Deciders:** architecture review (this record); founder (Johan) sign-off via PR review
**Relationship to prior ADRs:** none superseded or amended.
[ADR-056](056-oss-server-tenancy-model.md) establishes that a loopback bind is
the unauthenticated configuration, which is the trust posture this record
reasons from. It constrains how `inkentry server start`/`stop` and the
auto-start path in [ADR-070](070-init-embed-lifecycle-and-search-warmup-contract.md)
may act on a process; it changes nothing either of them decided.

## Context

`inkentry server` manages one local daemon per state directory. Its record of
that daemon is three files written at spawn time by `cmd_start` /
`ensure_server_running` (`crates/inkentry-cli/src/cli/cmd/server.rs`):
`server.pid`, `server.port` and `server.db-path`. `server.pid` is the only
thing `cmd_stop` has: it reads the pid, checks the process is alive, classifies
it, and signals it.

### The gap that raised the question

A daemon whose `server.pid` was removed, or never written (started by hand,
started by a service manager, state directory wiped underneath it), cannot be
stopped by `inkentry server stop`. The process is plainly alive in `ps` while
the command that is supposed to manage it has nothing to signal. Today
`cmd_stop` answers that case by refusing and explaining:

> no server.pid in ... this CLI has no record of a running inkentry-server. If
> one is running anyway, it was not started from here (or its state files were
> removed): find it with `ps ax | grep inkentry-server` and stop that process
> directly.

The obvious next step, and the one proposed, is to close the gap: when there is
no pid on record, discover whatever is listening on the server's port, confirm
it looks like ours, and signal that instead. This record exists because that
step was refused, and the reason generalises well beyond the one command.

### What a port fallback would have to trust

It would authorise a kill on the strength of two signals, neither of which the
CLI wrote:

1. **Port occupancy.** Local discovery already trusts the port rather than the
   process. `probe_local_relay_port` reads `server.port` and returns that port
   to callers if anything answers `GET /v1/health` on it; there is no check
   that the responder is the daemon this CLI spawned, and on a loopback bind
   there is typically no key to check with (ADR-056). This is not a
   hypothetical weakness. An integration test
   (`crates/inkentry-cli/tests/e2e_tests/adr037_p2_auto_start_scope.rs`) stands
   up an in-process `axum` router on an ephemeral port, writes that port into
   `server.port` with no pid file at all, and the CLI accepts it as the local
   relay. The mechanism the tests exploit deliberately is the same mechanism a
   fallback would rest a kill on.
2. **A command-line substring.** `process_matches_server(pid)` runs
   `ps -p <pid> -o args=` (Unix) or `tasklist` (Windows) and returns true when
   the output contains the literal text `inkentry-server`. That is the whole
   check.

A third signal that sounds like identity is not one. `/v1/health` carries
`started_by`, the effective UID the server reports for itself
(`crates/inkentry-server/src/main.rs`, `handlers/health.rs`). The client side
(`crates/inkentry-cli/src/capability/probe.rs`) compares it to the caller's UID
and emits `tracing::warn!` when they differ. It is self-reported by whatever
answered, and it gates nothing.

### The identity check is known to be wrong in both directions

`process_matches_server` has a demonstrated false negative on a process that
genuinely is ours. The workspace binaries were renamed from the predecessor
product's `spelunk-*` to `inkentry-*`. A daemon still running from a
pre-rename install presents as `spelunk-server` in `ps`, fails the substring
test, is classified `RunningServer::Foreign`, and `stop` refuses it as an
unrelated process.

The converse follows from the same line of code, and reproduces in one command:
any process whose argv happens to contain the string passes.

```
$ python3 -c "import time; time.sleep(8)" inkentry-server &
$ ps -p $! -o args=
.../Python -c import time; time.sleep(8) inkentry-server
```

Nothing about that process is a server. A substring test over a
caller-controlled argv is not an identity, and a check that already
false-negatives on a real daemon cannot be relied on to false-positive rarely.

### Why this is a decision, not a bug report

Fixing the substring check does not dissolve the question. Any cheaper or
richer variant (match the resolved executable path, compare a start time,
compare an inode) narrows the error rate without changing what is being asked
of it: to authorise an irreversible, out-of-process action against a target the
tool did not record. The pid file is the only artefact in this system that
represents "we started this". Everything else is a guess with a good hit rate.

The cost of being wrong is asymmetric. A refusal costs the user one manual
command, printed for them. A wrong kill takes down an unrelated process on a
developer or build machine, with no undo and no audit trail.

## Decision

**A lifecycle command may not send a signal to, terminate, or otherwise act on
a process whose identity rests only on network behaviour (something answered on
a port) or on a match against its command line. It may observe such a process,
report it precisely, and instruct the user, and it must stop there.**

Concretely, for `inkentry server` and any future command that manages a
process:

**May:**

- Probe a port and use the answer to decide what the *CLI itself* does next
  (reuse a healthy relay, skip an auto-start, report status).
- Inspect a process it did not record: whether a pid is alive, what its command
  line looks like, which port is occupied.
- Report what it found, naming the pid, the port and the state directory.
- Hand the user the exact command to run, so the action is taken by a human who
  can see the target.
- Signal a process whose pid it read from its own state file, using a command
  line or health check as a *further* guard that narrows an already-recorded
  pid. Corroboration may only subtract from the set of processes the command
  will touch; it may never be the thing that puts one in.

**May not:**

- Signal, terminate, or kill a pid it discovered by asking who holds a port.
- Signal, terminate, or kill a pid because its argv matched a product string.
- Treat a `/v1/health` response, an `instance_id`, or a self-reported
  `started_by` as proof of identity for the purpose of authorising a kill.
  Those fields describe what a responder claims about itself.

Today's code already sits inside this rule, and this ADR is a record of why it
stays there rather than a request to change it. `cmd_stop` and
`ensure_server_running` take their pid from `server.pid` and use
`classify_running_server` only to decide whether that recorded pid is still
ours (`Healthy`), ours but wedged (`HungOurs`), or a reused pid to leave alone
(`Foreign`). The substring check narrows; it never sources. The missing-pid
branch already diagnoses and instructs.

## Non-goals

- **Not a change to `process_matches_server`.** Its weakness is evidence here,
  not work raised by this record. Within the rule above it is used only to
  refuse, which is the direction its errors are tolerable in.
- **Not a fix for the pid-file lifecycle.** Making `server.pid` harder to lose,
  or writing it from the daemon rather than the spawning CLI, would shrink the
  gap and is compatible with this decision. It is separate work.
- **Not a ban on port probing.** Discovery that informs the CLI's own next
  action is unaffected. The constraint is on acting *against another process*.
- **Not a statement about a future supervised or service-managed daemon.** A
  process supervisor that holds its own record of what it started is a
  different identity story, and would get its own record.

## Alternatives considered

1. **Port-discovery fallback with an identity check (rejected).** When
   `server.pid` is absent, find the holder of the server's port, confirm it
   with `process_matches_server`, and signal it. Rejected because the
   confirmation step cannot carry the weight placed on it: the check is a
   substring test over argv, it is already known to reject a real daemon
   started from a pre-rename binary, and the discovery half trusts whatever
   answers on loopback, which is unauthenticated by design and which the test
   suite already substitutes freely.
2. **Port-discovery fallback behind a confirmation prompt (rejected).** Moves
   the decision to the user, but shows them only what the tool believes, and
   the tool's belief is the part that is unreliable. A prompt that says "kill
   pid 4711?" on the strength of a bad match makes a wrong kill likelier, not
   less likely, because it borrows the tool's authority for it. Naming the pid
   and the command to run gives the user the same information with the action
   left where the knowledge is.
3. **A stronger identity check, then signal (deferred, not adopted).** Compare
   the resolved executable path, or record and compare a start time or an
   executable inode. This is a genuine improvement to identity and may be worth
   doing on its own merits, but it does not change the rule: it would still be
   the CLI deciding, from the outside, that an unrecorded process is safe to
   kill. If a future record wants to authorise that, it should argue for the
   authority first and the mechanism second.
4. **Do nothing and say nothing (rejected).** Leaving this as a code comment
   invites the same proposal on the next lifecycle command, and the reasoning
   has to be reconstructed each time. The refusal is a constraint on a class of
   commands, so it belongs in the record.

## Consequences

- **Recovery from a lost pid file stays manual, and that is accepted.** A user
  whose `server.pid` is gone must stop the daemon themselves. The CLI's job is
  to make that one command obvious: which pid, on which port, from which state
  directory, and exactly what to run. This is a real cost, taken deliberately,
  not an oversight to be closed later by a fallback.
- **A daemon started outside the CLI is outside the CLI's lifecycle.** That is
  the honest description of the current design and should be documented that
  way rather than papered over.
- **`stop` remains capable of reclaiming a wedged daemon** whose pid it holds,
  including one that has stopped answering `/v1/health`. Nothing in this record
  narrows the recorded-pid path.
- **The `spelunk-server` false negative persists** for anyone still running a
  pre-rename daemon, and the fix for those users is the manual path above.
  Widening the substring to match the predecessor name would trade a known
  false negative for a wider false-positive surface, which is the wrong
  direction under this rule; if it is ever done, it must remain confined to the
  recorded-pid path.
- **Future lifecycle work has a decision to cite** instead of relitigating the
  fallback each time it looks convenient.

## Future work

Whether loopback discovery should be trusted at all for anything beyond "reuse
this relay" is open and tracked separately: `probe_local_relay_port` accepts a
`/v1/health` answer on a recorded port with no proof that the responder is the
daemon this CLI spawned, and the test suite substitutes its own listener there
routinely. This record deliberately does not settle that. It settles only what
may be done with the answer, which is: not a kill.

## Verification (what would falsify this)

The decision rests on the claim that the available identity signals are too
weak to authorise a kill. It would be falsified by an identity signal that is
sound rather than merely better: one that ties a discovered process to a record
this tool wrote, and that cannot be forged by an unrelated process choosing its
own argv or binding a port. If such a signal exists on all three supported
platforms, the fallback deserves a fresh look, as a new record superseding this
one.

Conversely, it is supported by the two reproductions in *Context*: a
pre-rename daemon that a substring check rejects, and an unrelated process that
the same check accepts.

## Acceptance criteria

This record documents an existing posture; no behaviour changes with it. It is
satisfied when:

1. No lifecycle command in `crates/inkentry-cli/src/cli/cmd/server.rs` sources a
   pid for signalling from anything other than a state file it wrote. A review
   grep for signalling call sites (`terminate_process`, `force_kill`,
   `terminate_and_wait`) shows every one of them reached from a `read_pid`
   result.
2. `process_matches_server` and `classify_running_server` continue to be used
   only to *withhold* a signal from a recorded pid, never to justify one.
3. Any future change proposing to signal a discovered process cites this record
   and supersedes it explicitly, rather than adding a fallback beside it.
