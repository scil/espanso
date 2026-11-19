# Quits snip, builds the release binary and starts snip again with the
# command line it had. If snip ran as administrator, it stops and starts
# through snip.ps1 (one UAC prompt each). If snip did not run, it starts
# with the command line of the sign-in entry, or with the defaults.
# The new process is created through WMI, so it gets the normal user
# environment and not the environment of the calling shell.
$ErrorActionPreference = 'Stop'
$studio = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$exe = Join-Path $studio 'target\release\snip.exe'
$logDir = Join-Path $env:LOCALAPPDATA 'snip'
$log = Join-Path $logDir 'events.log'

$running = @(Get-CimInstance Win32_Process -Filter "Name='snip.exe'")
$line = ($running | Select-Object -First 1).CommandLine
if (-not $line) {
    $line = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name snip -ErrorAction SilentlyContinue).snip
}
if (-not $line) { $line = "`"$exe`" --timing --log `"$log`"" }

$elevated = $false
foreach ($p in $running) {
    try {
        Stop-Process -Id $p.ProcessId -Force
    } catch {
        $elevated = $true
    }
}
if ($elevated) { & "$PSScriptRoot\snip.ps1" -Action stop | Out-Null }
for ($i = 0; $i -lt 50 -and (Get-Process snip -ErrorAction SilentlyContinue); $i++) { Start-Sleep -Milliseconds 100 }
Write-Output ("snip: stopped {0} process(es){1}" -f $running.Count, $(if ($elevated) { ', elevated' } else { '' }))

Push-Location $studio
cmd /c "cargo build --release -p popup-gpui --bin snip 2>&1" | Select-Object -Last 3
$built = $LASTEXITCODE
Pop-Location

# Start snip again also after a failed build, with the old binary, so the
# expander keeps running.
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
if (Test-Path $log) { Move-Item -Force $log (Join-Path $logDir 'events.prev.log') }
if ($elevated) {
    & "$PSScriptRoot\snip.ps1" -Action start
} else {
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $line; CurrentDirectory = $studio }
    if ($r.ReturnValue -ne 0) { Write-Output "snip: start failed, code $($r.ReturnValue)"; exit 1 }
    Write-Output "snip: started, pid $($r.ProcessId)"
}
if ($built -ne 0) { Write-Output 'snip: build failed, the old binary runs'; exit $built }
