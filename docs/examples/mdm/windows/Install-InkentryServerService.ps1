<#
.SYNOPSIS
  Install a managed, shared inkentry-server as a Windows Service (NSSM-hosted).

.DESCRIPTION
  Windows counterpart to macos/com.inkentry.server.mobileconfig. Registers a
  long-lived inkentry-server on an always-on host (a build host or team server)
  so a Windows fleet can share memory.

  inkentry-server.exe is a plain console program: it does NOT implement the
  Windows Service Control Manager (SCM) dispatcher, so `sc.exe create` /
  `New-Service` pointed straight at it will register but fail to start with
  error 1053 ("did not respond in a timely fashion"). A wrapper that hosts the
  console process as a service is required. This script uses NSSM
  (https://nssm.cc). Alternatives that also work: WinSW (XML-configured
  service shim) or a Task Scheduler task set to run at startup, whether or not a
  user is logged on, with restart-on-failure. Pick one; don't stack them.

  For a team-reachable server, pass -BindHost 0.0.0.0 with -TlsCert and -TlsKey:
  inkentry-server terminates HTTPS in-process (ADR-066), so nothing sits in front
  of it. A non-loopback bind is refused unless BOTH the TLS cert/key AND an API
  key are set. Bring your own PEM certificate (internal CA / certbot / cloud);
  the server does not renew it. With no -TlsCert/-TlsKey the service stays on the
  loopback (127.0.0.1) default, reachable on-host only. See
  ../../../docs/adr/056-oss-server-tenancy-model.md for the trust model.

.NOTES
  Run as Administrator. Verify NSSM argument names against your NSSM version;
  they are stable but not guaranteed.
#>
[CmdletBinding()]
param(
    # inkentry-server.exe location. The install script places it in
    # %LOCALAPPDATA%\Programs\inkentry for the running user; for a machine
    # service, copy it somewhere machine-wide first (e.g. %ProgramData%).
    [string]$BinaryPath = "$env:ProgramData\inkentry\inkentry-server.exe",

    # nssm.exe location (install via Scoop/Chocolatey or download from nssm.cc).
    [string]$NssmPath = "nssm.exe",

    [string]$ServiceName = "inkentry-server",

    # Interface to bind. Loopback (default) is on-host only. For a
    # team-reachable server pass 0.0.0.0 together with -TlsCert and -TlsKey.
    [string]$BindHost = "127.0.0.1",

    [int]$Port = 7777,

    # Operator-provided PEM cert chain (public) and private key. Both or neither.
    # Required for a routable (-BindHost 0.0.0.0) bind; leave empty for loopback.
    # Bring your own (internal CA / certbot / cloud); the server does not renew.
    [string]$TlsCert = "",
    [string]$TlsKey  = "",

    # Persistent DB + logs for an always-on host.
    [string]$DataDir = "$env:ProgramData\inkentry",

    # Shared API key. REQUIRED for any server other teammates reach. Prefer
    # passing it out-of-band over hardcoding it here.
    [Parameter(Mandatory = $true)]
    [string]$ServerKey
)

$ErrorActionPreference = "Stop"

$dbPath  = Join-Path $DataDir "inkentry.db"
$logDir  = Join-Path $DataDir "logs"
New-Item -ItemType Directory -Force -Path $DataDir, $logDir | Out-Null

if (-not (Test-Path $BinaryPath)) {
    throw "inkentry-server.exe not found at '$BinaryPath'. Deploy the binary first (see README.md)."
}

# TLS args are all-or-nothing, and a routable bind requires them.
if ([bool]$TlsCert -ne [bool]$TlsKey) {
    throw "Set both -TlsCert and -TlsKey, or neither."
}
$tlsEnabled = [bool]$TlsCert
if ($BindHost -ne "127.0.0.1" -and $BindHost -ne "::1" -and $BindHost -ne "localhost" -and -not $tlsEnabled) {
    throw "A non-loopback -BindHost requires -TlsCert and -TlsKey (inkentry-server refuses a plaintext off-host bind)."
}

# --key-file is preferred over --key so the key is not visible in the service's
# argument list; write it 0600-equivalent (Administrators/SYSTEM only).
$keyFile = Join-Path $DataDir "server-key"
Set-Content -Path $keyFile -Value $ServerKey -NoNewline
icacls $keyFile /inheritance:r /grant:r "SYSTEM:(R)" "Administrators:(R)" | Out-Null

$serverArgs = "--host $BindHost --port $Port --db `"$dbPath`" --key-file `"$keyFile`""
if ($tlsEnabled) {
    # The private key must be a locked-down file (SYSTEM/Administrators only).
    $serverArgs += " --tls-cert `"$TlsCert`" --tls-key `"$TlsKey`""
}

& $NssmPath install $ServiceName $BinaryPath
& $NssmPath set $ServiceName AppParameters $serverArgs
& $NssmPath set $ServiceName AppDirectory $DataDir
& $NssmPath set $ServiceName AppStdout (Join-Path $logDir "inkentry-server.log")
& $NssmPath set $ServiceName AppStderr (Join-Path $logDir "inkentry-server.err.log")
& $NssmPath set $ServiceName Start SERVICE_AUTO_START
# RUST_LOG controls server verbosity. Add INKENTRY_* policy vars here if you
# prefer the service environment over machine environment variables.
& $NssmPath set $ServiceName AppEnvironmentExtra "RUST_LOG=info"

& $NssmPath start $ServiceName

$scheme = if ($tlsEnabled) { "https" } else { "http" }
Write-Host "Installed and started service '$ServiceName' on ${scheme}://${BindHost}:$Port."
Write-Host "Verify: & '$BinaryPath' --health-check --port $Port"
