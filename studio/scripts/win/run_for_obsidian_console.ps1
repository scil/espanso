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

$log = Join-Path $env:LOCALAPPDATA 'popup-ime\events.log'
switch ($Button) {
    'ime-admin' { & "$PSScriptRoot\popup-ime.ps1" -Action start; Done }  # restart popup-ime as administrator
    'ime-stop' { & "$PSScriptRoot\popup-ime.ps1" -Action stop; Done }  # stop popup-ime
    # cmd joins cargo's stderr, so PowerShell does not show its progress lines as errors.
    'ime-build' { cmd /c "cargo build --release -p popup-gpui --bin popup-ime 2>&1" | Select-Object -Last 3; Done }  # build popup-ime
    'ime-log' { if (Test-Path $log) { Get-Content $log -Tail 30 } else { "No log at $log" }; Done }  # last 30 log lines
    default { Write-Output "Unknown console button: $Button"; exit 2 }
}
