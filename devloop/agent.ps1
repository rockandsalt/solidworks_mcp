<#
.SYNOPSIS
    Hands a task to a headless Claude Code session that has only the SolidWorks
    MCP tools, and saves its report: does a model manage with the tools?

.EXAMPLE
    .\devloop\agent.ps1 "Model a 60 x 40 x 8 mm plate with four 6 mm corner holes, then report."
    .\devloop\agent.ps1 -Resume "I fixed the cut direction; try the cut again."
    .\devloop\agent.ps1 -TaskFile tasks.txt -Model sonnet -MaxBudgetUsd 1

.NOTES
    Like build.ps1 -Target test-session, but scripted: claude -p from
    %LOCALAPPDATA%\sld_mcp\test-agent (outside the repo, so no CLAUDE.md),
    --strict-mcp-config with only this server, no built-in tools.

    A new session starts SolidWorks if needed and closes every document
    first (sw.ps1 start + reset); -Resume continues the last session and
    leaves the documents alone. claude exits after each task, so
    mcp_server.exe is never left running to block a rebuild.

    Output: devloop\out\<time>-agent\ with report.md (the agent's answer),
    transcript.md (its tool calls and results), stream.jsonl (raw). See
    docs/notes/devloop.md.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Task,

    # Read the task from a file instead.
    [string]$TaskFile,

    # Continue the previous session instead of starting a new one.
    [switch]$Resume,

    # Model alias or id; default: Claude Code's default.
    [string]$Model,

    [double]$MaxBudgetUsd = 3,

    [int]$TimeoutSec = 1200
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$sw = Join-Path $PSScriptRoot 'sw.ps1'
$exe = Join-Path $root 'mcp_server\target\debug\mcp_server.exe'
$sessionDir = Join-Path $env:LOCALAPPDATA 'sld_mcp\test-agent'
$config = Join-Path $sessionDir 'mcp.json'
$sessionIdFile = Join-Path $sessionDir 'session-id.txt'
# Same rules as build.ps1's test session: a tester that works around a
# broken tool hides exactly the bug it's there to find.
$testerPrompt = 'You are testing the solidworks MCP server under development. Work with SolidWorks only ' +
    'through its MCP tools. If a tool is missing, fails or behaves unexpectedly, stop and report exactly what ' +
    'you saw (tool names, arguments, results, errors). Do not work around it. End with a short report: the ' +
    'tool calls you made, what was confusing or did not work as described, and the result.'

if ($TaskFile) { $Task = Get-Content -LiteralPath $TaskFile -Raw }
if (-not $Task -or -not $Task.Trim()) { throw "Give a task, or -TaskFile." }
if (-not (Test-Path -LiteralPath $exe)) { throw "$exe not found -- build it first (build.ps1 -Target server)." }
$claude = Get-Command claude -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $claude) { throw "claude not found on PATH." }

New-Item -ItemType Directory -Force -Path $sessionDir | Out-Null
@{ mcpServers = @{ solidworks = @{ command = $exe } } } | ConvertTo-Json -Depth 4 |
    Set-Content -LiteralPath $config -Encoding Ascii

if ($Resume) {
    if (-not (Test-Path -LiteralPath $sessionIdFile)) { throw "No previous session to resume; run without -Resume." }
    $sessionId = (Get-Content -LiteralPath $sessionIdFile -Raw).Trim()
    $sessionArgs = @('--resume', $sessionId)
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $sw start
}
else {
    $sessionId = [guid]::NewGuid().ToString()
    Set-Content -LiteralPath $sessionIdFile -Value $sessionId -Encoding Ascii
    $sessionArgs = @('--session-id', $sessionId)
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $sw start
    if ($LASTEXITCODE -eq 0) { & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $sw reset }
}
if ($LASTEXITCODE -ne 0) { throw "sw.ps1 failed; SolidWorks isn't ready." }

$runDir = Join-Path $PSScriptRoot ("out\{0:yyyyMMdd-HHmmss}-agent" -f (Get-Date))
New-Item -ItemType Directory -Force -Path $runDir | Out-Null
$stream = Join-Path $runDir 'stream.jsonl'
Set-Content -LiteralPath (Join-Path $runDir 'task.md') -Value $Task -Encoding UTF8

$claudeArgs = @('-p') + $sessionArgs + @(
    '--strict-mcp-config', '--mcp-config', $config,
    '--tools', '""',
    '--allowedTools', 'mcp__solidworks',
    '--append-system-prompt', $testerPrompt,
    '--max-budget-usd', $MaxBudgetUsd,
    '--output-format', 'stream-json', '--verbose')
if ($Model) { $claudeArgs += @('--model', $Model) }

Write-Host ("[{0:HH:mm:ss}] Agent session {1} ({2}) working; output in {3}" -f (Get-Date), $sessionId,
    $(if ($Resume) { 'resumed' } else { 'new' }), $runDir)
# In a job so a stuck session can be abandoned. The task goes in on stdin
# (no quoting trouble); UTF-8 both ways, which Windows PowerShell doesn't default to.
$job = Start-Job -ArgumentList $claude.Source, $claudeArgs, $Task, $sessionDir, $stream -ScriptBlock {
    param($claudeExe, $arguments, $taskText, $workDir, $outFile)
    $OutputEncoding = [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
    Set-Location -LiteralPath $workDir
    $taskText | & $claudeExe @arguments | Out-File -LiteralPath $outFile -Encoding utf8
    $LASTEXITCODE
}
if (-not (Wait-Job $job -Timeout $TimeoutSec)) {
    # Kill the session's claude (and its mcp_server.exe) by its unique config path.
    Get-CimInstance Win32_Process -Filter "Name = 'claude.exe' OR Name = 'node.exe'" |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($config) } |
        ForEach-Object { & taskkill.exe /PID $_.ProcessId /T /F | Out-Null }
    Stop-Job $job
    Remove-Job $job -Force
    throw "The agent didn't finish within $TimeoutSec s; partial output in $stream. Continue it with -Resume."
}
$exitCode = Receive-Job $job
Remove-Job $job -Force

# transcript.md: what the agent said and called, in order.
$transcript = New-Object System.Collections.Generic.List[string]
$result = $null
foreach ($line in Get-Content -LiteralPath $stream -Encoding UTF8) {
    if (-not $line.Trim()) { continue }
    try { $event = $line | ConvertFrom-Json } catch { continue }
    switch ($event.type) {
        'assistant' {
            foreach ($block in $event.message.content) {
                if ($block.type -eq 'text' -and $block.text.Trim()) { $transcript.Add($block.text.Trim()); $transcript.Add('') }
                elseif ($block.type -eq 'tool_use') {
                    $transcript.Add(('-> **{0}** `{1}`' -f ($block.name -replace '^mcp__solidworks__', ''),
                        ($block.input | ConvertTo-Json -Depth 20 -Compress)))
                }
            }
        }
        'user' {
            foreach ($block in @($event.message.content)) {
                if ($block.type -ne 'tool_result') { continue }
                $text = (@($block.content) | ForEach-Object { if ($_.type -eq 'text') { $_.text } elseif ($_.type) { "[$($_.type)]" } else { "$_" } }) -join ' '
                if ($text.Length -gt 600) { $text = $text.Substring(0, 600) + ' [...]' }
                $transcript.Add(('   <- {0}{1}' -f $(if ($block.is_error) { 'ERROR: ' } else { '' }), ($text -replace '\s+', ' ')))
                $transcript.Add('')
            }
        }
        'result' { $result = $event }
    }
}
Set-Content -LiteralPath (Join-Path $runDir 'transcript.md') -Value $transcript -Encoding UTF8

if (-not $result) { throw "claude exited ($exitCode) without a result; see $stream." }
Set-Content -LiteralPath (Join-Path $runDir 'report.md') -Value $result.result -Encoding UTF8
Write-Host ("[{0:HH:mm:ss}] Done: {1} turns, {2:N0} s, `${3:N2}{4}. Report: {5}" -f (Get-Date), $result.num_turns,
    ($result.duration_ms / 1000), $result.total_cost_usd, $(if ($result.is_error) { ", ERROR ($($result.subtype))" } else { '' }),
    (Join-Path $runDir 'report.md'))
Write-Host ""
Write-Host $result.result
if ($result.is_error) { exit 1 }
