# Console entry point for the studio (obsidian-console and logseq-ops skills):
# every console button runs this one file with a key, and each key runs one
# fixed command. An unknown key runs nothing.
param(
    [Parameter(Mandatory = $true)][string]$Button
)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))

function Done([int[]]$Ok = @()) {
    $code = if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE }
    if ($Ok -contains $code) { $code = 0 }
    exit $code
}

$log = Join-Path $env:LOCALAPPDATA 'snip\events.log'
switch ($Button) {
    'snip-admin' { & "$PSScriptRoot\snip.ps1" -Action start; Done }  # restart snip as administrator
    'snip-stop' { & "$PSScriptRoot\snip.ps1" -Action stop; Done }  # stop snip
    # cmd joins cargo's stderr, so PowerShell does not show its progress lines as errors.
    'snip-build' { cmd /c "cargo build --release -p popup-gpui --bin snip 2>&1" | Select-Object -Last 3; Done }  # build snip
    'snip-rebuild' { & "$PSScriptRoot\snip-rebuild.ps1"; Done }  # quit snip, build release, start it again
    'snip-log' { if (Test-Path $log) { Get-Content $log -Tail 30 } else { "No log at $log" }; Done }  # last 30 log lines
    default { Write-Output "Unknown console button: $Button"; exit 2 }
}
