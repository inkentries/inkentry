# Upgrade corpus (the "DB museum")

Artifacts written by real, released inkentry binaries, kept so every future build
can be tested against what users actually have on disk.

Every other migration test in this repo builds an old shape by hand. That tests
what we *believe* the old format was. This one tests what it *is*. The corpus is
cheap to capture while the releases are recent and impossible to reconstruct
faithfully later, which is the whole reason it exists.

## Layout

```
scripts/upgrade-corpus/
  generate.sh         rebuild the corpus from pinned releases
  capture_expect.py   read each artifact with plain SQL, write MANIFEST.json
  embed_stub.py       stand-in for the pre-1.0 embedding wire
  checksums.txt       pinned sha256 per release asset

crates/inkentry-cli/tests/
  fixtures/upgrade-corpus/
    MANIFEST.json     one entry per wing: producer, artifact, digest, expected content
    wings/<id>/       the artifact itself, gzipped
  upgrade_corpus.rs   opens every wing with the current build and asserts
```

## The wings

| wing | producer | stamp | what it pins |
| --- | --- | --- | --- |
| `index-v0.8.3-float768` | 0.8.3 | 0 | the last release that wrote `FLOAT[768]` vectors, with vectors actually in the table |
| `index-v0.9.2-pre-user-version` | 0.9.2 | 0 | the last release before `index.db` grew `PRAGMA user_version`, so its version has to be inferred from table shapes |
| `index-v0.9.4-pre-file-mtime` | 0.9.4 | 14 | the last release writing `index.db` at 14, before `files` grew `mtime`; a trusted header the runner has to resume from mid-ladder |
| `index-v0.9.8` | 0.9.8 | 15 | the newest release: the `index.db` shape sitting in almost every project directory in use today |
| `memory-v0.9.3-pre-entity-id` | 0.9.3 | 0 | the last release before memory entries grew a content-addressed `entity_id` |
| `memory-v0.9.5` | 0.9.5 | 0 | entity-id era, with a supersede chain and a separately archived entry; the last release before `memory.db` was stamped at all |
| `memory-v0.9.6-pre-import-state` | 0.9.6 | 9 | the one and only release writing `memory.db` at 9, the last before entries grew import state |
| `memory-v0.9.8` | 0.9.8 | 10 | the highest stamp any release ever wrote, so the store most users are holding when they meet the refusal |
| `registry-v0.9.5` | 0.9.5 | — | two registered projects and a dependency link |
| `git-notes-eras` | 0.7.1 / 0.9.3 / 0.9.5 | — | all three note-writing eras on one notes ref |

"Stamp" is the `PRAGMA user_version` the capturing release left in the file,
recorded in `MANIFEST.json` as `schema_version` and asserted before the current
build touches the artifact. It is not decoration, and what it decides differs by
store. An `index.db` still migrates: unstamped, its version is inferred from
table shapes; stamped, its header is trusted and only the steps above it run. A
`memory.db` no longer migrates at all — this build stamps 11 and refuses
everything at or below 10 — so the stamp decides which of the two refusals the
user is handed, and only the stamped branch can name the version it found. No
row count tells either pair apart, so both eras have to stay represented for
both stores, which `a_schema_version_that_advances_past_the_corpus_fails_here`
enforces.

That test also pins `memory-v0.9.8` specifically. The old product has shipped
its last release, so 10 is the highest stamp any artifact will ever carry;
without a named requirement, dropping that wing would leave every other check
here satisfied by the 0.9.6 wing and cost the coverage silently.

v0.9.7 gets no wing. It stamps 15/10, exactly what v0.9.8 already provides for
both stores. This is a list of change boundaries, not a list of releases.

The note eras were established by running the binaries, not by reading history:
releases up to and including **0.9.2 replace** a commit's note blob on every
add; the append-only JSON-lines log starts at **0.9.3** (still without
`entity_id`); entity-keying arrives at **0.9.5**. Because the older eras
replace, each era writes against its own commit, which is also what a
long-lived checkout looks like.

## What is real, and what is not

Real, and the entire point: the binaries are the published release assets,
pinned by the sha256 GitHub records for them. Every database file, its schema,
its `vec0` table declarations and every row in it were written by that binary.

Not real, and deliberately so: the **values** inside the embedding vectors.
Pre-1.0 releases embed by calling a `inkentry-server` that shipped an embedder
which no longer exists, and a current server answers on a wire shape those
binaries cannot parse, so neither can produce these wings. `embed_stub.py`
serves that era's `/v1/health` and embedding endpoints so the real old binary
can complete a real run.

`generate.sh` starts the stub for **every wing but one**, not only the
768-dimension one. Where the stub runs and where synthetic values actually end
up on disk are two different lists, so both are spelled out:

| wing | stub runs during capture | synthetic vector values in the artifact |
| --- | --- | --- |
| every `index-*` wing | yes, for `inkentry index` | yes, the chunk embeddings |
| every `memory-*` wing | yes, for `inkentry memory add` | yes, the note embeddings |
| `registry-v0.9.5` | yes: the capture indexes two repos in order to register and link them | none, `registry.db` stores no vectors |
| `git-notes-eras` | no, notes are written without embedding | none |

Note that the memory wings reach the stub through `memory add`, not through
`inkentry index`, so "every wing that runs `inkentry index`" is not the right
rule for which wings are affected. The list above is.

Vector values are irrelevant to a migration test: what is asserted is that the
right number of vectors survives, and that the dimension-upgrade path discards
768-dimension ones wholesale.

Nothing the stub says about itself reaches disk: no wing contains its instance
id, its address or its port, and no wing has an `index_meta` provenance row,
because that table post-dates all of them.

## Determinism

Vector values are derived from a hash of the chunk text, so they do not churn.
The artifacts as a whole are **not** byte-reproducible: the databases carry
wall-clock `indexed_at` / `created_at` / `registered_at` values, note ids are
epoch milliseconds, and the registry wing stores the absolute path of the
`mktemp` directory it was captured in. Regenerating a wing therefore produces a
different file even when nothing about the release changed. Do not regenerate a
wing you did not mean to change: `--only` exists for exactly this reason.

## Captured paths are foreign paths

A path stored inside a wing belongs to the machine that captured it, so it is a
**portability** constraint as well as a reproducibility one. The suite runs on
Windows as well as macOS and Linux, and a macOS path is not a valid path there:
`Path::is_absolute` is false for `/private/var/...` on Windows, which needs a
drive or UNC prefix, and `canonicalize`, `exists` and separator handling are
host-OS questions in the same way.

So assert a captured path by comparing it with what the artifact holds, read
out of the wing before the current build opens it, never by asking the host
whether it looks like a path. Equality is the same question on every runner and
is the stronger check anyway: a path rewritten to a different absolute path is
still mangled. Whole-component operations that only read the string, such as
`Path::starts_with`, are safe. This is what
`every_registry_wing_keeps_its_projects_and_dependency_links` does with the
`registry-v0.9.5` wing.

`checksums.txt` pins the release binaries, which says nothing about the
artifacts. The artifacts are pinned separately by the `sha256` recorded per
wing in `MANIFEST.json`, which the suite checks before asserting anything else.
A wing that is edited or regenerated without its expectations being recaptured
fails the suite rather than quietly asserting one artifact's contents against
another's.

## Regenerating

Needs `gh` (authenticated), `python3`, `sqlite3`, and `git`. No inkentry-server
and no model download.

```sh
scripts/upgrade-corpus/generate.sh              # every wing
scripts/upgrade-corpus/generate.sh --list
scripts/upgrade-corpus/generate.sh --only index-v0.9.2-pre-user-version
```

Wings are only rewritten when `--only` names them, so touching one does not
churn the others.

To fill in a newly recorded expectation for wings that already exist, re-read
them instead of rebuilding them:

```sh
python3 scripts/upgrade-corpus/capture_expect.py \
  crates/inkentry-cli/tests/fixtures/upgrade-corpus/wings \
  crates/inkentry-cli/tests/fixtures/upgrade-corpus/MANIFEST.json --refresh
```

A refresh reads the same bytes with the same plain SQL, so every value it
writes — including each `sha256` — comes back identical apart from the field
being added. Rebuilding a wing to add a field would instead churn its bytes for
nothing, because capture is not byte-reproducible.

### Which name a release answers to

The project renamed at **v0.9.8**. Releases up to v0.9.7 were published from
the predecessor repository under the predecessor name and read `SPELUNK_*`,
write `.spelunk/`, and ship a binary of that name; v0.9.8 is the first release
under the current one. `release_repo` and `release_name` in `generate.sh` are
the single place that boundary is encoded, and every site that touches a
released binary asks them. Sweeping either vocabulary into the other points the
download at an asset that does not exist, or leaves an old binary reading the
real `HOME` the capture exists to isolate it from.

## Keeping the list from stopping

The wing list is **change-boundary driven**: one wing per "last release before X
changed", never one per release. That is what keeps it short. It is also how the
list quietly stopped once — it ended at the last release before `user_version`
existed and stayed there while four more releases shipped, so every wing was
captured unstamped and the stamped path, which is what every user is on, had no
coverage at all. A suite can only test the wings it has, so it went on passing.

`a_schema_version_that_advances_past_the_corpus_fails_here` in the suite is the
guard against a repeat. It holds an acknowledged version per store and fails the
moment `CURRENT_SCHEMA_VERSION` or `MEMORY_SCHEMA_VERSION` moves past it, naming
the store, the newest wing covering it, the release that produced that wing, and
what to do. Its failure is a decision to make, not a number to bump on reflex,
and the decision differs by store. For `index.db`, a release that now writes the
new version means the corpus is genuinely behind and wants a wing. For
`memory.db` no wing can ever answer: the product that stamped it below this
build has shipped its last release, so what moved is this build's own shape —
which is the boundary the refusal is drawn at — and the thing to re-check is
that every memory wing is still refused rather than opened in place.

## Adding a wing at each boundary

1. Add the release asset's sha256 to `checksums.txt`. Take it from
   `gh api repos/<slug>/releases/tags/<tag>`, not from a local download alone.
   Spell the asset the way that release spelled itself, per
   [Which name a release answers to](#which-name-a-release-answers-to).
2. Append `wing-id|tag|kind` to the `WINGS` table in `generate.sh`, and write
   the boundary reasoning into the comment above that table. That comment is
   what makes the list maintainable; a wing added without one reads as "the
   next release" and invites one wing per release forever.
3. Run `generate.sh --only <wing-id>`. It captures the wing and then runs
   `capture_expect.py` for it, which is what records its expectations and its
   `sha256` pin in `MANIFEST.json`. Entries for wings it did not build are
   carried through untouched, so the diff should be the new wing only.
4. Run `cargo test -p inkentry-cli --test upgrade_corpus`.
5. If the wing was added because a schema version advanced, update the
   acknowledged version in `upgrade_corpus.rs` (`CORPUS_COVERS_INDEX_SCHEMA` /
   `CORPUS_COVERS_MEMORY_SCHEMA`) in the same change.
6. Check the old-binary leg against
   [When this contract flips](#when-this-contract-flips). A release carrying the
   `memory.db` version guard is expected to *refuse* a newer memory store rather
   than read it, and the criterion-4 assertion has to be updated to say so
   rather than relaxed.

The test is data-driven off `MANIFEST.json`, so a new wing of an existing kind
needs no Rust changes. A genuinely new *kind* of artifact needs a builder in
`generate.sh`, a reader in `capture_expect.py`, and an opener in the test. If it
stores paths, read [Captured paths are foreign paths](#captured-paths-are-foreign-paths)
before writing assertions about them.

## Size

The corpus checks in at under 200 KB for ten wings. A captured database is
mostly the `vec0` extension's preallocated vector chunk, which is zeros: 3.8 MB
raw compresses to 28 KB. Wings are stored gzipped and expanded into a temp dir by
the test, which it would do anyway, since opening a database migrates it and
would otherwise destroy the fixture on first run.

## CI

`.github/workflows/upgrade-corpus.yml` runs on `main`, release branches, version
tags, and any pull request touching the corpus, the suite, the storage layer or
the migrations. The main leg reads the checked-in fixtures only, so it cannot be
broken by a network hiccup. One extra step downloads a pinned release to check
what an old binary does with a database the current build has already upgraded.

## The old-binary contract, as measured

Measured against the **0.9.x releases**, and true of them rather than of any
old binary forever: a pinned release opening a current database does a **clean
read**, exit 0, correct counts, entries listed, full-text hits returned, no row
lost. See "When this contract flips" below before assuming it holds for a
release you are about to add.

One wrinkle the corpus surfaced: a release whose own schema version is
below the current one re-stamps `PRAGMA user_version` down to its own on close
(v0.9.3 rewinds an `index.db` from 15 to 14; v0.9.2 pre-dates the header and
never stamps; v0.9.5 stamps what it finds). That loses no data and self-heals,
because the steps above the rewound version are individually idempotent, so the
next current-build open re-runs them as no-ops and re-stamps the current
version. The test asserts that heal rather than assuming it.

The rewind is not a v0.9.3 quirk. It falls out of how the `index.db` runner
works in every build: it returns early only when the stamp already equals its
own `CURRENT_SCHEMA_VERSION`, and otherwise runs whatever steps are above the
value it read and stamps its own version at the end. A stamp *above* its own is
therefore written back down. Anyone debugging an `index.db` whose
`user_version` went backwards is looking at an older binary having opened it,
not at corruption, and the next open by a current build repairs it.

### When this contract flips

`memory.db` behaves differently from `index.db` here, and the difference is
deliberate. `MemoryStore::open` **refuses** a store whose `user_version` is
above the build's own `MEMORY_SCHEMA_VERSION`, with an upgrade message, rather
than opening it and rewinding it. Memory is not derived data and cannot be
rebuilt, so a refusal is the designed outcome. That is the promise recorded for
`.inkentry/memory.db` in [the stability contract](../../docs/stability.md#on-disk-formats).

The guard ships from **0.9.6** onward, along with the `memory.db` version stamp
it reads; releases up to 0.9.5 predate both, which is the only reason the
clean-read result above covers `memory.db` at all. The old-binary leg is pinned
at v0.9.3, so it is still measuring a pre-guard release. Two things flip it:
pinning that leg to 0.9.6 or later, or the current build's
`MEMORY_SCHEMA_VERSION` moving above whatever the pinned release supports —
either way the old binary **refuses** the memory wing instead of listing it.

When that happens, `a_pinned_old_binary_reads_a_current_database_cleanly_and_loses_no_data`
starts failing on its `memory list` assertion. That failure is **correct** and
is the contract working. Do not weaken the assertion to make it pass: encode
the refusal instead, asserting a non-zero exit and the upgrade message, and
keep the `index.db` half of the test asserting the clean read and heal, which
is unaffected.
