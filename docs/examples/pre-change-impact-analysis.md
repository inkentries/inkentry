# Example: Pre-change impact analysis

Before making a significant change, use `spelunk` to understand the blast radius.

## Scenario

You need to change the signature of a core function — say, adding a required parameter to `validate_token()`.

## Step 1: Find everything that calls it

```bash
spelunk graph validate_token --kind calls
```

```
Incoming to 'validate_token':
  calls  auth/middleware.go  (handler:45)
  calls  api/routes.go       (apply_auth:112)
  calls  grpc/interceptor.go (unary_auth:67)
```

## Step 2: Understand each call site

```bash
# Full-text — no server needed
spelunk search "validate_token" --mode text --limit 20

# Semantic — requires server + index
# spelunk search "validate_token call site usage" --graph --limit 20
```

## Step 3: Check memory for prior context

```bash
spelunk memory search "validate_token authentication"
```

With a server that has an LLM backend, you can also ask for a synthesis:

```bash
spelunk explore "If I add a required 'scope' parameter to validate_token, what would I need to update across the codebase?"
```

## Step 4: Find the tests

```bash
spelunk search "validate_token test" --mode text
```

## Step 5: Check for related documentation

```bash
spelunk search "validate_token" --mode text
spelunk memory search "validate_token authentication"
```

## Step 6: Write a plan

Create a checklist as a plain markdown file in `docs/plans/`. It should cover:

- `- [ ] Update validate_token signature in src/auth/token.rs`
- `- [ ] Update call sites in middleware, routes, and interceptor`
- `- [ ] Update test fixtures and mocks`

## Step 7: After the change, verify

```bash
# Confirm all call sites are updated
spelunk graph validate_token --kind calls

# If the project is indexed, re-index changed files
spelunk index .
```
