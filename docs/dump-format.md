# Portable dump format

A **dump** is a plain-text export of a project's stored data: one JSON object
per line, diffable, re-importable with `inkentry import`.

## Why this file exists

This is the reader-side spec for `inkentry import`. Its only writer today is
`spelunk-export`, which ships with the predecessor product so that a
`memory.db` written before 1.0.0 can cross into inkentry (see
[Upgrading](upgrading.md)). There is
no `inkentry export`. If your project started at 1.0.0, nothing on your
machine writes one of these files, and you have no reason to read further.

**Git notes does not substitute for this.** Memory entries shared through git
notes already hydrate automatically on `inkentry init` — no dump needed for
those. But on a real spelunk corpus being upgraded, git notes carried only 51
of 343 memory entries (15%); the rest existed solely in the old `memory.db`,
which git notes never touched. Git notes also structurally can't carry
`project` or `command_usage` entities, or `relates_to`/`contradicts`/
`depends_on` relationships: `GitNotesBackend` refuses those operations
(`add_edge`/`get_edges` return `BackendUnsupported`) by design, since it only
ever carried individual memory entries, not the rest of the store. A dump is
the only path that carries all of it.

This document is the specification of the format used by `inkentry import`,
written for anyone implementing a reader. **Current version:
`format_version` 1.**

## Entities and relationships, not tables

A dump describes **entities** (a memory entry, a project, a recorded command)
and **relationships** between them (this entry supersedes that one) —
deliberately not a serialization of inkentry's SQL schema, which is an
implementation detail that moves between releases. Two consequences, both
normative:

- A field's presence in the format does **not** imply a column of that name
  exists anywhere, now or ever.
- A writer expresses what it holds **in these terms**, not its raw rows. Where
  its internal representation disagrees with the format, the format wins. The
  [`supersedes` orientation](#supersedes-orientation-from-supersedes-to) is
  where this matters most.

## Container

Line-delimited JSON, chosen because it streams, diffs cleanly, and lets every
record be checksummed on its own:

- **UTF-8**, no byte-order mark.
- **One JSON object per line.** No pretty-printing, no array wrapper, no
  trailing commas.
- **Every line ends with a single LF (`\n`)**, including the last.
- Every object has a `record` field naming its kind: `header`, `entity`,
  `relationship`, or `footer`.

```
{"record":"header", ...}            ← exactly one, first line
{"record":"entity", ...}            ← zero or more, any order
{"record":"relationship", ...}      ← zero or more, any order
{"record":"footer", ...}            ← exactly one, last line
```

### Rules a reader must enforce

- **Exactly one `header`, as the first line. Exactly one `footer`, as the
  last.** A file missing either is invalid and must be **refused, not
  partially imported**.
- **No record may follow the footer.** A file with one is not whole and is
  refused.
- **An unrecognised `record` value is an error, not something to skip.**
  Compatibility is handled by `format_version`, so an unknown record kind
  means the file is not what it claims to be.
- **`entity` and `relationship` records may appear in any order**, and may be
  interleaved; a relationship may precede the entities it names. A reader
  resolves endpoints only after reading the whole file.

### Absent fields are omitted, never null

Every optional field below is **left out of the object entirely** when it has
no value — never `"status":null`, never an empty array for `tags` or
`linked_files`. This is not cosmetic: the [footer digest](#footer-and-integrity)
is computed over each line's exact bytes, so two writers that disagree on how
to spell "absent" produce different digests for identical data.

`""` is not a second spelling of absent either. A carried identity — `uuid`,
`entity_id`, `remote_id` — is either meaningful or omitted. A reader **refuses
the whole dump** when one is present but blank or whitespace-only, naming the
offending record.

## Header

| Field | Type | Required | Meaning |
|---|---|---|---|
| `record` | `"header"` | ✓ | |
| `format` | string | ✓ | Always the literal `"portable-dump"` |
| `format_version` | integer | ✓ | `1` |
| `generated_at` | integer | ✓ | Unix seconds at which the dump was produced |
| `generator` | string | ✓ | Producing tool and version, free text, **informational only** |

```json
{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"spelunk-export/0.9.9"}
```

- A reader **must refuse a `format` it does not recognise**, rather than
  parsing optimistically.
- A reader **must refuse a `format_version` it does not implement**. Version 1
  readers accept `1` and nothing else.
- A reader **must not branch on `generator`.** It is informational only; a
  reader that changes behaviour based on it has coupled itself to one
  producer, the exact thing this format exists to avoid.

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

- It is **not identity** and says nothing about what the entity is.
- It is **not stable across dumps**: exporting the same store twice after any
  change may produce different `ref` values for the same entities.
- A reader **must not persist it**, must not store it as a foreign key, and
  must not match it against anything outside the file it came from.

### `memory_entry`

| Field | Type | Required | Notes |
|---|---|---|---|
| `ref` | string | ✓ | Dump-local |
| `uuid` | string | | Stable identity, carried verbatim when the writer has one. See [identity policy](#identity-policy-belongs-to-the-reader). Absent means the reader assigns one. |
| `kind` | string | ✓ | The entry's kind (`decision`, `requirement`, `note`, and so on) |
| `title` | string | ✓ | |
| `body` | string | ✓ | |
| `tags` | array of string | | **An array.** A writer whose storage joins tags into one delimited string splits it back apart. Omitted when empty. |
| `linked_files` | array of string | | Same. Omitted when empty. |
| `created_at` | integer | ✓ | Unix seconds. Required so a reader can seed a time-ordered identifier from it. |
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

`created_at` is the one timestamp that is never optional — see [identity
policy](#identity-policy-belongs-to-the-reader) for why.

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
the project root, and a reader lays its stores out by its own conventions,
which need not match the writer's — the general rule being that the format
carries authored data and omits anything derived from it.

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

This is the sharpest correctness trap in the format: the same fact is commonly
stored the other way round internally, as a "superseded_by" property on the
**predecessor** pointing forward at its successor. Those two encodings are
inverses. **A writer whose internal storage encodes supersession the other way
must invert it on the way out** — emitting both without inverting one produces
two contradictory edges for a single fact, and nothing else in the format
catches it.

Readers **deduplicate on `(type, from, to)`**: two records with the same
triple are one relationship, and where duplicates carry different `created_at`
values, the recorded timestamp is preferred over its absence.

### Endpoints must resolve

**Both `from` and `to` must name an entity `ref` present in the same dump.**
An endpoint that does not resolve is a **hard error that refuses the whole
dump**, not a relationship to skip — a dangling endpoint means the file is
damaged or the writer is broken, and importing most of the data while saying
nothing is worse than importing none of it. Because record order is
unconstrained, this check runs after the whole file has been read.

## Footer and integrity

| Field | Type | Required | Meaning |
|---|---|---|---|
| `record` | `"footer"` | ✓ | |
| `counts` | object | ✓ | `{"entity":{<type>:<n>},"relationship":{<type>:<n>}}` |
| `digest` | string | ✓ | `sha256:<hex>` |

```json
{"record":"footer","counts":{"entity":{"memory_entry":3},"relationship":{"contradicts":1,"relates_to":1,"supersedes":1}},"digest":"sha256:210e1420ea0e650622873d8ab201e380a16774ef5ebc37995bc270fe994fcff5"}
```

`entity` and `relationship` are always both present in `counts`, as empty
objects (`{}`) when the dump holds none of that kind. A dump of an empty store
is still valid — a header, a footer with empty counts, nothing between — which
is how a reader learns "there was nothing here" rather than "this was never
exported".

### Computing the digest

1. **Per-record digest.** For each record, SHA-256 over **that record's own
   serialized line bytes, excluding the line terminator**, rendered as
   **lowercase hexadecimal**.
2. **Whole-dump digest.** SHA-256 over the **concatenation of those hex digest
   strings, as ASCII text, in file order**, prefixed with `sha256:`.

Load-bearing details:

- The fold is over the **hex text**, not the raw 32-byte digests.
- The **header contributes**; a tampered header is caught the same as a
  tampered entity.
- The **footer does not contribute** — it carries the result.
- The fold is **order-sensitive**: reordering records changes the digest, even
  though counts still agree.

Reference implementation, in Python:

```python
import hashlib

lines = open("project.dump", "rb").read().splitlines()
per_record = [hashlib.sha256(line).hexdigest() for line in lines[:-1]]
digest = "sha256:" + hashlib.sha256("".join(per_record).encode()).hexdigest()
```

### What a reader must do with it

**A reader must recompute both `counts` and `digest`, and refuse the whole
dump on any mismatch. It must never import partially.** Not one or the other:
`counts` catches a removed record, `digest` catches an altered or reordered
one — a single flipped byte anywhere must fail the import.

## Two properties that keep readers and writers independent

These are what let a writer and a reader be built by different people, at
different times, against this document alone.

### Identity policy belongs to the reader

**A writer carries a `uuid` when it has one and omits it otherwise. It never
mints one** — an invented identifier would impose the writer's own scheme on
every reader and change on every run. A writer also withholds an identifier
that isn't a genuine stable identity: one reflecting arrival order rather than
creation order isn't the entry's identity.

**A reader assigns identity to every entity lacking one, seeded from that
entity's own `created_at`, never from the wall clock.** A reader processes an
entire back catalogue in one pass, so wall-clock assignment would stamp all of
history with one instant, destroying the time ordering a time-ordered
identifier exists to carry. **This is why `created_at` is required.**

### The dump carries no secret material of any kind

By construction, a dump contains **no configuration, no credentials, and no
tokens** — no field in this spec can hold one, and no entity type corresponds
to a secret. This is a property of the format, not a filtering step a writer
performs and might get wrong, and is what makes a dump safe to commit or hand
to someone else. Dump files are still ordinary files holding whatever you
wrote into your memory entries, though, so treat their contents with the same
care as the entries themselves.

## A complete example

A dump of three memory entries, one of which supersedes another:

```
{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"spelunk-export/0.9.9"}
{"record":"entity","type":"memory_entry","ref":"e1","uuid":"0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33","kind":"decision","title":"Old choice","body":"we did X","tags":["a","b"],"linked_files":["src/x.rs"],"created_at":1000,"status":"superseded","source_ref":"commit:abc","entity_id":"ent-1"}
{"record":"entity","type":"memory_entry","ref":"e2","kind":"decision","title":"New choice","body":"we now do Y","created_at":2000,"status":"active","valid_at":1500}
{"record":"entity","type":"memory_entry","ref":"e3","kind":"note","title":"Aside","body":"related thing","created_at":3000,"remote_id":"rem-9"}
{"record":"relationship","type":"contradicts","from":"e1","to":"e3"}
{"record":"relationship","type":"relates_to","from":"e3","to":"e2","created_at":3100}
{"record":"relationship","type":"supersedes","from":"e2","to":"e1","created_at":2500}
{"record":"footer","counts":{"entity":{"memory_entry":3},"relationship":{"contradicts":1,"relates_to":1,"supersedes":1}},"digest":"sha256:210e1420ea0e650622873d8ab201e380a16774ef5ebc37995bc270fe994fcff5"}
```

`e2` supersedes `e1`, so `e1` is the replaced entry. `e2` has no `uuid`, so a
reader assigns it one seeded from `created_at` 2000; it also has no `tags` and
no `source_ref`, which is why those fields are absent rather than empty or
null.

## Compatibility

`format_version` is the only compatibility mechanism — no field-level
negotiation, no partial understanding.

- **Within a version**, change is additive only. Field meaning is fixed and
  names are never repurposed; new optional fields may appear, and a reader
  must tolerate one it doesn't know.
- **Across versions**, a reader refuses what it doesn't implement. Nothing is
  best-effort parsed.

Any change a version 1 reader could not handle correctly — a new entity type,
relationship type, or required field — is a `format_version` bump, not an
addition.

Under the [stability contract](stability.md), the dump format is a **stable**
surface: `format_version` 1 stays readable for the life of the major version.

## Where this matters most

A dump is also how memory crosses a build boundary that nothing else crosses.
1.0.0 refuses a store an earlier build wrote, and a dump is the only form that
carries every entry together with its `supersedes`, `relates_to` and
`contradicts` links. See [Upgrading](upgrading.md).
