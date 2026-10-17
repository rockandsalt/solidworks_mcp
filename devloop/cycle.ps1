<#
.SYNOPSIS
    One dev-loop iteration: build, (re)start SolidWorks with the add-in,
    run scenarios against the real thing.

.EXAMPLE
    .\devloop\cycle.ps1                                  # build both, run smoke
    .\devloop\cycle.ps1 -Build server -Scenario smoke    # Rust only: SolidWorks stays up
    .\devloop\cycle.ps1 -Build none -Scenario smoke,regressions,devloop\scenarios\other.py
    .\devloop\cycle.ps1 -Build addin -Force              # also discard unsaved changes to saved files

.NOTES
    Exit code 0 only if the builds succeed and every scenario passes. Output
    of each run lands in devloop\out\<time>-<scenarios>\. See
    docs/notes/devloop.md.
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    # What to rebuild. addin/all exit SolidWorks first: it locks sld_mcp.dll.
    [ValidateSet('all', 'addin', 'server', 'none')]
    [string]$Build = 'all',

    # Scenario names (devloop\scenarios\<name>.py) or paths, also comma-separated in one
    # string (what powershell -File passes). '' or none: just build and start.
    [string[]]$Scenario = @('smoke'),

    # Passed to sw.ps1 stop.
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$sw = Join-Path $PSScriptRoot 'sw.ps1'

function Invoke-Checked([string]$what, [scriptblock]$command) {
    & $command
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)." }
}

$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
if ($Build -in 'all', 'addin') {
    $stopArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $sw, 'stop')
    if ($Force) { $stopArgs += '-Force' }
    Invoke-Checked 'sw.ps1 stop' { & powershell.exe @stopArgs }
}
if ($Build -ne 'none') {
    Invoke-Checked 'build.ps1' { & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'build.ps1') -Target $Build }
}
Invoke-Checked 'sw.ps1 start' { & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $sw start }

$paths = @($Scenario | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ } | ForEach-Object {
    $candidate = if (Test-Path -LiteralPath $_) { $_ } else { Join-Path $PSScriptRoot "scenarios\$_.py" }
    if (-not (Test-Path -LiteralPath $candidate)) { throw "Scenario '$_' not found (looked for $candidate)." }
    (Resolve-Path -LiteralPath $candidate).Path
})
if ($paths.Count) {
    Invoke-Checked 'scenarios' { & py -3 (Join-Path $PSScriptRoot 'mcp_client.py') run @paths }
}
Write-Host ("`nCycle done in {0:N0} s" -f $stopwatch.Elapsed.TotalSeconds) -ForegroundColor Green
