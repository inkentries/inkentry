# Portable dump format

A **dump** is your project's stored data written out as a plain text file: one
JSON object per line, readable in any editor, diffable in git, and re-importable
into inkentry.

It exists so that four things are possible without asking anyone's permission:

- **Take a backup** that is not a copy of an opaque database file.
- **Inspect what is actually stored**, with `less` and `jq` rather than a SQL client.
- **Move a project between machines**, including between machines running
  different versions of inkentry.
- **Leave**, with your data in a form something other than inkentry can read.

This document is the specification. It is written for anyone implementing a
reader or a writer, including implementations that are not inkentry.

**Current version: `format_version` 1.**

## The format is expressed as entities and relationships, not as tables

A dump is **not** a serialisation of inkentry's database schema, and deliberately
so. It describes **entities** (a memory entry, a project, a recorded command) and
**relationships** between them (this entry supersedes that one).

Tables are an implementation detail that moves between releases. Entities and
relationships are what the data actually is. Because the format is defined in
those terms and owned independently of any schema, the internal storage can keep
changing without the format changing, and a reader written against this document
alone stays correct across those changes.

Two consequences follow, and both are normative:

- A field's presence in the format does **not** imply a column of that name
  exists anywhere, now or ever.
- A writer's job is to express what it holds **in these terms**, not to copy its
  rows out. Where its internal representation disagrees with the format, the
  format wins and the writer converts. The [`supersedes`
  orientation](#supersedes-orientation-from-supersedes-to) is the case where this
  matters most.

## Container

Line-delimited JSON:

- **UTF-8**, no byte-order mark.
- **One JSON object per line.** No pretty-printing, no array wrapper, no
  trailing commas.
- **Every line is terminated by a single LF (`\n`)**, including the last one. A
  dump ends with a newline.
- Every object has a `record` field naming its kind: `header`, `entity`,
  `relationship`, or `footer`.

```
{"record":"header", ...}            ← exactly one, first line
{"record":"entity", ...}            ← zero or more, any order
{"record":"relationship", ...}      ← zero or more, any order
{"record":"footer", ...}            ← exactly one, last line
```

Line-delimited JSON was chosen because it streams, because you can read and diff
it before and after an import, and because it lets every record be checksummed on
its own.

### Rules a reader must enforce

- **Exactly one `header`, as the first line. Exactly one `footer`, as the last.**
  A file missing either is invalid. It must be **refused, not partially
  imported**: a truncated dump is precisely the case where a partial import
  causes silent data loss.
- **No record may follow the footer.** A file with one is not whole and is
  refused.
- **An unrecognised `record` value is an error, not something to skip.** This
  format does not use the "ignore what you do not understand" convention;
  compatibility is handled by `format_version` instead, so an unknown record kind
  means the file is not what it claims to be.
- **`entity` and `relationship` records may appear in any order** and may be
  interleaved. A relationship may precede the entities it names. A reader
  therefore resolves endpoints after reading the whole file, never as each line
  arrives.

### Absent fields are omitted, never null

Every field marked optional below is **left out of the object entirely** when it
has no value. A writer must not emit `"status":null`, and must not emit an empty
array for `tags` or `linked_files`.

This is not cosmetic. The [footer digest](#footer-and-integrity) is computed over
the exact bytes of each line, so two writers that disagree about how to spell
"absent" produce different digests for identical data.

`""` is not a second spelling of absent either. A carried identity — `uuid`,
`entity_id`, `remote_id` — names an entry in the store it came from, so it is
either meaningful or omitted. A reader **refuses the whole dump** when one is
present but blank or whitespace-only, naming the offending record: an empty
identity is not a value any store can use, and it fails much later, describing
neither the file nor the entry it came from.

## Header

| Field | Type | Required | Meaning |
|---|---|---|---|
| `record` | `"header"` | ✓ | |
| `format` | string | ✓ | Always the literal `"portable-dump"` |
| `format_version` | integer | ✓ | `1` |
| `generated_at` | integer | ✓ | Unix seconds at which the dump was produced |
| `generator` | string | ✓ | Producing tool and version, free text, **informational only** |

```json
{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"inkentry/1.0.0"}
```

- A reader **must refuse a `format` it does not recognise**. A file that is not
  a portable dump must not be parsed optimistically.
- A reader **must refuse a `format_version` it does not implement**, rather than
  reading what it can and hoping. Version 1 readers accept `1` and nothing else.
- A reader **must not branch on `generator`.** It is there so a human can tell
  what wrote a file. A reader that changes behaviour based on it has coupled
  itself to a specific producer, which is the exact thing this format exists to
  avoid.

## Entities

Every entity record carries three common fields:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `record` | `"entity"` | ✓ | |
| `type` | string | ✓ | `memory_entry`, `project`, or `command_usage` |
| `ref` | string | ✓ | Dump-local reference, see below |

### `ref` is wiring, not identity

`ref` is an **opaque** string, **unique within this one dump**, whose only
purpose is to let relationship records name their endpoints.

- It is **not identity**. It says nothing about what the entity is.
- It is **not stable across dumps**. Exporting the same store twice after any
  change may produce entirely different `ref` values for the same entities.
- A reader **must not persist it**, must not store it as a foreign key, and must
  not use it to match against anything outside the file it came from.

Treat `ref` values as opaque tokens. Do not parse them, and do not infer entity
type or ordering from their shape.

### `memory_entry`

| Field | Type | Required | Notes |
|---|---|---|---|
| `ref` | string | ✓ | Dump-local |
| `uuid` | string | | Stable identity, carried verbatim when the writer has one. See [identity policy](#identity-policy-belongs-to-the-reader). Absent means the reader assigns one. |
| `kind` | string | ✓ | The entry's kind (`decision`, `requirement`, `note`, and so on) |
| `title` | string | ✓ | |
| `body` | string | ✓ | |
| `tags` | array of string | | **An array.** A writer whose storage joins tags into one delimited string splits it; the joined form is not the format. Omitted when empty. |
| `linked_files` | array of string | | Same. Omitted when empty. |
| `created_at` | integer | ✓ | Unix seconds. Required precisely so a reader can seed a time-ordered identifier from it. |
| `status` | string | | Defaults to `active` when absent |
| `source_ref` | string | | Provenance, carried verbatim. Never replaced with an import marker. |
| `valid_at` | integer | | Unix seconds |
| `invalid_at` | integer | | Unix seconds |
| `entity_id` | string | | Content-addressed convergence key. Carried verbatim, **never recomputed** by either side. |
| `remote_id` | string | | |
| `namespace` | string | | Multi-tenant stores only. Absent means the single default store. |

```json
{"record":"entity","type":"memory_entry","ref":"e1","uuid":"0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33","kind":"decision","title":"Fail closed with no local project","body":"...","tags":["locked"],"linked_files":["src/config.rs"],"created_at":1786300000,"status":"superseded","source_ref":"commit:abc1234","entity_id":"9f2c..."}
```

`created_at` is the one timestamp that is never optional. The reason is in
[identity policy](#identity-policy-belongs-to-the-reader).

### `project`

| Field | Type | Required | Notes |
|---|---|---|---|
| `ref` | string | ✓ | Dump-local |
| `root_path` | string | ✓ | The project root as recorded |
| `registered_at` | integer | | Unix seconds |

```json
{"record":"entity","type":"project","ref":"p1","root_path":"/home/u/alpha","registered_at":1786200500}
```

**A project's store path is deliberately absent.** It is always derivable from
the project root, and a reader lays its stores out according to its own
conventions, which need not match the writer's. Carrying the path would carry a
value that is wrong on the far side. **A reader derives it.** This is both more
portable and less coupled, and it is the general rule: the format carries
authored data and omits anything derived from it.

### `command_usage`

| Field | Type | Required | Notes |
|---|---|---|---|
| `ref` | string | ✓ | Dump-local |
| `command` | string | ✓ | |
| `at` | integer | ✓ | Unix seconds |

```json
{"record":"entity","type":"command_usage","ref":"u0","command":"search","at":1786200700}
```

## Relationships

| Field | Type | Required | Meaning |
|---|---|---|---|
| `record` | `"relationship"` | ✓ | |
| `type` | string | ✓ | `supersedes`, `relates_to`, `contradicts`, or `depends_on` |
| `from` | string | ✓ | The `ref` of an entity in this dump |
| `to` | string | ✓ | The `ref` of an entity in this dump |
| `created_at` | integer | | Unix seconds |

```json
{"record":"relationship","type":"supersedes","from":"e2","to":"e1","created_at":1786350000}
```

`supersedes`, `relates_to`, and `contradicts` link `memory_entry` entities.
`depends_on` links `project` entities.

### `supersedes` orientation: `from` supersedes `to`

**`from` is the successor. `to` is the entry being replaced.** In the example
above, `e2` supersedes `e1`; `e1` is the older, now-replaced entry.

This is the sharpest correctness trap in the format, and it is worth being blunt
about why. The same fact is commonly stored the other way round: as a
"superseded_by" property hanging off the **predecessor** and pointing forward at
its successor. That encoding and this one are inverses. **A writer whose internal
storage encodes supersession in the opposite direction must invert it on the way
out.** A writer that holds both encodings and emits both without inverting one
will produce two contradictory edges for a single fact, and nothing else in the
format will catch it.

Readers **deduplicate on `(type, from, to)`**. Two records with the same triple
are one relationship. Where duplicates carry different `created_at` values, the
recorded timestamp is preferred over its absence.

### Endpoints must resolve

**Both `from` and `to` must name an entity `ref` present in the same dump.** An
endpoint that does not resolve is a **hard error that refuses the whole dump**,
not a relationship to skip.

Skipping would be the wrong call: a dangling endpoint means either the file is
damaged or the writer is broken, and both are conditions where importing most of
the data and saying nothing is worse than importing none of it.

Because record order is unconstrained, this check runs after the whole file has
been read.

## Footer and integrity

| Field | Type | Required | Meaning |
|---|---|---|---|
| `record` | `"footer"` | ✓ | |
| `counts` | object | ✓ | `{"entity":{<type>:<n>},"relationship":{<type>:<n>}}` |
| `digest` | string | ✓ | `sha256:<hex>` |

```json
{"record":"footer","counts":{"entity":{"memory_entry":3},"relationship":{"contradicts":1,"relates_to":1,"supersedes":1}},"digest":"sha256:210e1420ea0e650622873d8ab201e380a16774ef5ebc37995bc270fe994fcff5"}
```

Both `entity` and `relationship` objects are always present in `counts`, and are
empty objects (`{}`) when the dump holds none of that kind. A dump of an empty
store is a valid dump: a header, a footer with empty counts, and nothing between
them. That is meaningfully different from a missing file, and it is how a reader
learns "there was nothing here" rather than "this was never exported".

### Computing the digest

Two steps, and both details below are load-bearing for interoperability.

1. **Per-record digest.** For each record, SHA-256 over **that record's own
   serialized line bytes, excluding the line terminator**. Render as
   **lowercase hexadecimal**.
2. **Whole-dump digest.** SHA-256 over the **concatenation of those hex digest
   strings, as ASCII text, in file order**. Prefix the result with `sha256:`.

Precisely:

- The fold is over the **hex text**, not over the raw 32-byte digests. Folding
  raw bytes produces a different, wrong answer.
- The **header contributes**, so a tampered header is caught by the same check as
  a tampered entity.
- The **footer does not contribute**. It is the record carrying the result.
- The fold is **order-sensitive**: reordering records changes the digest, so a
  reordered dump is a different dump even though its counts still agree.

Reference implementation of the check, in Python:

```python
import hashlib

lines = open("project.dump", "rb").read().splitlines()
per_record = [hashlib.sha256(line).hexdigest() for line in lines[:-1]]
digest = "sha256:" + hashlib.sha256("".join(per_record).encode()).hexdigest()
```

Together this gives all three properties at once: a per-record checksum, an
order-sensitive whole-file digest, and an integrity check that turns a corrupted
or truncated dump into a loud refusal instead of a quiet partial import.

### What a reader must do with it

**A reader must recompute both `counts` and `digest`, and refuse the whole dump
on any mismatch. It must never import partially.**

Not one or the other: `counts` catches a removed record, and `digest` catches an
altered or reordered one. A single flipped byte anywhere in the file must make
the import fail.

## Two properties that keep readers and writers independent

These are the reason a writer and a reader can be built by different people, at
different times, against this document alone.

### Identity policy belongs to the reader

**A writer carries a `uuid` when it has one and omits it otherwise. It never
mints one.**

A writer that invented identifiers would be imposing its own identity scheme on
every reader, and would produce different identifiers each time it ran. A writer
should also withhold an identifier that is not a genuine stable identity: an id
that reflects arrival order rather than creation order is not the entry's
identity, and presenting it as one hands the reader an ordering it did not
choose.

**A reader assigns identity to every entity lacking one, seeded from that
entity's own `created_at`, never from the wall clock.**

The wall clock is wrong here for a specific reason: a reader processes an entire
back catalogue in a single pass, so wall-clock assignment would stamp all of
history with one instant, destroying the time ordering that a time-ordered
identifier exists to carry. Seeding from each entity's own creation time
preserves it. **This is why `created_at` is a required field.**

### The dump carries no secret material of any kind

By construction, a dump contains **no configuration, no credentials, and no
tokens**. There is no field in this specification that can hold one, and no
entity type that corresponds to a secret.

This is a property of the format, not a filtering step that a writer performs
and might get wrong. It is what makes a dump safe to commit to a repository,
attach to an issue, or hand to someone else. Implementations should assert it
with a test rather than by inspection.

Dump files are still ordinary files holding whatever you wrote into your memory
entries, so treat their contents with the same care as the entries themselves.

## A complete example

A dump of three memory entries, one of which supersedes another:

```
{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"inkentry/1.0.0"}
{"record":"entity","type":"memory_entry","ref":"e1","uuid":"0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33","kind":"decision","title":"Old choice","body":"we did X","tags":["a","b"],"linked_files":["src/x.rs"],"created_at":1000,"status":"superseded","source_ref":"commit:abc","entity_id":"ent-1"}
{"record":"entity","type":"memory_entry","ref":"e2","kind":"decision","title":"New choice","body":"we now do Y","created_at":2000,"status":"active","valid_at":1500}
{"record":"entity","type":"memory_entry","ref":"e3","kind":"note","title":"Aside","body":"related thing","created_at":3000,"remote_id":"rem-9"}
{"record":"relationship","type":"contradicts","from":"e1","to":"e3"}
{"record":"relationship","type":"relates_to","from":"e3","to":"e2","created_at":3100}
{"record":"relationship","type":"supersedes","from":"e2","to":"e1","created_at":2500}
{"record":"footer","counts":{"entity":{"memory_entry":3},"relationship":{"contradicts":1,"relates_to":1,"supersedes":1}},"digest":"sha256:210e1420ea0e650622873d8ab201e380a16774ef5ebc37995bc270fe994fcff5"}
```

Reading it: `e2` supersedes `e1`, so `e1` is the replaced entry. `e2` has no
`uuid`, so a reader assigns it one seeded from `created_at` 2000. `e2` also has
no `tags` and no `source_ref`, which is why those fields are absent rather than
empty or null.

## Compatibility

`format_version` is the only compatibility mechanism. There is no field-level
negotiation and no partial understanding.

- **Within a version**, change is additive only. The meaning of every field above
  is fixed, and field names are never repurposed. New optional fields may appear,
  and a reader must tolerate an optional field it does not know.
- **Across versions**, a reader refuses what it does not implement. Nothing is
  best-effort parsed.

Any change that a version 1 reader could not handle correctly, including a new
entity type, a new relationship type, or a new required field, is a
`format_version` bump rather than an addition.

Under the [stability contract](stability.md), the dump format is a **stable**
surface: `format_version` 1 stays readable for the life of the major version.
