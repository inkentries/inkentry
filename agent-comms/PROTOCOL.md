# Agent Communication Protocol

Agents communicate via NDJSON files in this directory and via GitHub Issues + spelunk memory.

## Channels

### 1. GitHub Issues (primary — task tracking)
- All work is tracked as GitHub Issues in `usercise/spelunk`
- Every agent that starts a task comments on the issue: "Starting work on #N"
- Every agent that completes a task comments: "Done — PR #M / ready for QA"
- Labels signal which persona should pick up the work (see label descriptions)

### 2. NDJSON Message Files (secondary — async agent-to-agent)
Files live in `agent-comms/inbox/<recipient>.ndjson`.

A message is one JSON object per line:
```json
{"from": "implementer", "to": "architect", "re": "#42", "ts": "2026-04-16T10:00:00Z", "session_id": "sess_abc123", "kind": "question", "body": "Spec is ambiguous: does knn emit one JSON object or one per line?", "priority": "blocking"}
```

Fields:
- `from` — sender persona name (see persona table below)
- `to` — recipient persona name (or `founder` for human-in-the-loop)
- `re` — GitHub issue reference e.g. `"#42"` (required)
- `ts` — ISO-8601 timestamp
- `session_id` — opaque session identifier; set when an agent claims a task. Prevents parallel agents from picking up the same issue and allows querying stuck sessions.
- `kind` — message type: `"message"` (general), `"question"` (sender is blocked and needs a reply before continuing), `"handoff"` (session handover), or `"amendment"` (correction to a prior message)
- `body` — free text, keep under 500 chars
- `priority` — `"blocking"` | `"fyi"`

#### Personas

| Alias | Full name | Inbox file |
|-------|-----------|------------|
| `architect` | Architect | `agent-comms/inbox/architect.ndjson` |
| `implementer`, `impl` | Implementer | `agent-comms/inbox/implementer.ndjson` |
| `pm` | Product Manager | `agent-comms/inbox/product-manager.ndjson` |
| `test`, `te` | Test Engineer | `agent-comms/inbox/test-engineer.ndjson` |
| `qa` | QA Reviewer | `agent-comms/inbox/qa-reviewer.ndjson` |
| `docs` | Docs Writer | `agent-comms/inbox/docs-writer.ndjson` |
| `founder` | Founder (human-in-the-loop) | `agent-comms/inbox/founder.ndjson` |

`founder` is not a persona — it's the back-pressure point for human
feedback and questions. Messages to `founder` are for decisions only
the human can make, blocking issues that need unblocking, and
context the human should be aware of.

#### Routing

To send a message, append a JSON line to `agent-comms/inbox/<recipient>.ndjson`:
```json
{"from":"<sender>","to":"<recipient>","re":"#<N>","ts":"<ISO-8601>","session_id":"<id>","kind":"<message|question|handoff|amendment>","priority":"<blocking|fyi>","body":"..."}
```

Each agent reads their own inbox at session start.

#### Inbox processing (session startup)

Every agent session MUST:

1. **Read and process the inbox.** For each message:
   - Print: sender, date, priority, kind, summary (first 80 chars of body)
   - Flag `"priority":"blocking"` with **[BLOCKING]**
   - Flag `"kind":"question"` with **[NEEDS REPLY]**

2. **Run spelunk startup checks.** Dog-food the product:
   ```bash
   spelunk check                                    # verify index is fresh
   spelunk memory list --kind decision --limit 10   # review prior decisions
   spelunk memory list --kind handoff --limit 3     # pick up where last session left off
   spelunk memory list --kind question              # check open questions
   ```

3. **Clear consumed messages.** After processing, remove consumed entries
   from the inbox file (rewrite without consumed lines, or truncate).
   A message with a `session_id` indicates an active claim — other agents
   MUST NOT pick up that issue while the message is present. If a session
   appears stuck, the `session_id` can be used to query its status.

### 3. spelunk memory (tertiary — decisions and context)
```bash
spelunk memory add --kind decision --title "..." --body "why, what, what breaks"
spelunk memory add --kind note --title "..." --body "..."
spelunk memory add --kind handoff --title "Handoff: <summary>" --body "..."
```
All agents add handoff notes at the end of each session.

## Plumbing Command Output Protocol

All plumbing commands MUST:
- Emit **NDJSON** to stdout (one JSON object per line)
- Write human-readable errors to stderr only
- Exit `0` on success
- Exit `1` if the query returned zero results (not an error)
- Exit `2` on any error (bad args, DB failure, embedding failure)

Example plumbing output:
```ndjson
{"kind":"chunk","file":"src/main.rs","start_line":1,"end_line":42,"content":"...","chunk_id":"abc123"}
{"kind":"chunk","file":"src/lib.rs","start_line":5,"end_line":30,"content":"...","chunk_id":"def456"}
```

## Exit Code Convention
| Code | Meaning |
|------|---------|
| 0 | Success — results emitted |
| 1 | No results — query valid but nothing matched |
| 2 | Error — invalid args, missing DB, backend failure |

## File Routing Convention
```
agent-comms/
  PROTOCOL.md          — this file
  inbox/
    architect.ndjson   — messages waiting for Architect
    implementer.ndjson
    product-manager.ndjson
    test-engineer.ndjson
    qa-reviewer.ndjson
    docs-writer.ndjson
    founder.ndjson     — human-in-the-loop back-pressure
```
