# ADR-071: Per-server scoping of the client bearer credential

**Date:** 2026-07-16
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** operates strictly inside
[ADR-056](056-oss-server-tenancy-model.md)'s tenancy model (a spelunk-server
instance is a single trust domain, its shared key is the boundary, and
isolation between groups is achieved by running separate instances) and does
not reopen it. Takes [ADR-066](066-native-tls-in-spelunk-server.md)'s
transport as given: the team server is spelunk-server itself over HTTPS plus
an API key, with no reverse proxy in front. This ADR is about how the *client*
holds and selects among those API keys once ADR-056's topology gives one
developer more than one of them.

## Context

### The topology multiplies keys; the client holds one

ADR-056 settled that a spelunk-server instance is one trust domain and that
two groups needing isolation from each other run **separate server
instances**, each with its own key and its own database. That decision is not
reopened here, and it has a client-side consequence nothing has absorbed yet:
a developer who works on two projects backed by two servers legitimately holds
two keys, one per instance.

The client cannot represent that. The credential resolution in `Config::load`
(`crates/spelunk-core/src/config.rs:573-591`) resolves exactly one flat
bearer, with this precedence (highest first):

1. `SPELUNK_SERVER_KEY` environment variable
2. `[auth].access_token` from `spelunk login` (the WorkOS cloud path)
3. the secret-store `server_key` entry (keychain by default, owner-only file
   fallback when headless)
4. a `server_key` from the committed project-level `.spelunk/config.toml`

Nothing in that chain is keyed to `server_url`. Whichever value wins is
attached to whatever server the resolved `server_url` names, so a two-server
developer either juggles `SPELUNK_SERVER_KEY` per invocation or lets the wrong
key hit the wrong server and gets a 401. The topology ADR-056 recommends is,
on the client, an env-var discipline problem.

### The key has no front door, and two documented back doors

Three further facts about the current state, each verified against the tree:

- **`save_server_key` has no production caller.**
  `crates/spelunk-core/src/config.rs:689` defines the function that persists
  the bearer into the secret store, and nothing outside its own tests calls
  it. Its doc comment says it is "the token `spelunk login` persists", which
  is wrong: `login` persists the `[auth]` token pair and never touches it
  (only `logout` reaches the entry, via `remove_server_key`,
  `crates/spelunk-cli/src/cli/cmd/logout.rs:14`). There is no key-set command.

- **The documented way to set the key is a plaintext file edit.**
  `docs/server.md:159-164` instructs pasting the shared key into the personal
  `~/.config/spelunk/config.toml` as plaintext. A one-time migration in
  `Config::load` (`config.rs:524-541`) then quietly moves it into the secret
  store and strips the file. So the plaintext edit is the de facto set-key
  flow: the migration path is the front door, entered backwards.

- **The committed project file accepts a credential.** `docs/server.md:143`
  tells each developer to add `.spelunk/config.toml` at the project root and
  commit it, and `ProjectConfig` (`config.rs:256-266`) accepts a `server_key`
  field in that file, tier 4 of the precedence above. The struct's own doc
  comment says it contains "no secrets", one field above a field whose comment
  concedes it is a shared API key that is "acceptable if the server is behind
  a VPN/firewall". A credential in a committed file is in the repo's history
  for good, visible to anyone with repo access whether or not they should hold
  the key, and rotatable only by rewriting a file everyone has.

So the flat key is simultaneously too coarse for the recommended topology and
held in places a credential should not be. Both problems have one fix: give
the credential a real home, keyed by the server it belongs to.

## Decision

**Store a per-origin key map in the secret store as a single entry; resolve
the bearer for a given `server_url` through it; give the key a real command
surface (`spelunk auth set-key`, `spelunk auth list-servers`); and remove
`server_key` from the committed project config.**

### D1 – one secret-store entry holding a per-origin key map

A single new secret-store entry, `(service = "spelunk", user =
"server_keys")`, whose opaque string payload is a JSON object mapping origin
to key:

```json
{ "https://spelunk.internal.example.com": "sk-...", "https://other.example.net:8443": "sk-..." }
```

The map key is the **normalized origin** of the resolved `server_url`: scheme,
host, and port (explicit, with the scheme default applied), nothing else. Path,
query, trailing slash, and host case do not participate. Origin is the right
granularity because it is the trust-domain granularity: ADR-056 makes the
instance the boundary, and an instance is addressed by an origin.

**One entry, not one entry per host.** The keyring layer stores each secret as
its own keychain item (`(service = "spelunk", user = <key>)`,
`crates/spelunk-core/src/config/secret_store.rs`), and on macOS each distinct
keychain item prompts for access separately, even after "Always Allow" has
been granted for another item under the same service. Per-host items would
turn adding a second server into a second permission dialog for every binary
that reads keys. One item means one grant covers the whole map, and adding a
server never re-prompts.

**The `SecretStore` trait does not change.** It stays a `get`/`set`/`delete`
over opaque strings; the JSON encoding and origin normalization live entirely
in the config layer above it. The trait's opacity is what keeps the keychain,
file-fallback, and any future backend interchangeable, and a map-shaped
payload is exactly the kind of thing opacity is for.

### D2 – resolution is per resolved `server_url`

The bearer for a request is resolved against the `server_url` the request
will actually go to, with this precedence (highest first):

1. **`SPELUNK_SERVER_KEY` environment variable.** Unchanged, and still a
   per-invocation override: CI, headless setups, and one-off testing against
   a server whose key is not stored keep working exactly as today.
2. **`[auth].access_token`.** Unchanged. The cloud login path is a different
   credential kind with its own refresh lifecycle, and this ADR does not
   touch it.
3. **`server_keys[origin]`**, the map from D1, looked up by the normalized
   origin of the resolved `server_url`.
4. **The legacy flat secret-store `server_key` entry**, as a back-compat
   fallback. When this tier answers *and* a non-empty map exists, a one-line
   deprecation nudge on stderr points at `spelunk auth set-key`; when no map
   exists (the single-server user who has never touched the new surface) it
   answers silently, because for that user nothing is wrong.

This lands as a `Config::bearer_for(server_url)` lookup rather than a field
populated once at load time. That placement is deliberate: concurrent work on
reducing macOS keychain prompts is moving secret-store reads onto a lazy
resolution seam, so that commands which never talk to a server never touch
the keychain, and per-URL resolution has to sit on the same seam or it would
re-introduce an unconditional keychain read at load. The two changes are
separate deliverables but share the seam, and this ADR's resolution order is
the contract for what `bearer_for` returns regardless of when it is called.

Tier 4 of the *old* chain, the committed project-file `server_key`, is
removed rather than re-scoped. D4 records that as its own decision.

### D3 – the key gets a command surface

Three commands, of which two are new:

- **`spelunk auth set-key --server <url>`** stores a key for a server. The
  key is read from stdin or an interactive prompt, **never** from argv: a
  positional or flag-valued secret lands in shell history and in `ps` output,
  which is the same class of leak D4 closes for the committed file. The URL
  is normalized to its origin before storage, so `set-key` and resolution
  cannot disagree about spelling.
- **`spelunk auth list-servers`** prints the origins present in the map, and
  whether a legacy flat key also exists. It never prints key material, not
  even truncated: a listing surface that shows secret prefixes trains users
  to have secrets on screen.
- **`spelunk logout`** (existing) additionally clears the map. It already
  clears the flat entry and any plaintext remnant in the personal config;
  after this change it clears all three, so "remove stored credentials" keeps
  meaning all of them.

This is the first production caller of the `save_server_key`-shaped
persistence path, which until now existed only for its tests, and it retires
the plaintext-file edit as the documented way to install a key. The stale doc
comment on `save_server_key` (claiming `spelunk login` persists it) is
corrected as part of this work.

### D4 – `server_key` is removed from the committed project config

`ProjectConfig` stops accepting `server_key`. A `.spelunk/config.toml` that
still carries one gets a load-time warning naming the file, stating that the
value is ignored, and pointing at `spelunk auth set-key --server <url>` as
the replacement; the load then proceeds as if the field were absent. The
warning is actionable rather than nagging: it names the exact command that
makes it go away.

This is a deliberate breaking change taken inside the pre-v1.0 window, with
direct precedent: the deprecated `memory_server_*` config aliases were removed
the same way (rejected-with-guidance rather than silently honored), and the
regression tests for that removal (`config.rs:1209` onward) are the pattern
this follows.

Warn-and-ignore is chosen over warn-and-honor because honoring it would keep
the committed-file credential alive indefinitely: every repo that has one
would keep working, so no one would move, and the file (and its history)
would keep being the place the key lives. It is chosen over hard failure
because the file is *committed*, so an individual developer hit by a hard
error often cannot fix the shared file unilaterally; warn-and-ignore lets
them fix their own machine with `auth set-key` immediately and the repo at
leisure. The other shareable fields (`server_url`, `project_id`,
`server_ca`) are unaffected; they are what the committed file is for.

### Migration and back-compat

- **The legacy flat entry keeps working** as resolution tier 4 (D2). A
  single-server user who never runs `auth set-key` sees zero behavior change:
  same key, same store, same precedence position relative to env and
  `[auth]`.
- **The existing plaintext-file migration (`config.rs:524-541`) is
  unchanged.** It still moves a bare `server_key` out of the personal
  `config.toml` into the flat store entry. It does not learn about the map:
  a migrated flat key serves tier 4, and the user graduates to the map only
  by running `auth set-key`. Teaching the migration to guess an origin for a
  bare key would require guessing which server the key belongs to, which is
  exactly the ambiguity the map exists to remove.
- **No data migration is needed** because no released surface writes the map
  today; it is born empty and populated only by `auth set-key`.
- **Docs follow, separately.** The client-configuration sections of
  `docs/server.md` and `docs/self-hosting.md` describe the plaintext edit and
  the committed-file key, and both need rewriting to describe `auth set-key`.
  That rewrite is owned by the in-flight docs consolidation work covering
  those files; this ADR records the dependency so the docs change is traceable
  to the decision, and does not do the rewrite.

## Non-goals

- **Not reopening ADR-056.** The server-side model stays: one instance, one
  trust domain, one shared key, isolation by separate instances. This ADR
  changes only how the client holds the keys that model hands out. Per-project
  or per-principal ACLs on a single instance remain out of scope and deferred.
- **Not a reverse proxy or any new server surface.** ADR-066 stands; nothing
  here touches the server at all.
- **Not touching the `[auth]` cloud token path.** `spelunk login`'s WorkOS
  tokens keep their own storage, refresh, and precedence position.
- **Not per-project keys.** The map is keyed by origin, not by project.
  Two projects on one server share that server's key, which is exactly
  ADR-056's model. A key map keyed by project would imply an authorization
  granularity the server does not have.
- **Not credential rotation, expiry, or multiple keys per origin.** The map
  holds one current key per origin. Rotation is `auth set-key` with the new
  value.

## Consequences

- **Two-server workflows work concurrently, with no env juggling.** Each
  invocation resolves the key for the server it is actually talking to. The
  env var remains available as an override, but stops being the only way.
- **The credential has a front door.** `auth set-key` replaces "paste it into
  a config file and let the loader migrate it", and the key never transits
  argv, shell history, or a committed file on the supported path.
- **Repos with a committed `server_key` see a warning and must move.** The
  value is ignored on load (D4). This is the one breaking edge, taken
  deliberately pre-v1.0, and the warning carries the fix. Removing the field
  from the file (and rotating the key it exposed, since git history retains
  it) is the operator's follow-up; the warning cannot rotate a key that has
  already been committed.
- **A second resolution input exists.** The bearer now depends on
  `server_url`, not just on which stores hold values. Debugging "which key
  was sent" gains a step, which `auth list-servers` plus the deterministic
  precedence in D2 is designed to keep cheap.
- **`logout` clears more.** Users with both a map and a legacy entry get both
  removed, which is what "remove stored credentials" should always have
  meant.
- **Docs sections go stale until the consolidation lands.** Between this
  change shipping and the docs rewrite, `server.md`'s plaintext instruction
  describes a path that still functions (the migration keeps working) but is
  no longer the recommended one. The dependency is recorded above.

## Security implications

- **The committed-file credential path is closed.** A shared bearer in a
  committed `.spelunk/config.toml` was readable by anyone with repo access
  and preserved forever in history. After D4 the client ignores it, so the
  file stops being a live credential carrier; existing history exposure is a
  rotation matter for operators, which the D4 warning surfaces.
- **No secret ever transits argv.** `auth set-key` reads from stdin or a
  prompt. `list-servers` prints origins only.
- **Key material stays in the secret store.** The map lives in the same
  keychain (or owner-only file fallback) as the flat entry it generalizes;
  no new storage location is introduced, and `config.toml` remains
  credential-free on the supported path.
- **Blast radius per key shrinks in the multi-server case.** With one flat
  key it was easy for the wrong server to be sent a valid key for a
  different, more privileged instance. Origin-scoped resolution means a key
  is only ever presented to the origin it was stored for; the env-var
  override remains the deliberate escape hatch and keeps its current
  semantics.
- **The trust model is unchanged.** The key still grants everything on its
  instance (ADR-056), and transport is still ADR-066's native TLS. This ADR
  neither strengthens nor weakens what a key can do; it changes where keys
  live and which one is sent.
