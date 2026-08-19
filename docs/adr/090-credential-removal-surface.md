# ADR-090: One credential removal surface, under `auth`

**Date:** 2026-08-19
**Deciders:** founder (Johan); architect (this record)
**Relationship to prior ADRs:** overturns the placement half of
[ADR-071](071-per-server-client-bearer-scoping.md) D3. The `logout --servers`
and `logout --server <url>` flags D3 created are removed and the capability
moves to `inkentry auth remove-key`. D3's *substance* is untouched and is
restated below: a cloud logout still never clears a self-hosted server key as a
side effect. Sequenced strictly after
[ADR-088](088-retire-legacy-server-key-tiers.md), which rewrites the same three
functions this record does. Does not reopen ADR-071 D1's choice of origin as the
map key, or ADR-056's tenancy model.

## Context

Issue #120 reported that inkentry can store a server key but has no way to
remove one. The premise is false. `inkentry logout --server <url>` removes a
single origin's key and `inkentry logout --servers` removes all of them; both
have existed since ADR-071 D3 shipped.

What the reporter actually did is worth recording, because it is the reason this
record exists in the shape it does. They ran `inkentry auth --help`, saw
`set-key` and `list-servers` and nothing else, and concluded the capability was
missing. Nothing in that help text mentions `logout`, and nothing about the word
"logout" suggests it is where a self-hosted server key is removed. The defect is
real and it is a discoverability failure: the removal lives under a verb nobody
looking for it would try, in a command whose own doc comment
(`crates/inkentry-cli/src/cli/cmd/logout.rs:1`) describes it as clearing cloud
credentials.

That is a docs-and-naming problem on its own. Investigating it turned up four
code defects underneath, and those are what make this a decision rather than a
help-text edit.

### The four defects

**1. The LLM key can be set and never unset.**
`crates/inkentry-core/src/config/llm_key.rs` has `resolve_with_store` (:43) and
`set_with_store` (:51). It has no clearer, and no caller anywhere in the tree
deletes `KEY_LLM_KEY`. `inkentry auth set-key --llm` writes a credential into
the user's OS keychain that the product offers no way to take back out. Issue
#120's headline claim is false about server keys and exactly true here, one
module over, and the report did not notice it.

**2. Removing the last server key leaves an empty entry behind.**
`write_map` (`server_keys.rs:69-72`) serialises the map and calls `store.set`
unconditionally, so an emptied map is written as the string `{}` rather than
deleted. `clear_origin` calls `write_map` after removing the only entry, which
leaves a live keyring entry named `server_keys` holding an empty object. A user
who removes their last key and then audits their keychain still finds an
inkentry credential sitting in it. `clear_all` deletes the entry properly; only
the per-origin path is wrong.

**3. Removal reports success whether or not anything was removed.**
`clear_origin` returns the normalised origin and nothing else. `logout.rs:44`
prints `Cleared the stored server key for {origin}.` unconditionally. A typo in
the URL prints the same sentence as a real removal.

**4. A mistyped origin destroys an unrelated credential.**
`clear_origin` (`server_keys.rs:173-184`): when the origin is not in the map,
the `else` branch calls `store.delete(KEY_SERVER_KEY)`. The intent was
defensible when written, because pre-migration the legacy flat entry was the
fallback for every unmapped origin, so it might have been serving the origin
being cleared. The effect is that any typed URL that misses the map deletes the
legacy key, and combined with defect 3 it reports that as a successful removal
of the origin the user typed. This is the sharpest edge of the four, and it is
also the one ADR-088 makes disappear on its own: once the legacy tier is gone
there is no second credential for a miss to fall onto.

The report's remaining technical claim is correct and worth keeping.
`secret_store.rs` groups every inkentry secret under one keyring service
(`KEYRING_SERVICE`, :51) with one entry per key name (`KeyringStore::entry`,
:115), so the entire per-origin map is a single entry, `(inkentry,
server_keys)`, surfacing as `server_keys.inkentry` in the Windows credential
manager. Removal is a rewrite of that one blob, not a per-host delete, which is
what makes an emptied map's leftover entry (defect 2) visible to the user at
all.

## Decision

**The command that stores a credential is the command that removes it.**

Every credential inkentry holds is installed through `inkentry auth set-key`.
Removal belongs in the same place, spelled the same way, discoverable by the
same `--help`. A user who has just run `set-key --server <url>` and wants to
undo it should find the answer in the help text they already have open, not
under a verb that names a different product surface.

This is the whole ruling. The six sub-decisions below are what following it
consistently requires.

### D1 - `auth remove-key`, mirroring `set-key` argument for argument

```
inkentry auth remove-key (--server <url> | --llm | --all-servers)
```

The three flags form a required, mutually exclusive clap `ArgGroup`, the same
shape `AuthSetKeyArgs` already declares at `auth.rs:34-39`. Mirroring is the
point: `set-key --server <url>` and `remove-key --server <url>` differ by one
word, and `set-key --llm` and `remove-key --llm` likewise, so the removal form
is derivable from the installation form without reading anything.

The bulk flag is spelled `--all-servers`, not `--all`. The defect this record
answers is a user destroying a credential they did not mean to touch, and
`remove-key --all` in a command that can also address the LLM key is ambiguous
about whether "all" includes it. Ambiguity in a destructive bulk flag is a
defect at exactly the moment it costs the most. `--all-servers` says what it
clears and, by omission, what it does not.

There is no confirmation prompt and no `--yes`. The credentials are the user's
own, on their own machine, and every one of them is restorable with a single
`set-key`. A prompt on a command this cheap to undo trains people to type
through prompts on commands that are not.

### D2 - `--llm` removal is a new capability, not a relocation

`--server` and `--all-servers` move an existing capability. `--llm` creates one
that does not exist, backed by a new `llm_key::clear_with_store`, and it closes
defect 1.

It is included here rather than deferred because it is the same defect as the
one issue #120 reported, one module over, and because it is the unqualified
instance of the claim the report made. Fixing only the half a user happened to
notice would leave the product with exactly one credential you can set and never
unset, in a record whose entire subject is that removal should be uniformly
available where installation is. The governing principle does not survive an
exception, and a credential the user cannot revoke is not a cosmetic gap.

### D3 - origin matching is shared code, not agreed convention

`remove-key --server <url>` passes its argument through to `clear_origin`
unchanged. `clear_origin` normalises it with `normalize_origin`
(`server_keys.rs:41`), the same function `set_key_for_origin` uses to decide the
key it stores under. The CLI layer performs no normalisation of its own: no
trailing-slash trimming, no lowercasing, no scheme defaulting, nothing.

Two code paths that agree on a convention drift; two code paths that call one
function cannot. The failure mode this forecloses is specific and quiet. If a
remove normalises even slightly differently from the set, it misses the map
entry, and then (with D4) reports the honest "nothing stored" while the
credential the user believes they just revoked is still live and still
authenticating. A removal that silently fails to match what the set stored is
worse than having no removal at all, because the user stops looking.

### D4 - absence is idempotent and never reported as a removal

Removing a credential that is not there exits 0. It is not an error, and the
wording does not claim a removal happened: removing an unmapped origin says that
no key was stored for it, and the same for an absent LLM key.

The two halves answer different needs and both are required. Idempotence is what
makes the command usable in a rotation script and across a fleet of machines
where some have the key and some do not; making absence an error would force
every caller to special-case an outcome that is already the desired state. The
honest wording is what stops a typo reading as a completed rotation. Under
today's behaviour (defect 3) a user who mistypes an origin is told the removal
succeeded, believes the credential is revoked, and stops. Reporting exactly what
happened is the difference between a no-op and a false sense of revocation.

`clear_origin` and the new LLM clearer therefore report whether they found
anything, rather than returning only the origin they normalised.

### D5 - an emptied map removes its keyring entry

Writing an empty map deletes `KEY_SERVER_KEYS_MAP` instead of storing `{}`. This
is fixed in `write_map`, which makes it a property of the map's writer and so
true for every caller, present and future, rather than a check bolted onto
`clear_origin` that the next writer has to remember. It closes defect 2 and
makes `clear_origin` and `clear_all` converge on the same end state: no stored
keys means no entry.

The reason is user-visible, not tidiness. Someone who removes their last server
key and then opens their OS keychain to confirm must not find an inkentry
credential still sitting there. A leftover entry holding `{}` reads, in a
keychain UI that shows names and not values, as a credential that was not
removed.

### D6 - `logout --servers` and `logout --server <url>` are removed

Both flags are deleted outright. No alias, no hidden flag, no deprecation
period, no shim that forwards to the new spelling. This follows the standing
clean-break rule for the pre-1.0 window and the precedent ADR-088 set for the
credential surfaces specifically: a removed surface is gone, and the error the
user gets names the replacement.

Bare `inkentry logout` is unchanged in behaviour and keeps its residual-key
notice (`logout.rs:50-57`), rewritten to name `inkentry auth remove-key`. That
notice is the discoverability bridge this whole record turns on and it must
survive the change. A user who reaches for `logout` looking for key removal, as
issue #120's reporter would have, is told there that server keys exist, how many
there are, and which command removes them.

**ADR-071 D3's reasoning is preserved intact; only its placement changes.**
D3 was decided on founder review, and the argument that decided it stands
unaltered: a developer recovering from a broken cloud login must not silently
lose the server keys they use on other projects, so clearing a server key is an
explicit, separately requested action and never a side effect of a cloud logout.
After this record, that is still exactly true. Bare `logout` clears the `[auth]`
pair and nothing else. What changes is that the explicit action is now spelled
`auth remove-key` instead of `logout --server`, which is a better home for it on
D3's own reasoning: if clearing a server key is a distinct action from logging
out of the cloud, it should not be a flag on `logout`. D6 is not a reversal of
D3's substance. It is D3 carried to its conclusion.

## Consequences

- **Documentation.** `docs/commands.md` needs both tables rewritten: the `auth`
  table at :787-795 gains `remove-key` with its three flags, and the `logout`
  table at :860-866 loses its `--servers` and `--server` rows and their example
  block. `docs/config-reference.md:249-252` lists `auth set-key` and
  `list-servers` as the preferred alternatives to hand-editing and should list
  the removal alongside them. `docs/server-setup.md:408-439` covers key rotation
  after a compromise and is the page where an explicit revoke step most changes
  the advice. `docs/getting-started.md:609-613` introduces `auth set-key` in the
  team-server walkthrough. The `auth --help` text is itself a deliverable here,
  since a help listing that reads completely is what the underlying report was
  actually asking for.
- **CHANGELOG.** A `### Changed`/`### Removed` pair: `auth remove-key` added
  (naming `--llm` as new capability, not a move), and the two `logout` flags
  removed with the replacement spelling named. A breaking change taken inside
  the pre-1.0 window.
- **ADR-071** needs a line on D3 recording that its flags moved to `auth
  remove-key` and that its reasoning is unchanged, so a later reader does not
  find D3 describing flags that no longer parse.
- **Sequencing: this is implemented strictly after ADR-088 lands.** The two
  records rewrite the same code. ADR-088 removes the legacy flat tier, which
  touches `clear_origin`, `list_origins` and `logout.rs`, the three places this
  record also changes. Taken in the other order the work is done twice and
  conflicts. Order also matters for correctness, not just merge cost: defect 4
  is only *clean* once the legacy tier is gone. Implemented before ADR-088, the
  `else` branch still has a legacy entry to destroy and would need a guard whose
  only purpose is to survive until ADR-088 deletes it.
- **Tests.** New coverage for the idempotent-absence path (D4), for the emptied
  map deleting its entry (D5), and for `--llm` round-tripping set and remove.
  Tests pinning the `logout --server`/`--servers` flags are retired with them.
