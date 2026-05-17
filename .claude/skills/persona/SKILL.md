# /persona — Activate a project persona

Activate one of the project personas from `agent-personas/` for this session. Reads the persona's profile, processes their inbox, and runs their session startup protocol.

## Usage

```
/persona <name>
```

## Name aliases (case-insensitive)

| Input | File | Inbox |
|---|---|---|
| `architect` | `agent-personas/architect.md` | `agent-comms/inbox/architect.ndjson` |
| `implementer`, `impl` | `agent-personas/implementer.md` | `agent-comms/inbox/implementer.ndjson` |
| `product-manager`, `pm` | `agent-personas/product-manager.md` | `agent-comms/inbox/product-manager.ndjson` |
| `test-engineer`, `test`, `te` | `agent-personas/test-engineer.md` | `agent-comms/inbox/test-engineer.ndjson` |
| `qa-reviewer`, `qa` | `agent-personas/qa-reviewer.md` | `agent-comms/inbox/qa-reviewer.ndjson` |
| `docs-writer`, `docs` | `agent-personas/docs-writer.md` | `agent-comms/inbox/docs-writer.ndjson` |

## Steps when invoked

1. **Read the persona file in full.** Internalize the role, model, behaviour rules, limitations, and session protocol.

2. **Process the inbox.** Read `agent-comms/inbox/<name>.ndjson` (one JSON object per line). For each message:
   - Print: sender, date, priority, subject
   - Summarize the body in 1–3 bullet points
   - Flag `"priority":"high"` messages with **[HIGH]**
   - If the inbox file does not exist, note that the inbox is empty.

3. **Run the session startup commands** from the persona's Session Protocol block. Report findings concisely (spelunk check output, recent decisions, open questions).

4. **Announce readiness.** State:
   - Active persona name and preferred model
   - Inbox message count and any high-priority items requiring action
   - What the persona is ready to work on

5. **Adopt the persona for the rest of the session.** Follow that persona's behaviour rules, scope, and communication patterns until the user starts a new session or invokes `/persona` again.

## Sending messages between personas

To send a message from the active persona to another persona, append a JSON line to `agent-comms/inbox/<target-persona>.ndjson`. Use this schema:

```json
{"id":"<NNN>","from":"<sender>","to":"<recipient>","date":"<YYYY-MM-DD>","kind":"<message|question|handoff|amendment>","priority":"<high|normal|low>","subject":"...","body":"..."}
```

Increment `id` from the last entry in that file. Use `kind: "question"` when the sender is blocked and needs a reply before continuing.
