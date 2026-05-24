# MCP Server Integration (Future)

MCP (Model Context Protocol) server integration is planned for a future release.

Currently, `spelunk` is a CLI tool by design — it pushes only the output of the commands you actually run, keeping agent context lean. Full documentation for MCP integration will be added when the feature ships.

## What we're planning

- `spelunk-server` will support serving as an MCP tool host
- This enables tighter integration with Claude Code and other Claude-based agents
- The CLI-first design will remain — MCP is an additional integration path, not a replacement

## For now

Use `spelunk` directly from the CLI, or integrate it into your agent via SKILL.md and JSON output (`AGENT=true`).

See [agent-guide.md](agent-guide.md) for agent integration patterns.
