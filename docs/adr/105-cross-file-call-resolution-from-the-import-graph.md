# ADR-105: Cross-file call resolution from the import graph

**Date:** 2026-10-09
**Deciders:** Founder (Johan)
**Relationship to prior ADRs:** the cross-file tier that
[ADR-097](097-symbol-resolution-layer-above-tree-sitter.md) deferred. It fills
the `graph_edges.target_file` column ADR-097 added, for edges the intra-file
`locals.scm` tier leaves unresolved, and keeps ADR-097's meaning for that
column. It confirms ADR-097's rejection of `tree-sitter-stack-graphs` and of a
live language server, and keeps SCIP ingest out of scope. It adds an `index.db`
schema step, numbered after
[ADR-104](104-embed-a-subset-of-code-chunks.md)'s.

## Context

ADR-097 shipped the intra-file tier: a call whose callee is defined in the
caller's own file carries that file as `target_file`. A call to a name
imported from another module stays `NULL`, and every consumer still joins it
on the bare name to every same-named chunk in the repository.

ADR-097 set the bar for the cross-file tier as the deductive-engine proof of
concept's crude-locality number: on lago, same file, then same directory, then
same package resolves **32%** of the call edges whose callee name is defined in
more than one file ("multi-def" edges). This ADR measures what resolution over
the code's own import statements achieves against that number, and whether 32%
is the right bar.

### Measurement

A throwaway probe read existing indexes (`graph_edges`, `chunks`, `files`)
and the source tree on disk, extracted import bindings with regular
expressions, and resolved each multi-def call edge the intra-file tier left
unresolved. Corpora:

- **lago `front/`**: React and TypeScript, 2.6k files.
- **django**: Python, 2.7k files.
- **graphiti**: Python, 270 files.
- **inkentry**: Rust.
- **lago `api/`**: Ruby. This is out of scope here and shown for reference.

The probe reproduces the proof of concept's lago denominator and crude
number on today's index: 13,319 multi-def edges, crude 31.6%.

Share of multi-def call edges bound to a single defining file:

| corpus | multi-def edges | crude locality | `locals.scm` (shipped) | + import graph | also decided as external | type-bound share |
|---|---|---|---|---|---|---|
| lago front (TS) | 2,213 | 40.3% | 26.2% | **37.1%** | 50.6% | 25% |
| django (Python) | 32,375 | 20.2% | 6.4% | **28.5%** | 33.0% | 57% |
| graphiti (Python) | 1,675 | 29.3% | 16.1% | **30.3%** | 33.9% | 71% |
| inkentry (Rust) | 11,478 | 46.4% | 20.5% | **27.6%** | 46.3% | 48% |
| lago api (Ruby) | 25,729 | 17.7% | 4.2% | (8.3%, constant-path convention) | 0% | 53% |

- **"+ import graph"** is the shipped tier plus a binding through the
  caller's imports to a repository file that defines a chunk of that name.
- **"Also decided as external"** adds the calls whose callee is bound by an
  import to a module outside the repository. Such a call cannot reach any
  repository definition. Today it joins every same-named chunk.
- **"Type-bound share"** counts calls on a value receiver (`client.send()`),
  on `self`/`this`, or on an expression. Without receiver types, no import
  rule reaches these calls.

Five findings decide the shape of this record:

1. **Crude locality is not a sound floor.** It reaches its number by guessing
   from directories. The probe looked at the cases where crude locality and
   the import graph both give an answer. Crude locality names a different file
   in 11% of them on inkentry, 27% on django and 30% on lago front. On lago's
   Ruby, against the constant-path rule, it is 93%. A hand-checked sample of
   these disagreements found the import binding right every time. Recall at
   that precision is the wrong target, so this record does not aim to beat
   32% on raw share.
2. **The import graph adds 7 to 22 points of sound resolution where
   imports are explicit.** It adds 11 points over the shipped tier on
   TypeScript, 22 on django, 14 on graphiti and 7 on Rust. Hand-checked
   samples of bound and external calls on each corpus were correct
   throughout.
3. **External bindings are the largest precision gain.** Calls bound to a
   package outside the repository are 13.5% (TypeScript), 4.5% (django) and
   18.8% (Rust) of multi-def edges. They are also 1.7% to 6.8% of
   *single-def* edges, which today join the one same-named repository chunk
   wrongly: `std::fs::write` joins a repository `write`, and
   `@testing-library/react`'s `render` joins a repository `render`. On
   django, dropping those edges cuts the module graph's cycle membership
   from 207 modules to 158. Crude locality reports 233.
4. **The residual is type-bound.** Value-receiver and `self` calls are a
   quarter of the TypeScript residual and half or more of the Python, Rust
   and Ruby residual. They need receiver types, which is SCIP's territory,
   not this record's.
5. **In-repository packages must resolve.** lago front imports
   `lago-design-system`, a workspace package in `front/packages/`. A rule that
   treats every bare specifier as external marks those calls external
   wrongly.

Two observations fall outside this decision:

- **lago's TypeScript was invisible to the original probe.** 31,061 of
  31,683 call edges in `front/` have a `NULL` `source_name`. Components are
  arrow-function constants, which the JS/TS extractor does not name as an
  enclosing scope and the chunker leaves as unnamed windows. The proof of
  concept dropped `NULL`-source edges, so its lago figures are Ruby. The TS
  row above includes them.
- **No consumer reads `target_file` yet.** PageRank counts
  `(source_name, target_name)` pairs. `search --graph` neighbours and
  `edges_for_symbol` join on the name. ADR-097 scoped consumer adoption as a
  follow-on. Until that lands, neither tier changes what a user sees.

## Decision

### 1. Resolution over the import graph, as a repository-wide pass

After the parse phase and before PageRank, `inkentry index` runs a
**cross-file resolution pass**. It binds each import-reached call edge to
the file that defines its callee, using only what the index and the
repository already hold. It adds no new dependency and makes no network
call. The same tree always yields the same assignments.

- **Which edges it handles.** The pass handles a call edge whose callee or
  qualifier the extractor bound to an import (section 3) or to an absolute
  module path. The intra-file tier never sets `target_file` on such an edge,
  because an import binding stays `NULL` under ADR-097. So the two tiers
  write disjoint rows.
- **It recomputes every run.** One file's change can rebind edges in files
  that did not change: a definition appears, or a re-export moves. So the
  pass recomputes `target_file` for every handled edge on every
  `inkentry index` run, from stored facts and the repository's module
  configuration files (section 2). It re-parses no unchanged source file.
- **The result has three states.** The column keeps ADR-097's meaning:
  - **A repository-relative path:** the file defines a chunk whose name is
    the edge's `target_name`, reached through the import chain.
  - **The empty string:** the binding leads outside the repository (section
    2, "External"). No repository chunk has an empty path, so the two-column
    join finds nothing. This is the precision gain: the edge no longer joins
    a same-named chunk. Unlike `NULL`, the empty string does not fall back to
    the bare-name join.
  - **`NULL`:** everything else. That covers an unresolvable specifier, a
    chain that renames the callee (`export { a as b }`), a chain that is
    ambiguous or deeper than 16 hops, and a target file with no chunk of
    that name. All of these keep today's bare-name behaviour.
- **Cardinality is unchanged.** The pass updates rows in place. It never
  splits a row and never deletes one. An external binding is marked rather
  than removed, so a later run can rebind it if the package moves into the
  repository.

### 2. Specifier-to-file rules for the first languages

A specifier only ever resolves to a path in the **indexed file set**. The pass
never probes the filesystem for a path, so a configured alias cannot reach
outside the repository or into an excluded file.

**External** is claimed only on positive evidence, as each language defines
it below. A specifier that is neither in-repository nor positively external
stays `NULL`.

#### TypeScript and JavaScript (`typescript`, `tsx`, `javascript`, `jsx`)

- **Bindings:**
  - `import d from 's'` binds `d` to `s`'s `default`.
  - `import { a, b as c } from 's'` binds `a` and `c`.
  - `import * as n from 's'` binds `n` to the module.
  - `import type` binds the same way.
  - `require('s')`, plain or destructured, binds the same way the intra-file
    tier already recognises it.
  - Dynamic `import()` is not a binding.
- **Re-exports** (a module's own bindings that another module can reach):
  - `export { a, b as c } from 's'`
  - `export * from 's'`
  - `export * as n from 's'`
  - `export { a }` of a name the file imported
- **Specifier classes**, tried in order:
  1. **Relative** (`./`, `../`): resolved against the importer's directory.
  2. **Path alias:** a pattern in `compilerOptions.paths` of the nearest
     enclosing `tsconfig.json` or `jsconfig.json`, relative to its `baseUrl`.
     The config's `extends` is followed when it resolves to a repository file.
  3. **In-repository package:** the specifier equals, or is a `/`-prefix of,
     the `name` of a `package.json` in the repository outside `node_modules`.
     The bare name resolves to the first of `source`, `types`, `module` and
     `main` that probes to an indexed file, then to `src/index.*`, then to
     `index.*`. A subpath resolves under the package directory.
  4. **Anything else** that is a bare specifier, including Node built-ins
     and `node:` specifiers, is **external**.
- **Probing:** a candidate path resolves to:
  1. the path itself, when it is an indexed file;
  2. otherwise the path plus `.ts`, `.tsx`, `.d.ts`, `.js`, `.jsx`, `.mjs` or
     `.cjs`;
  3. otherwise `<path>/index` plus the same extensions.

  A `.js`, `.jsx` or `.mjs` specifier also tries its `.ts`, `.tsx` or `.mts`
  sibling, the ESM-in-TypeScript convention. An alias or package path that
  probes to nothing is `NULL`, never external.
- **Lookup of a name in a module file:**
  1. A chunk of that name in the file wins.
  2. Otherwise a named re-export of that name is followed.
  3. Otherwise each `export *` is followed. More than one hit is ambiguous,
     so the result is `NULL`.
  4. `default` resolves to the file's `export default` declaration when it
     names a chunk. Under the rename rule, the edge binds only when that name
     is its `target_name`, as in `import Foo from './Foo'`.
- **Call shapes resolved:**
  - a bare call to a named or default import;
  - `n.f()` through a namespace import;
  - `C.m()` where `C` is an imported class or object, and its defining file
    has a chunk `m`.

#### Python

- **Bindings:**
  - `import a.b` binds `a`.
  - `import a.b as n` binds `n` to `a.b`.
  - `from m import x [as y]` binds `y`, or `x` when there is no `as`. This
    includes parenthesised and multi-line forms.
  - Relative `from . import x` and `from .m import x` resolve against the
    importer's package.
  - `from m import *` is a star binding.
- **Re-exports:** in Python every module-level import is an attribute of the
  module. A module's own `from … import` and star imports are therefore its
  re-exports.
- **Source roots:** the repository root, plus every directory that is the
  parent of a top-level package. A top-level package is a directory with an
  `__init__.py` whose parent has none. This rule covers the `src/` layout.
  Module `a.b` resolves to `<root>/a/b.py` or `<root>/a/b/__init__.py`. When
  several roots match, the importer's own root wins. When none of those
  decides it, the result is `NULL`.
- **External:** the module's top-level name is neither a `.py` file stem nor
  a directory holding `.py` files anywhere in the indexed file set. This
  includes the standard library and installed packages.
- **Lookup of a name in a module file:**
  1. A chunk of that name.
  2. Otherwise a module-level binding of that name, followed.
  3. Otherwise each star import, followed.
  4. Otherwise, for a package's `__init__.py`, a submodule of that name.
- **Call shapes resolved:**
  - a bare call to a `from`-import;
  - `mod.f()` and `pkg.sub.f()` through module bindings, walking submodules;
  - `C.m()` where `C` is an imported class and its file has a chunk `m`.

  `self.m()` and `cls.m()` through a base class in another file are **not**
  resolved here: that needs class hierarchy, not imports.

#### Rust

- **Bindings:** `use` trees, with nested groups, `as` renames, `self` in a
  group, and `*` globs. `pub use` and `pub(…) use` are re-exports. A path
  call `a::b::f()` needs no `use`.
- **Path roots:**
  - `crate` is the importer's crate.
  - `self` and `super` are relative to the importer's module.
  - A workspace crate name is the package `name` from its `Cargo.toml`, with
    `-` as `_`. It maps to that crate's library root.
  - A name bound by a `use` in the same file follows that binding, as Rust
    2018 uniform paths do.
  - A child module of the current module is one whose file exists.
- **External:** the root is `std`, `core`, `alloc` or `proc_macro`, or a
  dependency declared in any `Cargo.toml` in the repository. Prelude names
  are also external when no binding or same-file definition shadows them:
  `Vec`, `String`, `Box`, `Option`, `Result`, `Default` and the other
  prelude items.
- **Module tree:**
  - Crate roots are `src/lib.rs` and `src/main.rs`. `src/bin/*.rs`,
    `tests/*.rs`, `benches/*.rs` and `examples/*.rs` are crate roots too,
    with their child modules beside them.
  - Module `a::b` resolves to `<root dir>/a/b.rs` or `<root dir>/a/b/mod.rs`.
    A non-`mod.rs` file `x.rs` keeps its children in `x/`.
  - `#[path]` attributes and inline `mod x { … }` blocks are not followed:
    the result is `NULL`.
- **Lookup of an item in a module file:**
  1. A chunk of that name.
  2. Otherwise a `pub use` of that name, followed.
  3. Otherwise each `pub use …::*` glob, followed.
  4. Otherwise a child module of that name.
- **Call shapes resolved:**
  - a bare call to a `use`d function;
  - `module::f()`;
  - `Type::method()` where `Type` resolves to a file holding a chunk
    `method`.

  An `impl Type` block in a different file from `Type` is not found. That
  needs the chunker to record an impl's self type, and is out of scope.

### 3. What the extractor records, and schema step 21

A file's import facts and its call edges' import bindings come only from
parsing that file. The pass must not re-parse unchanged files, so the
extractor stores both:

- **`graph_edges` gains two nullable columns.**
  - **`import_specifier`** is the module the callee is reached through,
    as written: `./utils`, `django.db`, `crate::indexer::filter`.
  - **`import_path`** is the dot-joined chain from the imported binding to
    the callee: `render`, `models.CharField`, `IndexFilter.build`,
    `default`.

  Both are set only on call edges whose callee, or the head of whose static
  qualifier, the in-file pass bound to an import, and on Rust absolute path
  calls. They are `NULL` everywhere else, so cardinality only changes where
  one callee is reached through two different imports. Both join the
  per-file dedup key.
- **A new table, `import_bindings`**, holds one row per module-scope import
  binding and re-export. It is replaced per file alongside `graph_edges`.
  Columns:

  | column | meaning |
  |---|---|
  | `source_file` | the file holding the binding |
  | `local_name` | the name it introduces; `*` for a star or glob |
  | `specifier` | as written |
  | `imported_name` | `NULL` when the binding is the module itself |
  | `reexport` | whether another module can reach it through this file |
  | `line` | where the binding appears |

  The pass reads it to follow re-exports. Calls do not join to it, because
  the edge already carries its binding.
- **The step migrates; it does not rebuild.** The schema change is one
  registered step, `(21, IndexMigrationKind::Migrate(…))`, with its DDL in
  `migrations/index_021.sql`. `CURRENT_SCHEMA_VERSION` moves from 20 to 21,
  and `index_001_initial.sql` stays frozen. Existing edges get `NULL` in both
  columns, which keeps today's behaviour. The step records the graph
  re-extraction as owed, through the same `index_meta` marker ADR-097's step
  uses and only when the store holds edges. The next `inkentry index`
  re-extracts edges and bindings for every file without re-chunking or
  re-embedding, then runs the pass.
- **`plumbing graph-edges` JSONL:** a resolved edge carries `target_file` as
  it does today. An external one carries `"external": true` and no
  `target_file`. These are additive fields. `import_specifier` and
  `import_path` are not emitted: they are inputs to resolution, not a
  contract.

### 4. Out of scope

- **Ruby.** Rails binds constants by the autoload naming convention, not by
  an import. A constant-path rule (`Orders::UpdateService` maps to
  `…/orders/update_service.rb`) measured +4.1 points on lago's Ruby, with the
  answer right where checked. It is a convention, not a stated binding. It
  needs its own soundness rules, such as autoload roots, inflections and
  lexical constant nesting, so it is a separate decision if wanted.
- **Go, Java, C#, Kotlin, Swift, PHP, C and C++.** There is no measured
  corpus for them here. Go's same-package calls are a directory rule rather
  than an import rule. Go's qualified call edges also store `x.Method` rather
  than the method name, so they do not join chunks yet.
- **The type-bound residual (Mode 2)** and **SCIP ingest.** SCIP would reach
  the receiver calls this record cannot, but it needs each language's
  toolchain and a buildable project. Nothing consumes `target_file` yet, so
  an opt-in ingestion tier built now would be infrastructure ahead of any
  proven value. It is deferred, not rejected. Revisit it once a consumer
  shows a ranking or impact gain from resolution.
- **Consumer adoption** (PageRank, `search --graph`, graph neighbours), as
  ADR-097 scoped it.
- **Live LSP** stays a permanent boundary (ADR-097 §3).

## Alternatives considered

- **`tree-sitter-stack-graphs`.** Still rejected, for ADR-097's reason: it
  is archived and pinned to an incompatible tree-sitter line.
- **SCIP as the cross-file tier.** It is type-aware, and better on the
  residual, but it is not hermetic and needs a buildable project per
  language. It cannot be core indexing. As an opt-in tier it is deferred
  until something reads its output.
- **Crude locality (same directory or package) as a fallback after the
  import graph.** Rejected. It disagrees with a stated import binding in 11%
  to 93% of the cases where both answer. Shipping a guess into a column
  consumers will trust as a binding undoes the precision the column exists
  for.
- **Resolving at extraction time, per file.** Rejected. Whether a specifier
  resolves, and what the target module re-exports, depends on other files.
  A per-file answer goes stale when those files change while the importer
  does not.
- **Deleting external-bound edges, as the intra-file tier suppresses calls
  to parameters.** Rejected. A deleted row cannot be rebound when the
  package later moves into the repository, because the importer is not
  re-extracted. Marking keeps the pass a pure function of stored facts.
- **Joining calls to `import_bindings` by local name, instead of carrying the
  binding on the edge.** Rejected. The in-file pass already knows which
  binding, at which scope, a call reaches. A join by name loses
  function-local imports and aliases that the in-file tier has already
  rewritten.

## Consequences

- **What it adds:** sound resolution of 7 to 22 more points of multi-def
  call edges on TypeScript, Python and Rust. It also marks external calls:
  4.5% to 18.8% of multi-def edges and 1.7% to 6.8% of single-def edges no
  longer join an unrelated repository chunk once a consumer reads the
  column. The module graph built on resolved edges shows fewer spurious
  cycles than crude locality (django: 158 modules in cycles against crude's
  233).
- **The schema step:** `index.db` moves to 21 in place, keeping embeddings.
  One graph-only re-extraction is owed after upgrade, the same cost
  ADR-097's step had.
- **Cost per run:** the pass reads stored rows and the repository's
  `tsconfig.json`, `jsconfig.json`, `package.json` and `Cargo.toml` files. It
  runs on every index run, including one with nothing to re-parse, so it
  must stay small next to the parse phase. Its inputs are a few rows per
  import-reached call.
- **Security:** the new inputs are repository files the indexer already
  walks, read under the same ignore and sensitive-file rules. A specifier
  only ever maps to a path in the indexed file set, so an alias cannot name
  a path outside the repository. A config file that fails to parse degrades
  to `NULL` for the specifiers it governs; it never fails the index. Values
  reach SQLite as bind parameters only. There is no network and no new trust
  boundary, so `THREAT-MODEL.md` is unchanged and
  `egress_containment.rs` must stay green.
- **Value waits on consumers.** Like ADR-097's tier, nothing a user sees
  changes until PageRank or `search --graph` reads `target_file`. That
  adoption needs its own ranking evaluation.

## Validation the implementation must produce

On lago `front/`, django, graphiti and inkentry, re-run the probe's three
measures against the shipped `locals.scm` baseline, as the table above
reports them:

- the bound share of multi-def call edges;
- the external share;
- the module graph's edge and cycle counts.

Each language's numbers should land within a few points of this record's.
Report lago's Ruby unchanged, for continuity with ADR-097.
