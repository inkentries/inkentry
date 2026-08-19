# ADR-085: Retire the plaintext `server_key` lift and the legacy flat key tier

**Date:** 2026-08-19
**Deciders:** founder (Johan), ruling of 2026-08-17; architect (this record)
**Relationship to prior ADRs:** completes
[ADR-071](071-per-server-client-bearer-scoping.md). D1 introduced the
per-origin key map and D4 removed `server_key` from the committed project
config; this record removes the two compatibility paths ADR-071 left standing
so the map could be adopted without breaking anyone. It does not reopen
[ADR-056](056-oss-server-tenancy-model.md)'s tenancy model or ADR-071's choice
of origin as the map key. ADR-074 references the tier structure and needs a
line acknowledging the collapse.

## Context

ADR-071 replaced a flat, unkeyed bearer with a per-origin map. To avoid
stranding anyone mid-upgrade it kept two paths alive alongside the map, both of
which were always intended to be temporary and neither of which has been
retired.

### The two survivals

**(a) The plaintext lift.** A bare `server_key` in the personal
`~/.config/inkentry/config.toml` is read at load time, written into the secret
store, and stripped from the file
(`crates/inkentry-core/src/config/mod.rs:418-487`). This is documented as a
feature in `docs/config-reference.md:219-223` and referenced from five further
pages.

**(b) The flat `KEY_SERVER_KEY` tier.** Below the per-origin map,
`server_key_for_origin` (`crates/inkentry-core/src/config/server_keys.rs:79-95`)
falls back to a single unkeyed secret-store entry, migrates it into the map
under the origin being resolved, deletes it, and prints a notice. The tier
spans `server_keys.rs` (36 mentions), `secret_store.rs` (18), `config/mod.rs`
(12) and `persist.rs` (4), the last of which exists substantially to write and
clear that entry.

### Why the original framing expired

A debt audit scoped this work by one test: does the rename's forced break make
it cheap to remove? It qualified because `migrate.sh` was expected to rewrite
the personal config path and the keychain service name, letting the lift happen
once inside the script.

That window closed. The rename shipped, `migrate.sh` shipped, and it carries no
credential handling at all. So the removal is no longer cheap because of a
break that already happened. It is an ordinary change to a live surface, judged
on its own merits, which is why it was taken to a founder ruling rather than
done as cleanup.

### The two halves are not independent, which the earlier framing had wrong

The task that carried this work stated that (b) "can be done without touching
(a)". That is not true, and the direction of the coupling matters.

**The lift's destination is the legacy tier.** `save_server_key_with`
(`config/mod.rs:477`) writes to `KEY_SERVER_KEY`, the exact entry
`server_key_for_origin` reads as its fallback. Removing (b) alone would leave
the lift writing a credential into an entry nothing reads, so a user whose key
arrived by the documented plaintext route would silently stop authenticating.
That is a worse outcome than either the status quo or full removal, and it is
reachable by doing what the earlier framing said was safe.

Removing (a) alone is coherent but leaves the tier, and therefore
`persist.rs`, alive for pre-ADR-071 users indefinitely.

## Decision

**Remove both before 1.0.0.** The plaintext `server_key` lift goes, the flat
`KEY_SERVER_KEY` tier goes, and credential resolution collapses from three
tiers to two: the `INKENTRY_SERVER_KEY` environment variable, then the
per-origin map (with the cloud origin resolving through `[auth]` as before,
unchanged).

The founder ruling, 2026-08-17:

> (a) Let's deprecate this and remove before v1. This is a potential security
> hole that we don't need to carry across. The code that migrated it has been
> in place for a long time, and we can/should assume that any user that
> installed an early pre-release version has been upgrading along the way.
>
> (b) ADR-071 was implemented in v0.9.4 a month ago, so using same logic let's
> assume that any user has been migrated away by now.

Both rest on the same argument, and it is the right one: a migration path
earns its keep only while people are still travelling it. The lift has been in
place across many releases and the map since v0.9.4, so the population still
depending on either is, by construction, people who have not run inkentry in a
long time. They are better served by an error that names the fix than by
machinery every other user carries forever.

The security case for (a) stands on its own regardless of population. The lift
requires a credential to exist in plaintext in a file, briefly, as its normal
operating mode. `inkentry auth set-key` reads from stdin or a prompt and never
puts the key in a file at all, so the lift is a documented path whose only
purpose is to rescue a practice the product otherwise tells people not to
follow.

### D1 - remove the lift, and say so rather than going quiet

`Config::server_key` and the load-time lift are deleted. A personal config that
still carries the line **is named on stderr**, with the same shape of message
the project config now uses for a `server_key` it does not read: the field is
not read, the key should be rotated, and `inkentry auth set-key` is the
replacement.

This is deliberate and it is the opposite of what ADR-071 D4 originally chose
for the project file. That silence was overturned on review, and the reasoning
that overturned it applies here with more force, not less: only the tool knows
whether a key actually reached the file, and here the user's key is about to
stop working. Going quiet would turn a removal into an unexplained
authentication failure.

The stderr line is not a parse. Nothing reads the value, so this does not
recreate the "warn but still parse" branch ADR-071 D4 worried about.

### D2 - remove the flat tier, collapsing resolution to two tiers

`server_key_for_origin` loses its fallback, migration write, delete and notice,
becoming a map lookup. `KEY_SERVER_KEY` and every function that exists to write
or clear it are deleted, which is most of `persist.rs`'s reason to exist.
`inkentry auth list-servers` stops reporting whether a legacy flat key is also
present, because one cannot be.

The payoff is the one the original audit named and it survives the change of
framing intact: it collapses a three-tier credential resolution to two, and the
three-tier structure is what made ADR-071 hard to reason about in the first
place.

### D3 - a stored legacy key is not silently migrated on the way out

A user who still has a `KEY_SERVER_KEY` entry when they upgrade gets no bearer
for that origin, and the resulting failure names `inkentry auth set-key`. The
alternative, one final migrate-on-read before removal, was considered and
rejected: it keeps the entire tier alive to serve a single upgrade, which is
what this ADR exists to stop, and it cannot be removed later without repeating
this decision.

Both stores are the user's own, on their own machine. Nothing is destroyed by
this: `inkentry auth set-key --server <url>` restores service in one command,
and that command is what the failure text names.

## Consequences

- **Documentation.** `config-reference.md:219-223` loses the lift as a
  documented feature and gains the not-read line. `commands.md`,
  `getting-started.md`, `server-setup.md`, `stability.md` and
  `security/THREAT-MODEL.md` all reference the surface and need sweeping.
  `stability.md` is the one that matters most, since it carries the
  client-facing contract for which keys each file is read for.
- **CHANGELOG.** A `### Removed` entry, naming both paths and the one-command
  recovery. This is a breaking change taken inside the pre-1.0 window, which is
  the whole reason it is being taken now.
- **ADR-071 and ADR-074** each need a line recording that the tier is gone.
  ADR-071's D1 and D2 describe a resolution order that will no longer have a
  third tier.
- **Tests.** Those pinning the *ignoring* of removed keys are kept and extended
  to the new stderr line. Those pinning the migration itself are retired with
  it.
- **No dead code.** Removing a path means removing its now-unused functions and
  imports, `persist.rs` in particular.

## Explicitly not in scope

**`migrate.sh` is left as it is.** It carries no credential of any kind, so a
developer migrating from the predecessor with a stored bearer arrives
unauthenticated and runs `inkentry auth set-key` again. The founder ruled this
window missed rather than worth reopening:

> The migration of the config is a real miss, but it's been around 5 days since
> we published the migration is happening, I think we've missed the window so
> let's just leave the script as is.

Recorded here so it is not re-raised as an oversight. It is recoverable in one
command and it is a known, accepted gap.

Whether `docs/upgrading.md` tells a team-server user to expect that
re-authentication is a separate documentation question and does not belong in
this change.
