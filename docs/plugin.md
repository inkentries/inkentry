# Installing the inkentry agent skill

The skill ships from [inkentries/agent-plugin](https://github.com/inkentries/agent-plugin),
packaged to the [Agent Plugins](https://agent-plugins.org/) standard.

It lives in its own repository so that guidance can be corrected without
waiting for a CLI release. The two version independently, and that repository's
CI installs the released CLI and fails if the skill names a command the binary
does not have.

## Claude Code

Claude Code reads its own manifest rather than the portable one, so it is
supported alongside the standard rather than through it:

```
/plugin marketplace add inkentries/agent-plugin
/plugin install inkentry@inkentry
```

## Other agents

Agent Plugins 1.0.0 defines the package, not the delivery: distribution and
installation are left to each client. The artifact is that repository, and a
client implementing the standard consumes it the way that client does.

Failing that, [the skill](https://github.com/inkentries/agent-plugin/blob/main/skills/inkentry/SKILL.md)
is plain Markdown written for an agent operator, and works as context for any
agent that can run a shell.

## The CLI is a separate install

The plugin carries guidance, not the binary:

```bash
curl -fsSL https://get.inkentry.com/install.sh | sh
```

```powershell
irm https://get.inkentry.com/install.ps1 | iex
```

The skill checks for this and tells the agent to say so, rather than failing at
a shell call.

## Related, and not an install path

[`docs/examples/AGENTS.md`](examples/AGENTS.md) is a template for the
[`AGENTS.md`](https://agents.md) convention: a file you copy into your own
project, commit, and adapt, so agents working on *that* repository know it uses
inkentry. It is a project convention, not a way to obtain the skill.

[`docs/agent-guide.md`](agent-guide.md) is the long form: session start,
handoff, multi-agent coordination.
