<#
.SYNOPSIS
    Builds the sld_mcp SolidWorks add-in (C++, MSBuild) and/or the MCP server (Rust, cargo).

.EXAMPLE
    .\build.ps1                                  # both, Debug
    .\build.ps1 -Target server                   # Rust only (fine while SolidWorks is open)
    .\build.ps1 -Target addin -Configuration Release
    .\build.ps1 -Clean                           # clean both
    .\build.ps1 -Target register                 # regsvr32 the Debug DLL (prompts for admin)
#>
[CmdletBinding()]
param(
    [ValidateSet('all', 'addin', 'server', 'register')]
    [string]$Target = 'all',

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',

    [switch]$Clean
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$solution = Join-Path $root 'sld_mcp\sld_mcp.slnx'
$dll = Join-Path $root "sld_mcp\x64\$Configuration\sld_mcp.dll"
$serverDir = Join-Path $root 'mcp_server'
$exe = Join-Path $serverDir "target\$($Configuration.ToLower())\mcp_server.exe"

function Write-Step([string]$message) {
    Write-Host ""
    Write-Host "==> $message" -ForegroundColor Cyan
}

# A running process keeps its DLL/EXE locked, and the link step then fails with
# a cryptic LNK1104 / "Access is denied". Fail early with a clear message instead.
function Assert-NotRunning([string]$processName, [string]$hint) {
    $running = Get-Process -Name $processName -ErrorAction SilentlyContinue
    if ($running) {
        throw "$processName is running (PID $($running.Id -join ', ')) and locks the output. $hint"
    }
}

function Find-MSBuild {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) {
        throw "vswhere.exe not found -- is Visual Studio installed?"
    }
    # The vcxproj uses the v145 toolset, i.e. Visual Studio 2026 (version 18).
    foreach ($prerelease in $false, $true) {
        $vswhereArgs = @('-latest', '-products', '*', '-version', '[18.0,)',
                         '-requires', 'Microsoft.Component.MSBuild', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
                         '-find', 'MSBuild\**\Bin\amd64\MSBuild.exe')
        if ($prerelease) { $vswhereArgs += '-prerelease' }
        $found = & $vswhere @vswhereArgs | Select-Object -First 1
        if ($found) { return $found }
    }
    throw "No Visual Studio 2026 install with MSBuild and the C++ x64 tools was found."
}

function Build-Addin {
    if (-not $Clean) {
        Assert-NotRunning 'SLDWORKS' 'Close SolidWorks before rebuilding the add-in.'
    }
    $msbuild = Find-MSBuild
    $msbuildTarget = if ($Clean) { 'Clean' } else { 'Build' }
    Write-Step "Add-in: $msbuildTarget $Configuration|x64"
    & $msbuild $solution "-t:$msbuildTarget" "-p:Configuration=$Configuration" '-p:Platform=x64' `
        '-p:GenerateFullPaths=true' '-m' '-nologo' '-v:minimal'
    if ($LASTEXITCODE -ne 0) { throw "Add-in build failed (MSBuild exit code $LASTEXITCODE)." }
    if (-not $Clean) { Write-Host "    $dll" -ForegroundColor Green }
}

function Build-Server {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found on PATH -- install Rust from https://rustup.rs"
    }
    if (-not $Clean) {
        Assert-NotRunning 'mcp_server' 'Quit the MCP client using it (Claude Desktop, MCP Inspector, ...) and build again.'
    }
    # @( ) keeps this an array: a bare if-expression unrolls a one-element array
    # into a plain string, and splatting a string passes it one character at a time.
    $cargoArgs = @(if ($Clean) { 'clean' } else { 'build' })
    if ($Configuration -eq 'Release') { $cargoArgs += '--release' }
    Write-Step "MCP server: cargo $($cargoArgs -join ' ')"
    Push-Location $serverDir
    try {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "MCP server build failed (cargo exit code $LASTEXITCODE)." }
    }
    finally {
        Pop-Location
    }
    if (-not $Clean) { Write-Host "    $exe" -ForegroundColor Green }
}

function Register-Addin {
    if (-not (Test-Path $dll)) { throw "$dll not found -- build the add-in first." }
    Write-Step "Registering $dll (needs admin; a UAC prompt will appear)"
    $process = Start-Process regsvr32.exe -ArgumentList "`"$dll`"" -Verb RunAs -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "regsvr32 failed with exit code $($process.ExitCode)." }
}

$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
switch ($Target) {
    'all'      { Build-Addin; Build-Server }
    'addin'    { Build-Addin }
    'server'   { Build-Server }
    'register' { Register-Addin }
}
Write-Host ""
Write-Host ("Done in {0:N1} s" -f $stopwatch.Elapsed.TotalSeconds) -ForegroundColor Green
