<#
.SYNOPSIS
    Starts, stops and checks SolidWorks with the sld_mcp add-in, so a dev
    loop can run without anyone at the keyboard.

.EXAMPLE
    .\devloop\sw.ps1 status        # running? add-in listening? open documents
    .\devloop\sw.ps1 start         # launch if needed, wait until the add-in answers
    .\devloop\sw.ps1 stop          # exit SolidWorks, discarding never-saved documents
    .\devloop\sw.ps1 stop -Force   # also discard unsaved changes to saved files, kill if it won't exit
    .\devloop\sw.ps1 restart
    .\devloop\sw.ps1 reset         # close every document (clean slate for a scenario)

    stop and reset refuse when a saved file has unsaved changes (someone's
    work), unless -Force.
    .\devloop\sw.ps1 screenshot    # PNG of the whole desktop, e.g. to see a dialog

.NOTES
    Windows PowerShell 5.1 (COM through Marshal.GetActiveObject, which
    PowerShell 7 doesn't have). See docs/notes/devloop.md.

    The add-in pins its own DLL (PinThisModule), so only exiting SolidWorks
    releases the lock on sld_mcp.dll; there is no unload/reload shortcut.

    Every COM call runs in a background job with a timeout: a modal dialog in
    SolidWorks can block a call forever. Calls go through IDispatch by name
    (Com helper below): PowerShell's COM adapter fails on every SolidWorks
    method with TYPE_E_ELEMENTNOTFOUND, since it can't get type info.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('status', 'start', 'stop', 'restart', 'reset', 'screenshot')]
    [string]$Action = 'status',

    # stop: discard unsaved changes, and kill SolidWorks if it doesn't exit.
    [switch]$Force,

    # start: how long to wait for SolidWorks and the add-in, in total.
    [int]$TimeoutSec = 480,

    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Debug'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $PSScriptRoot 'out'
$dll = Join-Path $root "sld_mcp\x64\$Configuration\sld_mcp.dll"
$port = if ($env:SLD_MCP_PORT) { [int]$env:SLD_MCP_PORT } else { 50551 }

# The 3DEXPERIENCE launcher command from CLAUDE.md. It returns at once; the
# launcher then signs in and starts SLDWORKS.exe on its own.
$catstart = 'C:\Program Files\Dassault Systemes\SOLIDWORKS 3DEXPERIENCE R2026x\win_b64\code\bin\CATSTART.exe'
$catstartArgs = '-run "SWXDesktopLauncher.exe" -object "--AppName=\"SWXCSWK_AP\" -tenant=OI000596756 -monoapp ' +
    '-3DRegistryURL=https://eu1-makers-registry.3dexperience.3ds.com ' +
    '-LaunchAppDir=\"C:\Program Files\Dassault Systemes\SOLIDWORKS Apps 2026\SOLIDWORKS\" " -nowindow'

function Write-Step([string]$message) {
    Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $message)
}

function Get-SwProcess {
    @(Get-Process -Name SLDWORKS -ErrorAction SilentlyContinue)
}

function Test-AddinListening {
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $connect = $client.BeginConnect('127.0.0.1', $port, $null, $null)
        if (-not $connect.AsyncWaitHandle.WaitOne(1000)) { return $false }
        $client.EndConnect($connect)
        return $true
    }
    catch { return $false }
    finally { $client.Close() }
}

# Runs $script with $sw = the running ISldWorks in a separate process, so a
# blocked call can be abandoned. The script calls members through
# `Com $object Name [args] [-Get]`. Returns what the script outputs, or
# throws with the reason.
function Invoke-SwCom([scriptblock]$script, [object[]]$arguments = @(), [int]$timeoutSec = 30) {
    $job = Start-Job -ArgumentList $script.ToString(), $arguments -ScriptBlock {
        param($body, $jobArgs)
        $ErrorActionPreference = 'Stop'
        function Com($object, [string]$name, [object[]]$memberArgs = @(), [switch]$Get) {
            $kind = if ($Get) { 'GetProperty' } else { 'InvokeMethod' }
            [System.__ComObject].InvokeMember($name, [Reflection.BindingFlags]$kind, $null, $object, $memberArgs)
        }
        try {
            $sw = [Runtime.InteropServices.Marshal]::GetActiveObject('SldWorks.Application')
        }
        catch {
            throw "NO_COM: SolidWorks isn't reachable through COM yet ($($_.Exception.Message))"
        }
        & ([scriptblock]::Create($body)) $sw @jobArgs
    }
    try {
        if (-not (Wait-Job $job -Timeout $timeoutSec)) {
            throw "TIMEOUT: SolidWorks didn't answer a COM call within $timeoutSec s (a modal dialog? run: sw.ps1 screenshot)"
        }
        $output = Receive-Job $job -ErrorAction SilentlyContinue -ErrorVariable jobErrors
        if ($job.State -eq 'Failed' -or $jobErrors) {
            $reason = if ($jobErrors) { $jobErrors[0].ToString() } else { $job.ChildJobs[0].JobStateInfo.Reason.Message }
            throw $reason
        }
        return $output
    }
    finally {
        Remove-Job $job -Force -ErrorAction SilentlyContinue
    }
}

# Title + unsaved flag of every open document.
$listDocuments = {
    param($sw)
    $active = Com $sw ActiveDoc -Get
    $activeTitle = if ($active) { Com $active GetTitle } else { $null }
    $doc = Com $sw GetFirstDocument
    while ($doc) {
        $title = Com $doc GetTitle
        [pscustomobject]@{ Title = $title; Path = (Com $doc GetPathName); Dirty = [bool](Com $doc GetSaveFlag);
                           Active = ($title -eq $activeTitle) }
        $doc = Com $doc GetNext
    }
}

function Save-Screenshot([string]$name = 'screenshot') {
    Add-Type -AssemblyName System.Windows.Forms, System.Drawing
    if (-not ('DevloopDpi' -as [type])) {
        # Without this, a scaled display is captured at the wrong size.
        Add-Type -Name DevloopDpi -Namespace '' -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'
    }
    [void][DevloopDpi]::SetProcessDPIAware()
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen($bounds.Left, $bounds.Top, 0, 0, $bitmap.Size)
        New-Item -ItemType Directory -Force -Path $outDir | Out-Null
        $file = Join-Path $outDir ("{0}-{1:yyyyMMdd-HHmmss}.png" -f $name, (Get-Date))
        $bitmap.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
        return $file
    }
    finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

# Something went wrong while SolidWorks may be showing a dialog: keep a
# screenshot and the window titles, then fail.
function Fail-WithDiagnostics([string]$message) {
    $shot = try { Save-Screenshot 'failure' } catch { "(screenshot failed: $($_.Exception.Message))" }
    $windows = Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowTitle -and $_.Name -match 'SLDWORKS|SWX|CAT|3DEXP|msedge|chrome' } |
        ForEach-Object { "    $($_.Name) ($($_.Id)): $($_.MainWindowTitle)" }
    Write-Host "Windows:"
    if ($windows) { $windows | Write-Host } else { Write-Host "    (none)" }
    Write-Host "Screenshot: $shot"
    throw $message
}

function Show-Status {
    $procs = Get-SwProcess
    if ($procs.Count -eq 0) {
        Write-Host "SolidWorks: not running"
        return
    }
    Write-Host "SolidWorks: running (PID $($procs.Id -join ', '), started $($procs[0].StartTime))"
    Write-Host ("Add-in:     {0} on 127.0.0.1:{1}" -f $(if (Test-AddinListening) { 'listening' } else { 'NOT listening' }), $port)
    try {
        $docs = @(Invoke-SwCom $listDocuments -timeoutSec 20)
        Write-Host "Documents:  $(if ($docs.Count) { '' } else { '(none)' })"
        foreach ($doc in $docs) {
            Write-Host ("    {0}{1}{2}  {3}" -f $doc.Title, $(if ($doc.Active) { ' (active)' } else { '' }),
                $(if ($doc.Dirty) { ' *unsaved*' } else { '' }), $doc.Path)
        }
    }
    catch {
        Write-Host "COM:        $($_.Exception.Message)"
    }
}

function Start-Sw {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    if ((Get-SwProcess).Count -gt 0 -and (Test-AddinListening)) {
        Write-Step "SolidWorks is already up, add-in listening on $port"
        return
    }
    if ((Get-SwProcess).Count -eq 0) {
        if (-not (Test-Path -LiteralPath $catstart)) { throw "$catstart not found." }
        Write-Step "Launching SolidWorks through the 3DEXPERIENCE launcher"
        Start-Process -FilePath $catstart -ArgumentList $catstartArgs | Out-Null
        while ((Get-SwProcess).Count -eq 0) {
            if ((Get-Date) -gt $deadline) {
                Fail-WithDiagnostics ("SLDWORKS.exe didn't start within $TimeoutSec s. If the screenshot shows a " +
                    "3DEXPERIENCE sign-in, someone has to sign in once by hand.")
            }
            Start-Sleep -Seconds 2
        }
        Write-Step "SLDWORKS.exe started (PID $((Get-SwProcess).Id -join ', '))"
    }

    # SolidWorks normally loads the add-in itself (AddInsStartup). If it
    # hasn't once COM works, ask for it explicitly: LoadAddIn by path loads
    # this build even when the registered/enabled state says otherwise.
    $comSince = $null
    $lastLoad = $null
    while (-not (Test-AddinListening)) {
        if ((Get-SwProcess).Count -eq 0) { Fail-WithDiagnostics "SolidWorks exited while starting." }
        if ((Get-Date) -gt $deadline) {
            Fail-WithDiagnostics "SolidWorks is running but the add-in didn't listen on $port within $TimeoutSec s."
        }
        $comReady = $false
        try {
            $revision = Invoke-SwCom { param($sw) Com $sw RevisionNumber } -timeoutSec 20
            $comReady = $true
        }
        catch {
            if ($_.Exception.Message -notmatch '^NO_COM') { Write-Step $_.Exception.Message }
        }
        if ($comReady) {
            if (-not $comSince) {
                $comSince = Get-Date
                Write-Step "COM is up (SolidWorks $revision); waiting for SolidWorks to load the add-in"
            }
            elseif (((Get-Date) - $comSince).TotalSeconds -ge 20 -and
                    (-not $lastLoad -or ((Get-Date) - $lastLoad).TotalSeconds -ge 30)) {
                $lastLoad = Get-Date
                try {
                    # 0 = loaded, 2 = already loaded, -1 = failed, 1 = not loaded (swLoadAddinError_e).
                    $result = Invoke-SwCom { param($sw, $path) Com $sw LoadAddIn @($path) } -arguments @($dll) -timeoutSec 60
                    Write-Step "LoadAddIn($dll) -> $result"
                }
                catch {
                    Write-Step "LoadAddIn failed: $($_.Exception.Message)"
                }
            }
        }
        Start-Sleep -Seconds 3
    }
    Write-Step "SolidWorks is up, add-in listening on $port"
}

# Scenarios leave never-saved documents (Part1, ...) behind, which are fine to
# throw away. A saved file with unsaved changes is someone's work: refuse
# unless -Force.
function Assert-NothingToLose([string]$what) {
    if ($Force) { return }
    $atRisk = @(Invoke-SwCom $listDocuments -timeoutSec 20 | Where-Object { $_.Dirty -and $_.Path })
    if ($atRisk.Count) {
        throw ("These files have unsaved changes: $($atRisk.Path -join ', '). Save them, or run " +
            "'sw.ps1 $what -Force' to discard them.")
    }
}

function Reset-Sw {
    if ((Get-SwProcess).Count -eq 0) { throw "SolidWorks isn't running." }
    Assert-NothingToLose 'reset'
    $closed = Invoke-SwCom { param($sw) Com $sw CloseAllDocuments @($true) } -timeoutSec 60
    $left = @(Invoke-SwCom $listDocuments -timeoutSec 20)
    if (-not $closed -or $left.Count) {
        Fail-WithDiagnostics "CloseAllDocuments returned $closed; still open: $($left.Title -join ', ')"
    }
    Write-Step "Closed every document"
}

function Stop-Sw {
    $procs = Get-SwProcess
    if ($procs.Count -eq 0) {
        Write-Step "SolidWorks isn't running"
        return
    }
    try {
        Assert-NothingToLose 'stop'
        Write-Step "Closing documents and exiting SolidWorks"
        # CloseAllDocuments(TRUE) closes modified documents too, without asking.
        Invoke-SwCom { param($sw) [void](Com $sw CloseAllDocuments @($true)); Com $sw ExitApp } -timeoutSec 60 | Out-Null
    }
    catch {
        if ($_.Exception.Message -match '^These files') { throw }
        if (-not $Force) {
            throw "Couldn't exit SolidWorks through COM: $($_.Exception.Message). Use -Force to kill it."
        }
        Write-Step "COM exit failed ($($_.Exception.Message)); killing SolidWorks"
        $procs | Stop-Process -Force
    }
    foreach ($proc in $procs) {
        if (-not $proc.WaitForExit(60000)) {
            if (-not $Force) { throw "SolidWorks (PID $($proc.Id)) didn't exit within 60 s. Use -Force to kill it." }
            Write-Step "SolidWorks (PID $($proc.Id)) didn't exit within 60 s; killing it"
            $proc | Stop-Process -Force
            [void]$proc.WaitForExit(15000)
        }
    }
    Write-Step "SolidWorks stopped"
}

switch ($Action) {
    'status'     { Show-Status }
    'start'      { Start-Sw }
    'stop'       { Stop-Sw }
    'restart'    { Stop-Sw; Start-Sw }
    'reset'      { Reset-Sw }
    'screenshot' { Save-Screenshot }
}
