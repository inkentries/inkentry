<#
.SYNOPSIS
  Install a managed, shared spelunk-server as a Windows Service (NSSM-hosted).

.DESCRIPTION
  Windows counterpart to macos/cloud.spelunk.server.mobileconfig. Registers a
  long-lived spelunk-server on an always-on host (a build host or team server)
  so a Windows fleet can share memory.

  spelunk-server.exe is a plain console program: it does NOT implement the
  Windows Service Control Manager (SCM) dispatcher, so `sc.exe create` /
  `New-Service` pointed straight at it will register but fail to start with
  error 1053 ("did not respond in a timely fashion"). A wrapper that hosts the
  console process as a service is required. This script uses NSSM
  (https://nssm.cc). Alternatives that also work: WinSW (XML-configured
  service shim) or a Task Scheduler task set to run at startup, whether or not a
  user is logged on, with restart-on-failure. Pick one; don't stack them.

  This binds loopback (127.0.0.1) only. spelunk-server refuses a non-loopback
  plaintext bind unconditionally, so this service is not reachable off-host by
  itself. Exposing it to other machines is a separate deployment decision, out
  of scope for this script. See ../../../docs/adr/056-oss-server-tenancy-model.md
  for the trust model once you have a reachability plan.

.NOTES
  Run as Administrator. Verify NSSM argument names against your NSSM version;
  they are stable but not guaranteed.
#>
[CmdletBinding()]
param(
    # spelunk-server.exe location. The install script places it in
    # %LOCALAPPDATA%\Programs\spelunk for the running user; for a machine
    # service, copy it somewhere machine-wide first (e.g. %ProgramData%).
    [string]$BinaryPath = "$env:ProgramData\spelunk\spelunk-server.exe",

    # nssm.exe location (install via Scoop/Chocolatey or download from nssm.cc).
    [string]$NssmPath = "nssm.exe",

    [string]$ServiceName = "spelunk-server",

    # Loopback only; see .DESCRIPTION. Do not set 0.0.0.0 here.
    [int]$Port = 7777,

    # Persistent DB + logs for an always-on host.
    [string]$DataDir = "$env:ProgramData\spelunk",

    # Shared API key. REQUIRED for any server other teammates reach. Prefer
    # passing it out-of-band over hardcoding it here.
    [Parameter(Mandatory = $true)]
    [string]$ServerKey
)

$ErrorActionPreference = "Stop"

$dbPath  = Join-Path $DataDir "spelunk.db"
$logDir  = Join-Path $DataDir "logs"
New-Item -ItemType Directory -Force -Path $DataDir, $logDir | Out-Null

if (-not (Test-Path $BinaryPath)) {
    throw "spelunk-server.exe not found at '$BinaryPath'. Deploy the binary first (see README.md)."
}

# Register the service. --host is omitted so the server keeps its loopback
# default. --key-file is preferred over --key so the key is not visible in the
# service's argument list; write it 0600-equivalent (Administrators/SYSTEM only).
$keyFile = Join-Path $DataDir "server-key"
Set-Content -Path $keyFile -Value $ServerKey -NoNewline
icacls $keyFile /inheritance:r /grant:r "SYSTEM:(R)" "Administrators:(R)" | Out-Null

$serverArgs = "--port $Port --db `"$dbPath`" --key-file `"$keyFile`""

& $NssmPath install $ServiceName $BinaryPath
& $NssmPath set $ServiceName AppParameters $serverArgs
& $NssmPath set $ServiceName AppDirectory $DataDir
& $NssmPath set $ServiceName AppStdout (Join-Path $logDir "spelunk-server.log")
& $NssmPath set $ServiceName AppStderr (Join-Path $logDir "spelunk-server.err.log")
& $NssmPath set $ServiceName Start SERVICE_AUTO_START
# RUST_LOG controls server verbosity. Add SPELUNK_* policy vars here if you
# prefer the service environment over machine environment variables.
& $NssmPath set $ServiceName AppEnvironmentExtra "RUST_LOG=info"

& $NssmPath start $ServiceName

Write-Host "Installed and started service '$ServiceName' on 127.0.0.1:$Port."
Write-Host "Verify: & '$BinaryPath' --health-check --port $Port"
