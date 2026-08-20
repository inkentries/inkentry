# Example: Onboarding a new codebase

You've been handed a large project you've never seen before. Here's how to get up to speed quickly with `inkentry`.

> Steps marked **(requires server)** need an embedding model running. Everything else works with just the binary and an index.

## Step 1: Read what previous contributors left behind

`memory list` needs no index and no server, so start here:

```bash
inkentry memory list --kind context --limit 20
inkentry memory list --kind decision --limit 10
```

If someone has used inkentry on this repo before, you'll find architectural context here.

## Step 2: Build the index

`search` and the code graph both read the index. One command sets both up:

```bash
inkentry init
```

Full-text results are available as soon as the tree is parsed; semantic ranking
builds in the background.

## Step 3: Find the key entry points

```bash
# Full-text search for entry points
inkentry search "main function application startup" --only-text

# Trace what the main symbols call and what calls them
inkentry search "main" --graph
inkentry plumbing graph-edges --symbol Application
```

## Step 4: Understand the data layer

```bash
inkentry search "storage persistence database" --only-text
inkentry plumbing graph-edges --symbol Database
```

## Step 5: Find the API surface

```bash
inkentry search "HTTP handler route endpoint" --only-text
inkentry plumbing graph-edges --symbol Router
```

## Step 6: Understand error handling

```bash
inkentry search "error handling propagation" --only-text
inkentry plumbing graph-edges --symbol Error
```

## Step 7: Store what you've learned

```bash
inkentry memory add \
  --title "This service is a payment processor wrapping Stripe" \
  --body "Entry point: cmd/server/main.go. Core domain: pkg/payments/. REST API in pkg/api/. PostgreSQL via GORM." \
  --kind context \
  --tags architecture,overview

inkentry memory add \
  --title "Errors are wrapped with pkg/errors and logged at the handler boundary" \
  --kind context \
  --tags error-handling
```

Future sessions (and future agents) start from your notes rather than re-discovering the same things.

## Step 8: Check what tests exist

```bash
inkentry search "test suite integration unit" --only-text
inkentry plumbing graph-edges --symbol TestSuite
```

## Step 9: Semantic deep-dive (requires server)

Once the embedding pass has landed, the same command ranks by meaning and
interleaves any recorded decisions on the topic:

```bash
inkentry search "core interfaces abstractions domain objects" --graph
inkentry search "high-level overview: what it does and how it is structured" --graph
inkentry search "error handling strategy: how errors are propagated and surfaced" --graph
inkentry search "how the project is built and deployed" --graph
```

For a synthesised answer, loop these primitives yourself — search, trace with
`inkentry plumbing graph-edges --symbol <symbol>`, read with
`inkentry chunks <file>`, then refine the query and repeat. See the
"Exploring: multi-hop retrieval" section of [the skill](https://github.com/inkentries/agent-plugin/blob/main/skills/inkentry/SKILL.md).

After this session you'll have a solid mental model and a set of memory entries that make every future session faster.
