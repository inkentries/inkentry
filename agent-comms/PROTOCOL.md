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
{"from": "implementer", "to": "architect", "re": "#42", "ts": "2026-04-16T10:00:00Z", "session_id": "sess_abc123", "body": "Spec is ambiguous: does knn emit one JSON object or one per line?", "priority": "blocking"}
```

Fields:
- `from` — sender persona name (architect | implementer | test-engineer | docs-writer | qa-reviewer)
- `to` — recipient persona name
- `re` — GitHub issue reference e.g. `"#42"` (required)
- `ts` — ISO-8601 timestamp
- `session_id` — opaque session identifier; set when an agent claims a task. Prevents parallel agents from picking up the same issue and allows querying stuck sessions.
- `body` — free text, keep under 500 chars
- `priority` — `"blocking"` | `"fyi"` | `"question"`

**Routing**: messages are placed directly into the recipient's inbox file.
Each agent reads their own inbox at session start.

**Inbox lifecycle**: agents MUST remove messages from their inbox as they
finish processing them (rewrite the file without the consumed lines). A
message with a `session_id` indicates an agent has claimed the referenced
issue — other agents MUST NOT pick up that issue while the message is
present. If a session appears stuck, the `session_id` can be used to
query its status.

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
    test-engineer.ndjson
    docs-writer.ndjson
    qa-reviewer.ndjson
```
