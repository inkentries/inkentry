# Example: Onboarding a new codebase

You've been handed a large project you've never seen before. Here's how to get up to speed quickly with `spelunk`.

> Steps marked **(requires server)** need an embedding model running. All other steps work with just the binary.

## Step 1: Understand the structure

Start with what's already committed — no indexing needed:

```bash
# Check if there are any stored memory entries from previous contributors
spelunk memory list --kind context --limit 20
spelunk memory list --kind decision --limit 10
```

If someone has used spelunk on this repo before, you'll find architectural context here.

## Step 2: Find the key entry points

```bash
# Trace what the main symbols call and what calls them
spelunk graph main
spelunk graph Application --kind calls

# Full-text search for entry points
spelunk search "main function application startup" --mode text
```

## Step 3: Understand the data layer

```bash
spelunk graph Database --kind calls
spelunk search "storage persistence database" --mode text
```

## Step 4: Find the API surface

```bash
spelunk search "HTTP handler route endpoint" --mode text
spelunk graph Router --kind calls
```

## Step 5: Understand error handling

```bash
spelunk search "error handling propagation" --mode text
spelunk graph Error --kind extends
```

## Step 6: Store what you've learned

```bash
spelunk memory add \
  --title "This service is a payment processor wrapping Stripe" \
  --body "Entry point: cmd/server/main.go. Core domain: pkg/payments/. REST API in pkg/api/. PostgreSQL via GORM." \
  --kind context \
  --tags architecture,overview

spelunk memory add \
  --title "Errors are wrapped with pkg/errors and logged at the handler boundary" \
  --kind context \
  --tags error-handling
```

Future sessions (and future agents) start from your notes rather than re-discovering the same things.

## Step 7: Check what tests exist

```bash
spelunk search "test suite integration unit" --mode text
spelunk graph TestSuite --kind calls
```

## Step 8: Semantic deep-dive (requires server)

With a server running and a built index, these commands give richer results:

```bash
spelunk search "core interfaces abstractions domain objects" --graph
spelunk explore "Give me a high-level overview of this codebase. What does it do and how is it structured?"
spelunk explore "What is the error handling strategy? How are errors propagated and surfaced to users?"
spelunk explore "How is this project built and deployed?"
```

After this session you'll have a solid mental model and a set of memory entries that make every future session faster.
