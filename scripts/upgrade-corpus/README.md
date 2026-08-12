# Upgrade corpus (the "DB museum")

Artifacts written by **real released binaries**, checked in as test data and
opened by the current build.

Every other migration test in this repo builds an old shape by hand. That tests
what we *believe* the old format was. This tests what it **is**.

## Layout

```
scripts/upgrade-corpus/
  generate.sh          captures a wing by running a downloaded release
  capture_expect.py    reads expectations out of a captured artifact, with plain
                       SQL/git, before any current-build code opens it
  checksums.txt        pinned sha256 per release asset
crates/inkentry-cli/tests/fixtures/upgrade-corpus/
  MANIFEST.json        one entry per wing: producer, artifact, sha256, expectations
  wings/<id>/          the artifact itself
crates/inkentry-cli/tests/upgrade_corpus.rs   the suite
```

## The corpus holds one wing, on purpose

| wing | producer | what it pins |
| --- | --- | --- |
| `git-notes-eras` | 0.7.1 / 0.9.3 / 0.9.5 | all three note-writing eras on one ref |

**A wing earns its place by covering a path a real user's data actually takes.**
Neither local database is such a path:

- `index.db` is not carried across at all. The user reindexes.
- `memory.db` crosses as a portable dump and is imported into a store the
  current binary creates.

So no database written by an earlier release is ever opened in place by this
one. Wings for those covered migrations nothing performs, and they were removed
together with the migration ladders they were defending.

**The notes ref is the exception**, and it is the reason this harness outlived
them. It is renamed in place rather than exported, so the blobs on a migrating
user's ref arrive at the current reader exactly as the old binaries wrote them.
The eras were established by running those binaries, not by reading history:
releases up to and including **0.9.2 replace** a commit's note blob on every
add, **0.9.3 appends JSON lines without entity ids** and writes each entry
**twice**, and **0.9.5 writes the entity-keyed event log**. The double-write is
why the suite asserts an exact entry count and not just presence: a reader that
stopped folding duplicates would hand the user the same decision several times
and every content assertion would still pass.

## What is real, and what is not

The **artifacts** are real: produced by running a downloaded release binary,
pinned to the digest GitHub records for that release asset. `generate.sh`
refuses an asset that is not in `checksums.txt`, because a fixture is only
evidence about a release if the bytes are the ones that release shipped.

The **expectations** are read out of the artifact at capture time by
`capture_expect.py`, using plain SQL and git, **before any current-build code
opens it**. That is what makes them an independent record of what the old binary
wrote rather than an echo of what today's code produces.

Every filename in `checksums.txt` spells the project's former name and must keep
spelling it. Those assets shipped under that name; no rename reaches back and
changes bytes that already published. A filename here that has been modernised
is a filename that resolves to nothing.

## Adding a wing

Adding a wing is not a matter of picking a release. It is answering yes to one
question:

> Does a user's database at some shipped schema version have to survive the move
> to a newer one?

Until that is true there is nothing worth capturing.
`a_schema_version_that_advances_past_the_corpus_fails_here` is what puts the
question in front of you at the moment it stops being hypothetical: it fires
when either store's schema version moves past what the corpus was last checked
against, and names both ways out.

When the answer is yes:

1. Add the release asset's sha256 to `checksums.txt`, from
   `gh api repos/<slug>/releases/tags/<tag>`, not from a local download alone.
2. Append `wing-id|tag|kind` to the `WINGS` table in `generate.sh`, and write the
   reasoning into the comment above that table. That comment is what keeps the
   list from growing one wing per release.
3. Run `generate.sh --only <wing-id>`. It captures the wing and then runs
   `capture_expect.py` for it, which records its expectations and its `sha256`
   pin in `MANIFEST.json`. Entries for wings it did not build are carried through
   untouched, so the diff should be the new wing only.
4. Add a test to `upgrade_corpus.rs` that opens the wing and asserts its rows
   survive. The harness gives you `checkout`, the digest pin and the manifest
   types; the assertions are yours to write and should be specific enough that
   injecting the regression they guard against turns them red.
5. Update the acknowledged version constant in `upgrade_corpus.rs` in the same
   change.

## Regenerating

```sh
scripts/upgrade-corpus/generate.sh                 # every wing
scripts/upgrade-corpus/generate.sh --only <id>     # one
```

Regeneration is **scripted but not byte-reproducible**: wall-clock timestamps
and git object ids differ per run. So a regenerated wing is a genuinely new
artifact and its `sha256` and expectations are recaptured together — which is
exactly what the digest pin in the suite is there to enforce. Do not hand-edit a
checked-in artifact or its expectations; recapture instead.

`capture_expect.py --refresh` re-reads the checked-in wings without regenerating
them, for when a newly recorded expectation needs filling in on artifacts that
cannot be reproduced.

## Captured paths are foreign paths

Anything a wing records about the machine it was captured on — absolute paths in
particular — belongs to that machine. A host-OS predicate (`is_absolute` and
friends) answers for the runner, not for the artifact: a POSIX path is not
absolute to a Windows host whatever the migration did to it. Assert equality
with the captured bytes instead. It is the same question on every platform, and
it is the stronger one, since a path rewritten to some other absolute path is
still mangled.

## CI

`.github/workflows/upgrade-corpus.yml` runs the suite on `main`, release
branches and tags, and on any pull request touching the corpus, the suite, the
storage layer, the migrations or these scripts.

It keeps running while the corpus holds no database wing, deliberately. A corpus
that is empty on purpose and a corpus that quietly stopped collecting have the
same contents; the tripwire is what tells them apart, and it only fires if
something runs it.
