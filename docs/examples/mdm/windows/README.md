# Windows fleet deployment for inkentry

The Windows counterpart to the macOS example one directory up. It covers the two
Windows-specific pieces of an MDM rollout: pushing inkentry's config/environment
with Intune or Group Policy, and running a shared `inkentry-server` as a Windows
Service. Read [`../README.md`](../README.md) first: the configuration surface,
the two deployment shapes, and the trust model are the same on every platform;
only the mechanics below differ.

> Community examples. Test on one machine before a fleet rollout, and verify
> every Intune/GPO payload in your own MDM console: the exact policy CSP and
> ADMX shapes vary by console and are noted as "verify in console" below.

## Windows paths

inkentry reads the same TOML config and environment variables on Windows as
elsewhere; only the paths differ.

| What | Path |
|------|------|
| Global user config | `%USERPROFILE%\.config\inkentry\config.toml` |
| Project config (in-repo) | `<repo>\.inkentry\config.toml` |
| Stored credentials | Windows Credential Manager (default) |
| Binaries (install script) | `%LOCALAPPDATA%\Programs\inkentry\` |
| Shared-server data (this example) | `%ProgramData%\inkentry\` |

Note the config path: inkentry uses `~/.config` on **every** platform, so on
Windows that is `%USERPROFILE%\.config\inkentry\`, **not** `%APPDATA%` or
`%LOCALAPPDATA%`. Push the shared
[`../inkentry-config.toml`](../inkentry-config.toml) there; its keys are
identical on Windows.

## Shape A: managed laptops, no shared server

Each laptop autostarts a local, loopback-bound `inkentry-server` on demand.
There is nothing to provision. Two Windows-specific MDM tasks:

1. **Install the binaries.** Deploy `inkentry.exe` and `inkentry-server.exe` to
   a machine-wide directory on `PATH` (e.g. `%ProgramData%\inkentry\`), or run
   the PowerShell install script (see
   [`../../../docs/getting-started.md`](../../../docs/getting-started.md)) as a
   managed script. A winget package is planned as the primary managed path;
   until then, an Intune Win32 app or a script payload is the deployment
   vehicle.
2. **Pre-approve the loopback firewall rule.** The first time the local server
   binds its port, Windows Defender Firewall may prompt; if it is dismissed or
   silently blocked, inkentry drops to text/AST search with a "no server
   reachable" notice. Push an **inbound allow rule for `inkentry-server.exe`**
   so no user prompt is needed. With Group Policy:

   ```
   Computer Configuration > Policies > Windows Settings > Security Settings >
   Windows Defender Firewall with Advanced Security > Inbound Rules
   ```

   Create an inbound Program rule allowing
   `%ProgramData%\inkentry\inkentry-server.exe`. Via Intune, the equivalent is
   an Endpoint security **Firewall** rule policy (verify the rule fields in your
   console). Scope it as tightly as your policy allows: the server binds
   loopback, so a local rule is sufficient.

Optionally push fleet policy (e.g. `INKENTRY_NO_SERVER=1` on air-gapped
machines) via the environment mechanism below.

## Shape B: shared team memory server on a Windows host

Run one long-lived `inkentry-server` and point every laptop at it.

1. **Install the binaries** on the laptops (Shape A) and on the server host.
   Copy `inkentry-server.exe` to a machine-wide location such as
   `%ProgramData%\inkentry\`.
2. **Run the server as a Windows Service.** Use
   [`Install-InkentryServerService.ps1`](Install-InkentryServerService.ps1). It
   registers `inkentry-server.exe` under NSSM (a service wrapper), stores the DB
   and logs under `%ProgramData%\inkentry\`, and reads the API key from a
   locked-down key file. For a team-reachable server, pass `-BindHost 0.0.0.0`
   with your PEM certificate and private key so the server terminates HTTPS
   in-process (ADR-066), with nothing in front of it:

   ```powershell
   .\Install-InkentryServerService.ps1 `
     -ServerKey "replace-with-your-shared-api-key" `
     -BindHost 0.0.0.0 `
     -TlsCert "C:\ProgramData\inkentry\tls-cert.pem" `
     -TlsKey  "C:\ProgramData\inkentry\tls-key.pem"
   ```

   Bring your own certificate (an internal CA, `certbot`, or a cloud-issued
   cert); `inkentry-server` does not renew it, so the operator does. Omit the
   `-BindHost`/`-TlsCert`/`-TlsKey` args for an on-host-only server that keeps
   the loopback default.

   `inkentry-server.exe` is a plain console program and does not speak the
   Windows Service Control Manager protocol, so `sc.exe create` / `New-Service`
   pointed directly at it will fail to start (error 1053). A wrapper is
   required; the script uses NSSM, and WinSW or a startup Task Scheduler task
   work equally.
3. **A non-loopback bind needs both TLS and a key.** `inkentry-server` refuses a
   non-loopback plaintext bind with no override, so a routable `-BindHost`
   requires `-TlsCert`/`-TlsKey` and the API key (the script enforces this).
   Lock the private key file down to SYSTEM/Administrators only. Loopback with
   no TLS stays on-host only.
4. **Pre-configure the laptops.** Push
   [`../inkentry-config.toml`](../inkentry-config.toml) to each user's
   `%USERPROFILE%\.config\inkentry\config.toml` with `server_url` set to the
   server's `https://` endpoint, and deliver the shared `INKENTRY_SERVER_KEY`
   via the environment mechanism below.

## Pushing inkentry configuration on Windows

### The config file

Write your edited copy of `../inkentry-config.toml` to each user's
`%USERPROFILE%\.config\inkentry\config.toml`. Deploy it with an Intune
platform-script (PowerShell) payload or a GPO login/startup script that creates
`%USERPROFILE%\.config\inkentry\` and copies the file in per user. A team-wide
subset (`server_url`, `project_id`) can instead live in a committed
`.inkentry\config.toml` at the repo root, needing no MDM at all.

### The environment

Deliver the variables from [`../inkentry.env.example`](../inkentry.env.example)
as Windows environment variables. `INKENTRY_SERVER_KEY` is the one to
prioritise: it carries the shared credential, overrides Credential Manager, and
works on headless and freshly imaged machines with no interactive login.

- **Machine-wide, per host:** `setx /M INKENTRY_SERVER_KEY "..."` from a managed
  script (writes `HKLM\...\Environment`).
- **Group Policy:** *Computer/User Configuration > Preferences > Windows
  Settings > Environment*, one item per variable.
- **Intune:** a platform script that sets the machine/user environment, or an
  OMA-URI/settings-catalog environment payload (verify the exact setting in your
  console).

Do not set both `INKENTRY_SERVER_URL` and `INKENTRY_NO_SERVER=1`: the offline
kill-switch overrides everything and disables the server entirely.

## Verifying a rollout

On a managed machine after deployment (PowerShell):

```powershell
inkentry --version            # binaries are on PATH
inkentry status               # index + config health
inkentry context              # confirms the configured server is reachable
```

On the server host, probe the service without curl/wget:

```powershell
& "$env:ProgramData\inkentry\inkentry-server.exe" --health-check --port 7777
```

## What is in this directory

| File | Purpose |
|------|---------|
| `README.md` | This guide. |
| `Install-InkentryServerService.ps1` | Registers a loopback-bound `inkentry-server` as an NSSM-hosted Windows Service on a shared host. |

Shared, cross-platform config templates live one level up:
[`../inkentry-config.toml`](../inkentry-config.toml) and
[`../inkentry.env.example`](../inkentry.env.example).
