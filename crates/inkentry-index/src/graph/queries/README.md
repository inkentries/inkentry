# Vendored `locals.scm` scope queries

Every `<lang>/locals.scm` file in this directory is copied unmodified from
[nvim-treesitter](https://github.com/nvim-treesitter/nvim-treesitter) at commit
`f603a2f4da48728f80257fb5fbb90145fd1dc173`, path
`runtime/queries/<lang>/locals.scm`. nvim-treesitter is licensed under the
Apache License 2.0, and a copy of that licence is in `LICENSE` next to this
file. `ruby/locals.scm` also keeps the MIT notice it carries upstream.

Upstream composes some of these queries with an `; inherits:` header. The
indexer does the same in `../queries.rs` instead of editing the files:

| Indexer language      | Query files          |
|-----------------------|----------------------|
| `javascript`, `jsx`   | `ecma` + `javascript` |
| `typescript`, `tsx`   | `ecma` + `typescript` |
| `cpp`                 | `c` + `cpp`           |
| `php`                 | `php_only`            |

`tsx/locals.scm` and `php/locals.scm` upstream are nothing but that
`; inherits:` line, so they are not copied.

To update the queries, copy the same paths from a newer commit, update the
commit hash above, and run the indexer's query tests. They compile every query
against the grammar the indexer parses with, so a query that has drifted from
that grammar fails there and never at index time.
