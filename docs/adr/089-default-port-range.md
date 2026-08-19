# ADR-089: The default ports move off 7777 to the 465x range

**Date:** 2026-08-19
**Deciders:** founder (Johan); architect (this record)
**Relationship to prior ADRs:** does not change any decision in
[ADR-002](002-server-ai-endpoint-contract.md),
[ADR-058](058-team-server-bare-metal-deployment.md),
[ADR-066](066-native-tls-in-spelunk-server.md) or
[ADR-070](070-init-embed-lifecycle-and-search-warmup-contract.md). Each of
those names 7777 incidentally, in an example or a captured transcript, and each
now carries a pointer here.

## Context

The default listener was 7777, with auto-start walking the ten ports above it
when that one was taken.

**7777 and 7778 are registered on developer machines** by Unreal-engine dedicated
servers, Terraria and ARK. That is a collision with exactly the audience this
product is for: people who write code on the machine they also play on, and the
prior claim on those numbers is theirs rather than ours. It is not a
hypothetical clash with a registry entry nobody honours; it is software that is
actually listening.

The ten-port walk made it worse rather than better. A machine that had asked
for one port could end up with the daemon claiming a block of the range, and
the block it walked into is the same range the neighbouring games use, so the
walk could land the daemon on a *different* registered number and take that one
too.

A second problem surfaced while looking at this: **the loopback daemon and a
team deployment shared one default**, so the two roles were indistinguishable
by port on a machine running both.

## Decision

**Move the defaults into the 465x range, give each role its own number, and put
the number in one place.**

| port | role | mnemonic |
| --- | --- | --- |
| **4655** | local loopback daemon | `inkl` |
| **4658** | team deployments | `inkt` |
| **4652** | cloud-api dev listener, in its own repository | `inkc` |

465 spells `ink` on a phone keypad, which is the reason the range was picked
over any other unsaturated block: the numbers are guessable from the product
name rather than arbitrary.

### D1 - one constant, not four literals

The number now has a single home, `inkentry_core::config::DEFAULT_SERVER_PORT`,
consumed by both the clap defaults and loopback discovery. It previously
existed as four independent literals, which is how a default drifts between the
code that binds it and the code that goes looking for it.

### D2 - auto-start takes an ephemeral port; explicit `server start` does not

An auto-started daemon whose port is taken now binds an **ephemeral** port
rather than walking upward. Walking claimed a block of a range on behalf of a
machine that had asked for one port, and could land on a neighbour's number,
which is the same class of rudeness that made 7777 untenable.

Explicit `inkentry server start` is unchanged: it binds `--port` exactly and
**fails loudly** if it cannot. A user who names a port means it, and silently
serving on a different one would break every client configured to reach the
named one.

### D3 - the roles are separate numbers, not one number with a flag

4655 and 4658 are different defaults because they are different deployments
with different trust properties. A loopback daemon is an inference backend that
never owns memory; a team server owns memory and is reached over TLS by more
than one person. Sharing a default made a misconfiguration between the two
silent, and the distinction is already load-bearing everywhere else in the
product.

`cloud-api`'s 4652 is allocated here for the record, but **nothing in this
repository enforces it**: that listener is configured in its own repository.
Recorded so the three do not collide by accident, not as a constraint this
codebase can check.

## Consequences

- **Anyone running a team server on 7777 keeps working.** This changes a
  default, not an accepted value. An explicit `--port 7777` or a configured
  `server_url` naming it is unaffected.
- **A user with no explicit port, upgrading, moves.** Their daemon binds 4655
  and discovery looks there, so the two stay consistent. A stale `server_url`
  pinned to 7777 in a committed project config does not, and that is the one
  case worth calling out in release notes.
- **The four ADRs naming 7777 get a pointer here rather than a rewrite.** See
  below: two of them are copy-paste hazards, one is already superseded, and one
  is a transcript that must not be edited.
- **`^5` is not addressed by this.** Integration tests co-opting whatever
  answers on the well-known port is loopback auto-discovery, not the port
  number, and renumbering moves that problem without changing it.

## On amending the four ADRs, which are not alike

They are handled differently on purpose, because a blanket sweep would damage
one of them.

**ADR-058 and ADR-066 are copy-paste hazards.** ADR-058's systemd unit
(`--host 127.0.0.1 --port 7777`) and health-check probe, and ADR-066's
`-p 443:7777` Docker publish, are commands an operator lifts directly. These
get a note at the section head.

**ADR-002's reference sits inside a block already marked superseded**, so the
content is disclaimed. It gets a brief pointer for findability and nothing more.

**ADR-070's reference is inside a captured reproduction.** It is a transcript of
observed output, recorded as evidence for the defect that ADR exists to fix.
**Editing recorded evidence to keep it current is worse than leaving a stale
number in it**, because the value of a transcript is that it is what actually
happened. It gets a note next to the block saying the port shown is historical,
and the block itself is left exactly as captured.

That distinction is the reason this is a record rather than a find-and-replace:
the right treatment depends on what the number is doing in each document, and
in one case the right treatment is to leave it alone.
