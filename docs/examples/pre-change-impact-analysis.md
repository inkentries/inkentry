# Example: Pre-change impact analysis

Before making a significant change, use `inkentry` to understand the blast radius.

## Scenario

You need to change the signature of a core function — say, adding a required parameter to `validate_token()`.

## Step 1: Find everything that calls it

```bash
inkentry plumbing graph-edges --symbol validate_token
```

```json
{"source_file":"auth/middleware.go","source_name":"handler","target_name":"validate_token","kind":"calls","line":45}
{"source_file":"api/routes.go","source_name":"apply_auth","target_name":"validate_token","kind":"calls","line":112}
{"source_file":"grpc/interceptor.go","source_name":"unary_auth","target_name":"validate_token","kind":"calls","line":67}
```

Edges are emitted in both directions for the named symbol, so filter on
`target_name` for callers and `source_name` for callees. `kind` is one of
`calls`, `imports`, `extends`, `implements`, or `mentions`.

## Step 2: Understand each call site

```bash
# Full-text — no server needed
inkentry search "validate_token" --only-text --limit 20

# Best available ranking, plus the symbol's 1-hop neighbours
inkentry search "validate_token" --graph --limit 20
```

## Step 3: Check memory for prior context

```bash
inkentry search "validate_token authentication" --only-memory
```

Plain `inkentry search` returns both corpora at once, so a prior decision on the
symbol surfaces alongside the code:

```bash
inkentry search "validate_token callers scope parameter" --graph
```

## Step 4: Find the tests

```bash
inkentry search "validate_token test" --only-text
```

## Step 5: Check for related documentation

```bash
inkentry search "validate_token" --only-text
inkentry search "validate_token authentication" --only-memory
```

## Step 6: Write a plan

Create a checklist as a plain markdown file in `docs/plans/`. It should cover:

- `- [ ] Update validate_token signature in src/auth/token.rs`
- `- [ ] Update call sites in middleware, routes, and interceptor`
- `- [ ] Update test fixtures and mocks`

## Step 7: After the change, verify

```bash
# Re-index changed files first — graph edges come from the index
inkentry index .

# Confirm all call sites are updated
inkentry plumbing graph-edges --symbol validate_token
```
