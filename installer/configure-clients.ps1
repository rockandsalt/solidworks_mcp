<#
.SYNOPSIS
    Adds or removes the SolidWorks MCP server in Claude Desktop's or Codex's
    configuration for the current user.

.DESCRIPTION
    Run by the NSIS installer (and its uninstaller), but also works by hand:

        .\configure-clients.ps1 -Action Add -Client Claude -ServerPath 'C:\Program Files\SolidWorks MCP\mcp_server.exe'
        .\configure-clients.ps1 -Action Remove -Client Codex

    Claude Desktop: mcpServers.<Name> in claude_desktop_config.json. The
    Microsoft Store / MSIX build reads it from its package folder
    (%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude), the classic
    .exe build from %APPDATA%\Claude, so every one of those that exists is
    updated. With neither present, the classic location is created.

    Codex: [mcp_servers.<Name>] in config.toml under %CODEX_HOME%, or
    %USERPROFILE%\.codex when that isn't set. The Codex app, CLI and IDE
    extension all read this file.

    Each file is backed up next to itself (*.sld_mcp.bak) before it's
    changed, and a file that doesn't parse is left alone. Removing an entry
    that isn't there is not an error.

    Windows PowerShell 5.1 compatible, ASCII only (5.1 reads BOM-less scripts
    as ANSI).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Add', 'Remove')]
    [string]$Action,

    [Parameter(Mandatory = $true)]
    [ValidateSet('Claude', 'Codex')]
    [string]$Client,

    # mcp_server.exe; needed for -Action Add.
    [string]$ServerPath,

    # Key the server is registered under in the client's config.
    [ValidatePattern('^[A-Za-z0-9_-]+$')]
    [string]$Name = 'solidworks',

    # Codex: seconds a single tool call may take. The server's own gRPC
    # timeout to the add-in is 60 s, so give it some headroom on top.
    [int]$ToolTimeoutSec = 120
)

$ErrorActionPreference = 'Stop'
$utf8NoBom = New-Object System.Text.UTF8Encoding $false

function Backup-File([string]$path) {
    Copy-Item -LiteralPath $path -Destination "$path.sld_mcp.bak" -Force
}

# ---------------------------------------------------------------- Claude ---

function Get-ClaudeConfigFiles {
    $dirs = @()
    $packages = Join-Path $env:LOCALAPPDATA 'Packages'
    if (Test-Path -LiteralPath $packages) {
        $dirs += @(Get-ChildItem -LiteralPath $packages -Directory -Filter 'Claude_*' -ErrorAction SilentlyContinue |
            ForEach-Object { Join-Path $_.FullName 'LocalCache\Roaming\Claude' })
    }
    $classic = Join-Path $env:APPDATA 'Claude'
    if ((Test-Path -LiteralPath $classic) -or $dirs.Count -eq 0) {
        $dirs += $classic
    }
    $dirs | ForEach-Object { Join-Path $_ 'claude_desktop_config.json' }
}

function Update-ClaudeConfig([string]$path) {
    $exists = Test-Path -LiteralPath $path
    if (-not $exists -and $Action -eq 'Remove') { return }

    $config = $null
    if ($exists) {
        $raw = [System.IO.File]::ReadAllText($path)
        if ($raw.Trim()) {
            try { $config = $raw | ConvertFrom-Json }
            catch { throw "$path isn't valid JSON, so it was left unchanged: $($_.Exception.Message)" }
        }
    }
    if ($null -eq $config) { $config = New-Object PSObject }

    $servers = $null
    if ($config.PSObject.Properties['mcpServers']) { $servers = $config.mcpServers }

    if ($Action -eq 'Add') {
        if ($null -eq $servers) {
            $servers = New-Object PSObject
            $config | Add-Member -NotePropertyName 'mcpServers' -NotePropertyValue $servers -Force
        }
        $entry = New-Object PSObject
        $entry | Add-Member -NotePropertyName 'command' -NotePropertyValue $ServerPath
        $entry | Add-Member -NotePropertyName 'args' -NotePropertyValue @()
        $servers | Add-Member -NotePropertyName $Name -NotePropertyValue $entry -Force
    }
    else {
        if ($null -eq $servers -or -not $servers.PSObject.Properties[$Name]) { return }
        $servers.PSObject.Properties.Remove($Name)
    }

    if ($exists) { Backup-File $path }
    else { New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null }
    $json = $config | ConvertTo-Json -Depth 64
    [System.IO.File]::WriteAllText($path, $json, $utf8NoBom)
    $verb = if ($Action -eq 'Add') { 'Added' } else { 'Removed' }
    Write-Output "$verb '$Name' in $path"
}

# ----------------------------------------------------------------- Codex ---

function Get-CodexConfigFile {
    $codexHome = $env:CODEX_HOME
    if (-not $codexHome) { $codexHome = Join-Path $env:USERPROFILE '.codex' }
    Join-Path $codexHome 'config.toml'
}

# A TOML string for a Windows path: a literal string keeps the backslashes
# as they are, unless the path itself contains a single quote.
function ConvertTo-TomlString([string]$value) {
    if ($value -notmatch "['\r\n]") { return "'$value'" }
    '"' + ($value -replace '\\', '\\' -replace '"', '\"') + '"'
}

# Drops [mcp_servers.<Name>] and its sub-tables ([mcp_servers.<Name>.env],
# ...), i.e. every line from such a header up to the next unrelated header.
function Remove-CodexServerTable([string[]]$lines) {
    $quotedName = [regex]::Escape($Name)
    $ownHeader = '^\s*\[\s*mcp_servers\s*\.\s*(' + $quotedName + '|"' + $quotedName + '"|''' + $quotedName + ''')\s*[\.\]]'
    $removed = $false
    $skipping = $false
    $kept = New-Object System.Collections.Generic.List[string]
    foreach ($line in $lines) {
        if ($line -match '^\s*\[') {
            $skipping = $line -match $ownHeader
            if ($skipping) { $removed = $true }
        }
        if (-not $skipping) { $kept.Add($line) }
    }
    # Blank lines that preceded the removed table.
    while ($kept.Count -gt 0 -and -not $kept[$kept.Count - 1].Trim()) { $kept.RemoveAt($kept.Count - 1) }
    return @{ Lines = $kept; Removed = $removed }
}

function Update-CodexConfig([string]$path) {
    $exists = Test-Path -LiteralPath $path
    if (-not $exists -and $Action -eq 'Remove') { return }

    $raw = ''
    if ($exists) { $raw = [System.IO.File]::ReadAllText($path) }
    $newline = if ($raw.Contains("`r`n")) { "`r`n" } else { "`n" }
    $lines = @()
    if ($raw) { $lines = $raw -split '\r?\n' }

    $result = Remove-CodexServerTable $lines
    $kept = $result.Lines

    if ($Action -eq 'Add') {
        if ($kept.Count -gt 0) { $kept.Add('') }
        $kept.Add("[mcp_servers.$Name]")
        $kept.Add("command = $(ConvertTo-TomlString $ServerPath)")
        $kept.Add("tool_timeout_sec = $ToolTimeoutSec")
    }
    elseif (-not $result.Removed) {
        return
    }

    if ($exists) { Backup-File $path }
    else { New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null }
    $text = ''
    if ($kept.Count -gt 0) { $text = ($kept -join $newline) + $newline }
    [System.IO.File]::WriteAllText($path, $text, $utf8NoBom)
    $verb = if ($Action -eq 'Add') { 'Added' } else { 'Removed' }
    Write-Output "$verb [mcp_servers.$Name] in $path"
}

# ------------------------------------------------------------------ main ---

try {
    if ($Action -eq 'Add') {
        if (-not $ServerPath) { throw '-ServerPath is required with -Action Add.' }
        if (-not (Test-Path -LiteralPath $ServerPath -PathType Leaf)) { throw "$ServerPath doesn't exist." }
        $ServerPath = (Resolve-Path -LiteralPath $ServerPath).ProviderPath
    }
    switch ($Client) {
        'Claude' { foreach ($file in @(Get-ClaudeConfigFiles)) { Update-ClaudeConfig $file } }
        'Codex' { Update-CodexConfig (Get-CodexConfigFile) }
    }
    exit 0
}
catch {
    Write-Output "ERROR: $($_.Exception.Message)"
    exit 1
}
