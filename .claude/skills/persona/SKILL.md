# /persona — Activate a project persona

Activate one of the project personas from `agent-personas/` for this session.
Reads the persona's profile, processes their inbox, and runs session startup
per the protocol defined in `agent-comms/PROTOCOL.md`.

## Usage

```
/persona <name>
```

## Personas

See `agent-comms/PROTOCOL.md` § Personas for the full table. Quick reference:

| Input | Persona |
|-------|---------|
| `architect` | Architect |
| `implementer`, `impl` | Implementer |
| `pm` | Product Manager |
| `test`, `te` | Test Engineer |
| `qa` | QA Reviewer |
| `docs` | Docs Writer |

## Steps when invoked

1. **Read the persona file** at `agent-personas/<name>.md` in full.
   Internalize the role, model, behaviour rules, limitations, and session
   protocol.

2. **Process the inbox and run session startup** as defined in
   `agent-comms/PROTOCOL.md` § Inbox processing. This covers: read + flag
   messages, spelunk check, list decisions/handoffs/questions, clear
   consumed entries.

3. **Announce readiness.** State:
   - Active persona name and preferred model
   - Inbox message count and any blocking/question items requiring action
   - What the persona is ready to work on

4. **Adopt the persona for the rest of the session.** Follow that persona's
   behaviour rules, scope, and communication patterns until the user starts
   a new session or invokes `/persona` again.

## Sending messages

Follow the schema and routing in `agent-comms/PROTOCOL.md` § Routing.
Append to `agent-comms/inbox/<recipient>.ndjson`.

Use `"kind":"question"` when blocked and needing a reply. Use
`"kind":"handoff"` for session handovers. Use `"kind":"amendment"` to
correct a prior message. Send to `"founder"` for human-in-the-loop
decisions.
