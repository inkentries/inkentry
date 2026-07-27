# Upgrade corpus (the "DB museum")

Artifacts written by real, released spelunk binaries, kept so every future build
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

crates/spelunk-cli/tests/
  fixtures/upgrade-corpus/
    MANIFEST.json     one entry per wing: producer, artifact, digest, expected content
    wings/<id>/       the artifact itself, gzipped
  upgrade_corpus.rs   opens every wing with the current build and asserts
```

## The wings

| wing | producer | what it pins |
| --- | --- | --- |
| `index-v0.9.2-pre-user-version` | 0.9.2 | the last release before `index.db` grew `PRAGMA user_version`, so its version has to be inferred from table shapes |
| `index-v0.8.3-float768` | 0.8.3 | the last release that wrote `FLOAT[768]` vectors, with vectors actually in the table |
| `memory-v0.9.3-pre-entity-id` | 0.9.3 | the last release before memory entries grew a content-addressed `entity_id` |
| `memory-v0.9.5` | 0.9.5 | entity-id era, with a supersede chain and a separately archived entry |
| `registry-v0.9.5` | 0.9.5 | two registered projects and a dependency link |
| `git-notes-eras` | 0.7.1 / 0.9.3 / 0.9.5 | all three note-writing eras on one `refs/notes/spelunk` |

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
Pre-1.0 releases embed by calling a `spelunk-server` that shipped an embedder
which no longer exists, and a current server answers on a wire shape those
binaries cannot parse, so neither can produce these wings. `embed_stub.py`
serves that era's `/v1/health` and index-embed endpoints so the real old binary
can complete a real index run. This applies to **every wing whose capture runs
`spelunk index`**, which is both index wings and both memory wings, not only
the 768-dimension one. Vector values are irrelevant to a migration test: what
is asserted is that the right number of vectors survives, and that the
dimension-upgrade path discards 768-dimension ones wholesale.

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

`checksums.txt` pins the release binaries, which says nothing about the
artifacts. The artifacts are pinned separately by the `sha256` recorded per
wing in `MANIFEST.json`, which the suite checks before asserting anything else.
A wing that is edited or regenerated without its expectations being recaptured
fails the suite rather than quietly asserting one artifact's contents against
another's.

## Regenerating

Needs `gh` (authenticated), `python3`, `sqlite3`, and `git`. No spelunk-server
and no model download.

```sh
scripts/upgrade-corpus/generate.sh              # every wing
scripts/upgrade-corpus/generate.sh --list
scripts/upgrade-corpus/generate.sh --only index-v0.9.2-pre-user-version
```

Wings are only rewritten when `--only` names them, so touching one does not
churn the others.

## Adding a wing at each release

1. Add the release asset's sha256 to `checksums.txt`. Take it from
   `gh api repos/<slug>/releases/tags/<tag>`, not from a local download alone.
2. Append `wing-id|tag|kind` to the `WINGS` table in `generate.sh`.
3. Run `generate.sh --only <wing-id>`.
4. Run `cargo test -p spelunk-cli --test upgrade_corpus`.

The test is data-driven off `MANIFEST.json`, so a new wing of an existing kind
needs no Rust changes. A genuinely new *kind* of artifact needs a builder in
`generate.sh`, a reader in `capture_expect.py`, and an opener in the test.

## Size

The corpus checks in at well under 100 KB. A captured database is mostly the
`vec0` extension's preallocated vector chunk, which is zeros: 3.8 MB raw
compresses to 28 KB. Wings are stored gzipped and expanded into a temp dir by
the test, which it would do anyway, since opening a database migrates it and
would otherwise destroy the fixture on first run.

## CI

`.github/workflows/upgrade-corpus.yml` runs on `main`, release branches, version
tags, and any pull request touching the corpus, the suite, the storage layer or
the migrations. The main leg reads the checked-in fixtures only, so it cannot be
broken by a network hiccup. One extra step downloads a pinned release to check
what an old binary does with a database the current build has already upgraded.

## The old-binary contract, as measured

A pinned release opening a current database does a **clean read**, never a
refusal: exit 0, correct counts, entries listed, full-text hits returned, no row
lost. One wrinkle the corpus surfaced: a release whose own schema version is
below the current one re-stamps `PRAGMA user_version` down to its own on close
(v0.9.3 rewinds an `index.db` from 15 to 14; v0.9.2 pre-dates the header and
never stamps; v0.9.5 stamps what it finds). That loses no data and self-heals,
because the steps above the rewound version are individually idempotent, so the
next current-build open re-runs them as no-ops and re-stamps the current
version. The test asserts that heal rather than assuming it.
