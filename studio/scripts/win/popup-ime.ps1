# Starts or stops popup-ime as administrator. An elevated popup-ime also sees
# keys in elevated windows, such as Cmder run as administrator. A normal
# process cannot stop an elevated one, so both actions run elevated: the
# script asks for elevation once (UAC) and runs itself again.
param(
    [Parameter(Mandatory = $true)][ValidateSet('start', 'stop')][string]$Action
)
$ErrorActionPreference = 'Stop'

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    # Not -Wait: in PowerShell 7 it also waits for child processes, and
    # popup-ime keeps running.
    $p = Start-Process -FilePath 'pwsh' -Verb RunAs -WindowStyle Hidden -PassThru `
        -ArgumentList @('-NoProfile', '-File', "`"$PSCommandPath`"", '-Action', $Action)
    $p.WaitForExit()
    $running = Get-Process popup-ime -ErrorAction SilentlyContinue
    Write-Output ("popup-ime: {0}" -f $(if ($running) { "running, pid $($running.Id -join ', ')" } else { 'not running' }))
    exit 0
}

Get-Process popup-ime -ErrorAction SilentlyContinue | Stop-Process -Force
if ($Action -eq 'stop') { exit 0 }

$exe = Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'target\release\popup-ime.exe'
$logDir = Join-Path $env:LOCALAPPDATA 'popup-ime'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$log = Join-Path $logDir 'events.log'
if (Test-Path $log) { Move-Item -Force $log (Join-Path $logDir 'events.prev.log') }
Start-Process -FilePath $exe -ArgumentList @('--timing', '--log', "`"$log`"")
