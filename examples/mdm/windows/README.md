# Windows fleet deployment for spelunk

The Windows counterpart to the macOS example one directory up. It covers the two
Windows-specific pieces of an MDM rollout: pushing spelunk's config/environment
with Intune or Group Policy, and running a shared `spelunk-server` as a Windows
Service. Read [`../README.md`](../README.md) first — the configuration surface,
the two deployment shapes, and the trust model are the same on every platform;
only the mechanics below differ.

> Community examples. Test on one machine before a fleet rollout, and verify
> every Intune/GPO payload in your own MDM console — the exact policy CSP and
> ADMX shapes vary by console and are noted as "verify in console" below.

## Windows paths

spelunk reads the same TOML config and environment variables on Windows as
elsewhere; only the paths differ.

| What | Path |
|------|------|
| Global user config | `%USERPROFILE%\.config\spelunk\config.toml` |
| Project config (in-repo) | `<repo>\.spelunk\config.toml` |
| Stored credentials | Windows Credential Manager (default) |
| Binaries (install script) | `%LOCALAPPDATA%\Programs\spelunk\` |
| Shared-server data (this example) | `%ProgramData%\spelunk\` |

Note the config path: spelunk uses `~/.config` on **every** platform, so on
Windows that is `%USERPROFILE%\.config\spelunk\`, **not** `%APPDATA%` or
`%LOCALAPPDATA%`. Push the shared [`../spelunk-config.toml`](../spelunk-config.toml)
there; its keys are identical on Windows.

## Shape A: managed laptops, no shared server

Each laptop autostarts a local, loopback-bound `spelunk-server` on demand. There
is nothing to provision. Two Windows-specific MDM tasks:

1. **Install the binaries.** Deploy `spelunk.exe` and `spelunk-server.exe` to a
   machine-wide directory on `PATH` (e.g. `%ProgramData%\spelunk\`), or run the
   PowerShell install script (see [`../../../docs/getting-started.md`](../../../docs/getting-started.md))
   as a managed script. A winget package is planned as the primary managed path;
   until then, an Intune Win32 app or a script payload is the deployment vehicle.
2. **Pre-approve the loopback firewall rule.** The first time the local server
   binds its port, Windows Defender Firewall may prompt; if it is dismissed or
   silently blocked, spelunk drops to text/AST search with a "no server
   reachable" notice. Push an **inbound allow rule for `spelunk-server.exe`** so
   no user prompt is needed. With Group Policy:

   ```
   Computer Configuration > Policies > Windows Settings > Security Settings >
   Windows Defender Firewall with Advanced Security > Inbound Rules
   ```

   Create an inbound Program rule allowing `%ProgramData%\spelunk\spelunk-server.exe`.
   Via Intune, the equivalent is an Endpoint security **Firewall** rule policy
   (verify the rule fields in your console). Scope it as tightly as your policy
   allows — the server binds loopback, so a local rule is sufficient.

Optionally push fleet policy (e.g. `SPELUNK_NO_SERVER=1` on air-gapped
machines) via the environment mechanism below.

## Shape B: shared team memory server on a Windows host

Run one long-lived `spelunk-server` and point every laptop at it.

1. **Install the binaries** on the laptops (Shape A) and on the server host.
   Copy `spelunk-server.exe` to a machine-wide location such as
   `%ProgramData%\spelunk\`.
2. **Run the server as a Windows Service.** Use
   [`Install-SpelunkServerService.ps1`](Install-SpelunkServerService.ps1). It
   registers `spelunk-server.exe` under NSSM (a service wrapper), binds loopback,
   stores the DB and logs under `%ProgramData%\spelunk\`, and reads the API key
   from a locked-down key file.

   ```powershell
   .\Install-SpelunkServerService.ps1 -ServerKey "replace-with-your-shared-api-key"
   ```

   `spelunk-server.exe` is a plain console program and does not speak the Windows
   Service Control Manager protocol, so `sc.exe create` / `New-Service` pointed
   directly at it will fail to start (error 1053). A wrapper is required; the
   script uses NSSM, and WinSW or a startup Task Scheduler task work equally.
3. **This installs a loopback-only server.** `spelunk-server` refuses a
   non-loopback plaintext bind with no override, so the service from step 2 is
   not reachable off-host by itself. Exposing it to other machines is a
   separate deployment decision, out of scope for this script and README.
4. **Pre-configure the laptops.** Push [`../spelunk-config.toml`](../spelunk-config.toml)
   to each user's `%USERPROFILE%\.config\spelunk\config.toml` with `server_url`
   set to wherever your own deployment makes the server reachable, and deliver
   the shared `SPELUNK_SERVER_KEY` via the environment mechanism below.

## Pushing spelunk configuration on Windows

### The config file

Write your edited copy of `../spelunk-config.toml` to each user's
`%USERPROFILE%\.config\spelunk\config.toml`. Deploy it with an Intune
platform-script (PowerShell) payload or a GPO login/startup script that creates
`%USERPROFILE%\.config\spelunk\` and copies the file in per user. A team-wide
subset (`server_url`, `project_id`) can instead live in a committed
`.spelunk\config.toml` at the repo root, needing no MDM at all.

### The environment

Deliver the variables from [`../spelunk.env.example`](../spelunk.env.example) as
Windows environment variables. `SPELUNK_SERVER_KEY` is the one to prioritise: it
carries the shared credential, overrides Credential Manager, and works on
headless and freshly imaged machines with no interactive login.

- **Machine-wide, per host:** `setx /M SPELUNK_SERVER_KEY "..."` from a managed
  script (writes `HKLM\...\Environment`).
- **Group Policy:** *Computer/User Configuration > Preferences > Windows
  Settings > Environment*, one item per variable.
- **Intune:** a platform script that sets the machine/user environment, or an
  OMA-URI/settings-catalog environment payload (verify the exact setting in your
  console).

Do not set both `SPELUNK_SERVER_URL` and `SPELUNK_NO_SERVER=1` — the offline
kill-switch overrides everything and disables the server entirely.

## Verifying a rollout

On a managed machine after deployment (PowerShell):

```powershell
spelunk --version            # binaries are on PATH
spelunk status               # index + config health
spelunk context              # confirms the configured server is reachable
```

On the server host, probe the service without curl/wget:

```powershell
& "$env:ProgramData\spelunk\spelunk-server.exe" --health-check --port 7777
```

## What is in this directory

| File | Purpose |
|------|---------|
| `README.md` | This guide. |
| `Install-SpelunkServerService.ps1` | Registers a loopback-bound `spelunk-server` as an NSSM-hosted Windows Service on a shared host. |

Shared, cross-platform config templates live one level up:
[`../spelunk-config.toml`](../spelunk-config.toml) and
[`../spelunk.env.example`](../spelunk.env.example).
