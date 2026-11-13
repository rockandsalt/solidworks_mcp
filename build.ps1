<#
.SYNOPSIS
    Builds the sld_mcp SolidWorks add-in (C++, MSBuild) and/or the MCP server (Rust, cargo).

.EXAMPLE
    .\build.ps1                                  # both, Debug
    .\build.ps1 -Target server                   # Rust only (fine while SolidWorks is open)
    .\build.ps1 -Target addin -Configuration Release
    .\build.ps1 -Target test                     # native tests, without running SolidWorks
    .\build.ps1 -Target test -SwTypeLibDir 'C:\Program Files\SOLIDWORKS Corp\SOLIDWORKS (2)'
    .\build.ps1 -Clean                           # clean both
    .\build.ps1 -Target register                 # regsvr32 the Debug DLL (prompts for admin)
    .\build.ps1 -Target test-session             # open the MCP test session in its own window
    .\build.ps1 -Cycle                           # stop the test session, build both, resume it
    .\build.ps1 -Target server -Cycle            # same, MCP server only
    .\build.ps1 -Target installer                # Release builds + NSIS setup in installer\out

.NOTES
    SolidWorks is started and closed by hand; the add-in build still stops
    early if SolidWorks is running, since it locks sld_mcp.dll.

    The test session is an interactive Claude Code session that has only this
    MCP server, run from an empty folder outside the repo (%LOCALAPPDATA%\sld_mcp\
    test-session) so it sees the tools but not the code or CLAUDE.md.
    devloop\agent.ps1 runs the same kind of session headless.
    It holds mcp_server.exe open, so -Cycle stops it before building the server
    and resumes the same conversation afterwards.
    Any other MCP client running mcp_server.exe still blocks the server build.

    The installer target always builds Release, needs NSIS (makensis.exe on
    PATH, in its default install folder, or in MAKENSIS), and bundles the
    vc_redist.x64.exe of the Visual Studio that built the add-in.

    The test target builds the native execution tests in Release using installed
    C++ dependencies, with output separate from the installed or development DLL.
    It does not start, stop, or reset SolidWorks. SwTypeLibDir optionally overrides
    the project's default type-library location for add-in builds and tests.
#>
[CmdletBinding()]
param(
    [ValidateSet('all', 'addin', 'server', 'register', 'test', 'test-session', 'installer')]
    [string]$Target = 'all',

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug',

    [string]$SwTypeLibDir,

    [switch]$Clean,

    # Stop the MCP test session before building the server, and resume it
    # once the build succeeds.
    [switch]$Cycle
)

$ErrorActionPreference = 'Stop'
# The installer only ships Release builds.
if ($Target -eq 'installer') { $Configuration = 'Release' }
$root = $PSScriptRoot
$solution = Join-Path $root 'sld_mcp\sld_mcp.slnx'
$dll = Join-Path $root "sld_mcp\x64\$Configuration\sld_mcp.dll"
$serverDir = Join-Path $root 'mcp_server'
$exe = Join-Path $serverDir "target\$($Configuration.ToLower())\mcp_server.exe"

# Outside the repo: Claude Code loads every CLAUDE.md in the folders above its
# working folder, and the tester mustn't see the project's instructions.
# Deleting the folder starts a new session.
$testSessionDir = Join-Path $env:LOCALAPPDATA 'sld_mcp\test-session'
$testSessionConfig = Join-Path $testSessionDir 'mcp.json'
$testSessionIdFile = Join-Path $testSessionDir 'session-id.txt'
$testSessionHostFile = Join-Path $testSessionDir 'host-pid.txt'
$testSessionPendingFile = Join-Path $testSessionDir 'restart-pending'
$testSessionDebugLog = Join-Path $testSessionDir 'claude-debug.log'
# Appended to the test session's system prompt. A tester that works around a
# broken tool hides exactly the bug it's there to find. (No apostrophes: the
# text goes into a single-quoted PowerShell string.)
$testSessionPrompt = 'You are testing the solidworks MCP server under development. Work with SolidWorks only ' +
    'through its MCP tools. If a tool is missing, fails or behaves unexpectedly, stop and report exactly what ' +
    'you saw (tool names, arguments, results, errors). Do not work around it: do not start mcp_server.exe ' +
    'yourself, script SolidWorks another way, or edit files.'

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

# The claude processes started with the test session's config. Matches the
# native claude.exe as well as an npm install running under node.exe.
function Get-TestSession {
    @(Get-CimInstance Win32_Process -Filter "Name = 'claude.exe' OR Name = 'node.exe'" |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($testSessionConfig) })
}

function Get-DescendantProcessIds([int[]]$processIds) {
    $all = @(Get-CimInstance Win32_Process)
    $frontier = $processIds
    while ($frontier) {
        $children = @($all | Where-Object { $frontier -contains $_.ParentProcessId } | ForEach-Object { [int]$_.ProcessId })
        $children
        $frontier = $children
    }
}

function Stop-TestSession {
    $sessions = @(Get-TestSession)
    if ($sessions.Count -eq 0) { return }
    Write-Step "Stopping the MCP test session (PID $($sessions.ProcessId -join ', '))"

    # Kill from the window build.ps1 opened, so the window closes too; a
    # session started some other way is killed on its own.
    $hostPid = 0
    if (Test-Path -LiteralPath $testSessionHostFile) {
        [void][int]::TryParse((Get-Content -LiteralPath $testSessionHostFile -Raw).Trim(), [ref]$hostPid)
    }
    $roots = @($sessions | ForEach-Object {
        if ($hostPid -and $_.ParentProcessId -eq $hostPid) { $hostPid } else { [int]$_.ProcessId }
    } | Select-Object -Unique)

    $doomed = @($roots) + @(Get-DescendantProcessIds $roots)
    foreach ($id in $roots) {
        & taskkill.exe /PID $id /T /F | Out-Null    # /T also ends its mcp_server.exe
    }
    foreach ($id in $doomed) {
        Wait-Process -Id $id -Timeout 10 -ErrorAction SilentlyContinue
    }
    # Remembered across runs, so a cycle whose build failed still reopens it next time.
    New-Item -ItemType File -Force -Path $testSessionPendingFile | Out-Null
}

# VS Code hands tasks the PATH it started with, so a Claude Code installed
# since then isn't on it. Fall back to the PATH saved in the registry and to the
# installers' default locations.
function Find-ClaudeExe {
    if ($env:CLAUDE_EXE) {
        if (Test-Path -LiteralPath $env:CLAUDE_EXE) { return $env:CLAUDE_EXE }
        throw "CLAUDE_EXE is set to '$env:CLAUDE_EXE', which doesn't exist."
    }
    $onPath = Get-Command claude -CommandType Application, ExternalScript -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($onPath) { return $onPath.Source }

    $savedPath = @([Environment]::GetEnvironmentVariable('Path', 'User'),
                   [Environment]::GetEnvironmentVariable('Path', 'Machine')) -join ';'
    $candidates = @($savedPath -split ';' | Where-Object { $_ } | ForEach-Object {
        $dir = [Environment]::ExpandEnvironmentVariables($_)
        Join-Path $dir 'claude.exe'
        Join-Path $dir 'claude.cmd'
    })
    $candidates += @(
        (Join-Path $env:USERPROFILE '.local\bin\claude.exe'),                # native installer
        (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\claude.exe'),   # winget
        (Join-Path $env:APPDATA 'npm\claude.cmd')                            # npm -g
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
    }
    throw ("Claude Code (claude) not found. Install it from PowerShell with: irm https://claude.ai/install.ps1 | iex " +
        "-- or, if it is installed, set CLAUDE_EXE to its full path.")
}

function Start-TestSession([switch]$Resume) {
    $claudeExe = Find-ClaudeExe
    if (@(Get-TestSession).Count -gt 0) {
        Write-Host "    The MCP test session is already running."
        return
    }
    New-Item -ItemType Directory -Force -Path $testSessionDir | Out-Null
    @{ mcpServers = @{ solidworks = @{ command = $exe } } } | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath $testSessionConfig -Encoding Ascii

    $sessionId = $null
    if ($Resume -and (Test-Path -LiteralPath $testSessionIdFile)) {
        $sessionId = (Get-Content -LiteralPath $testSessionIdFile -Raw).Trim()
    }
    if ($sessionId) {
        Write-Step "Resuming the MCP test session"
        $sessionArg = "--resume $sessionId"
    }
    else {
        Write-Step "Opening a new MCP test session"
        $sessionId = [guid]::NewGuid().ToString()
        Set-Content -LiteralPath $testSessionIdFile -Value $sessionId -Encoding Ascii
        $sessionArg = "--session-id $sessionId"
    }
    Remove-Item -LiteralPath $testSessionPendingFile -ErrorAction SilentlyContinue

    # --strict-mcp-config: only this server, whatever else is configured.
    # --debug-file: Claude Code's own log (MCP connection, tool list), readable
    # from the repo. The window closes with the session, unless claude fails to start.
    $command = @"
`$Host.UI.RawUI.WindowTitle = 'SolidWorks MCP test session'
& '$claudeExe' $sessionArg --strict-mcp-config --mcp-config '$testSessionConfig' --debug-file '$testSessionDebugLog' --disallowedTools mcp__solidworks__run_macro --append-system-prompt '$testSessionPrompt'
if (`$LASTEXITCODE) { Read-Host 'claude exited with an error. Press Enter to close' }
"@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $window = Start-Process powershell.exe -WorkingDirectory $testSessionDir -PassThru `
        -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded
    Set-Content -LiteralPath $testSessionHostFile -Value $window.Id -Encoding Ascii
}

# Says which program runs each mcp_server.exe, since that's what has to go.
function Assert-ServerNotRunning {
    $running = @(Get-CimInstance Win32_Process -Filter "Name = 'mcp_server.exe'")
    if ($running.Count -eq 0) { return }
    $owners = foreach ($server in $running) {
        $parent = Get-CimInstance Win32_Process -Filter "ProcessId = $($server.ParentProcessId)"
        $parentName = if ($parent) { $parent.Name } else { 'a process that has exited' }
        "PID $($server.ProcessId), started by $parentName"
    }
    throw ("mcp_server is running ($($owners -join '; ')) and locks the output. Quit the MCP client " +
        "using it (Claude Desktop, MCP Inspector, ...), or test through -Target test-session so -Cycle can restart it.")
}

function Find-VsInstallPath {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    foreach ($prerelease in $false, $true) {
        $vswhereArgs = @('-latest', '-products', '*', '-version', '[18.0,)',
                         '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
                         '-property', 'installationPath')
        if ($prerelease) { $vswhereArgs += '-prerelease' }
        $found = & $vswhere @vswhereArgs | Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
}

# The redistributable that ships with the Visual Studio building the add-in,
# so it's at least as new as the toolset (which the runtime must be).
function Find-VcRedist {
    $vsPath = Find-VsInstallPath
    if (-not $vsPath) { return $null }
    $redistRoot = Join-Path $vsPath 'VC\Redist\MSVC'
    $versionFile = Join-Path $vsPath 'VC\Auxiliary\Build\Microsoft.VCRedistVersion.default.txt'
    if (Test-Path -LiteralPath $versionFile) {
        $candidate = Join-Path $redistRoot "$((Get-Content -LiteralPath $versionFile -Raw).Trim())\vc_redist.x64.exe"
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    # Otherwise the newest versioned folder that has one.
    $newest = Get-ChildItem -LiteralPath $redistRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -as [version] } |
        Sort-Object { [version]$_.Name } -Descending |
        ForEach-Object { Join-Path $_.FullName 'vc_redist.x64.exe' } |
        Where-Object { Test-Path -LiteralPath $_ } |
        Select-Object -First 1
    return $newest
}

function Find-MakeNsis {
    if ($env:MAKENSIS) {
        if (Test-Path -LiteralPath $env:MAKENSIS) { return $env:MAKENSIS }
        throw "MAKENSIS is set to '$env:MAKENSIS', which doesn't exist."
    }
    $onPath = Get-Command makensis -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($onPath) { return $onPath.Source }
    # The NSIS installer records its folder as the default value of this key.
    foreach ($key in 'HKLM:\SOFTWARE\WOW6432Node\NSIS', 'HKLM:\SOFTWARE\NSIS') {
        $dir = (Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue).'(default)'
        if ($dir -and (Test-Path -LiteralPath (Join-Path $dir 'makensis.exe'))) { return (Join-Path $dir 'makensis.exe') }
    }
    foreach ($dir in ${env:ProgramFiles(x86)}, $env:ProgramFiles) {
        $candidate = Join-Path $dir 'NSIS\makensis.exe'
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    throw "makensis.exe not found -- install NSIS from https://nsis.sourceforge.io, or set MAKENSIS to its full path."
}

function Build-Installer {
    $makensis = Find-MakeNsis
    foreach ($file in $dll, $exe) {
        if (-not (Test-Path -LiteralPath $file)) { throw "$file not found -- the build didn't produce it." }
    }
    $cargoToml = Join-Path $serverDir 'Cargo.toml'
    $versionMatch = Select-String -LiteralPath $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $versionMatch) { throw "No version found in $cargoToml." }
    $version = $versionMatch.Matches[0].Groups[1].Value

    $outDir = Join-Path $root 'installer\out'
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    $setup = Join-Path $outDir "sld_mcp-setup-$version.exe"
    $nsisArgs = @('/V2', '/INPUTCHARSET', 'UTF8', "/DVERSION=$version", "/DADDIN_DLL=$dll",
                  "/DSERVER_EXE=$exe", "/DOUTFILE=$setup")
    $redist = Find-VcRedist
    if ($redist) {
        $nsisArgs += "/DVCREDIST=$redist"
    }
    else {
        Write-Warning ("vc_redist.x64.exe not found in the Visual Studio install, so the installer won't " +
            "include the Visual C++ runtime. Machines without a recent one will fail to register the add-in.")
    }
    $nsisArgs += Join-Path $root 'installer\sld_mcp.nsi'

    Write-Step "Installer: makensis ($version)"
    if ($redist) { Write-Host "    bundling $redist" }
    & $makensis @nsisArgs
    if ($LASTEXITCODE -ne 0) { throw "makensis failed with exit code $LASTEXITCODE." }
    Write-Host "    $setup" -ForegroundColor Green
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
    $addinProperties = @()
    if ($SwTypeLibDir) { $addinProperties += "-p:SwTypeLibDir=$SwTypeLibDir" }
    & $msbuild $solution "-t:$msbuildTarget" "-p:Configuration=$Configuration" '-p:Platform=x64' `
        @addinProperties '-p:GenerateFullPaths=true' '-m' '-nologo' '-v:minimal'
    if ($LASTEXITCODE -ne 0) { throw "Add-in build failed (MSBuild exit code $LASTEXITCODE)." }
    if (-not $Clean) { Write-Host "    $dll" -ForegroundColor Green }
}

function Test-Addin {
    Write-Step 'Native CAD execution tests (no running SolidWorks required)'
    # Every protocol operation must use the shared execution boundary exactly once.
    $rpcNames = [regex]::Matches((Get-Content (Join-Path $root 'proto\sld_mcp.proto') -Raw), '\brpc\s+(\w+)\(') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object
    $sources = (Get-ChildItem (Join-Path $root 'sld_mcp\service') -Filter '*.cpp' | Get-Content -Raw) -join "`n"
    $executedNames = [regex]::Matches($sources, 'm_executor\.Execute\(context, "(\w+)"') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object
    if (Compare-Object $rpcNames $executedNames) { throw 'Every CAD RPC must use the shared executor exactly once.' }
    if ($sources -match '\b(?:ComApartment|LockForRequest|GetSwApp)\s*\(|std::(?:lock_guard|unique_lock)<std::mutex>') {
        throw 'CAD handlers must leave locking and COM setup to the executor.'
    }
    Write-Host "PASS: all $($rpcNames.Count) protocol operations use the shared execution boundary."

    $msbuild = Find-MSBuild
    $vsPath = Find-VsInstallPath
    $project = Join-Path $root 'sld_mcp\sld_mcp.vcxproj'
    $outDir = Join-Path $root 'build\mesh-cancellation'
    # Reuse installed dependencies and keep output away from any loaded DLL.
    $properties = @('-p:Configuration=Release', '-p:Platform=x64',
        '-p:VcpkgManifestInstall=false', "-p:OutDir=$outDir\")
    if ($SwTypeLibDir) { $properties += "-p:SwTypeLibDir=$SwTypeLibDir" }
    & $msbuild $project @properties -t:Build -m:2 -nologo -v:minimal
    if ($LASTEXITCODE) { throw 'Add-in test build failed.' }
    $settings = (& $msbuild $project @properties `
        '-getProperty:IntDir,SldMcpProtoOutDir,_ZVcpkgCurrentInstalledDir,SwTypeLibDir' -nologo | ConvertFrom-Json).Properties
    if ($LASTEXITCODE) { throw 'Could not read the add-in build directories.' }
    $projectDir = Split-Path -Parent $project
    $objectDir = [IO.Path]::GetFullPath((Join-Path $projectDir $settings.IntDir))
    $generatedDir = [IO.Path]::GetFullPath((Join-Path $projectDir $settings.SldMcpProtoOutDir))
    $dependencyDir = $settings._ZVcpkgCurrentInstalledDir

    # The fake connection prevents calls into CAD; localhost gRPC supplies cancellation.
    & (Join-Path $vsPath 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    $names = @(Get-ChildItem (Join-Path $projectDir 'service') -Filter '*.cpp' | ForEach-Object BaseName)
    $names += @('SolidWorksService', 'CadExecutor', 'MainThread', 'pch', 'sld_mcp.pb', 'sld_mcp.grpc.pb')
    $objects = @($names | ForEach-Object { Join-Path $objectDir "$_.obj" })
    $libraries = @(Get-ChildItem (Join-Path $dependencyDir 'lib') -Filter '*.lib' | ForEach-Object FullName)
    $testExe = Join-Path $outDir 'mesh_cancellation_tests.exe'
    & cl.exe /nologo /std:c++17 /EHsc /MD /D_UNICODE /DUNICODE /D_USE_MATH_DEFINES "/I$projectDir" "/I$($settings.SwTypeLibDir)" `
        "/I$generatedDir" "/I$(Join-Path $dependencyDir 'include')" `
        (Join-Path $projectDir 'tests\MeshCancellation.cpp') (Join-Path $projectDir 'tests\PmiPreferences.cpp') `
        "/Fo$outDir\" "/Fe$testExe" `
        /link @objects @libraries ws2_32.lib crypt32.lib iphlpapi.lib bcrypt.lib dbghelp.lib `
        ole32.lib oleaut32.lib uuid.lib user32.lib advapi32.lib shell32.lib
    if ($LASTEXITCODE) { throw 'Native execution test build failed.' }
    & $testExe
    if ($LASTEXITCODE) { throw 'Native execution regression failed.' }
}

function Build-Server {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found on PATH -- install Rust from https://rustup.rs"
    }
    if (-not $Clean) {
        Assert-ServerNotRunning
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

if ($Clean -and $Target -in 'installer', 'test') {
    throw "-Clean doesn't apply to -Target $Target; use it with -Target all, addin or server."
}
if ($Cycle -and ($Clean -or $Target -notin 'all', 'server')) {
    throw "-Cycle restarts the test session around the server build, so it needs -Target all or server, without -Clean."
}

$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
if ($Cycle) { Stop-TestSession }
switch ($Target) {
    'all'      { Build-Addin; Build-Server }
    'addin'    { Build-Addin }
    'server'   { Build-Server }
    'test'     { Test-Addin }
    'register' { Register-Addin }
    'test-session' { Start-TestSession }
    'installer' { Build-Addin; Build-Server; Build-Installer }
}
# Only reached when the builds succeeded: after a failed build there's nothing
# new to test, so the test session stays closed until the next good build.
if ($Cycle -and (Test-Path -LiteralPath $testSessionPendingFile)) { Start-TestSession -Resume }
Write-Host ""
Write-Host ("Done in {0:N1} s" -f $stopwatch.Elapsed.TotalSeconds) -ForegroundColor Green
