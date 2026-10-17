<#
.SYNOPSIS
    Runs a PowerShell snippet against the running SolidWorks through COM, for
    test fixtures no MCP tool can make (fillets, assemblies, folders, ...).

.EXAMPLE
    .\devloop\swcom.ps1 -CodeFile fixture.ps1

    The snippet gets $sw (ISldWorks) and calls members by name with
    `Com $object Name @(args)` or `Com $object Property -Get`; whatever it
    outputs is printed. Exit code 1 if it throws.

.NOTES
    Windows PowerShell 5.1. Same late-bound calls as sw.ps1 (PowerShell's COM
    adapter can't get SolidWorks' type info), in a job with a timeout since a
    modal dialog can block a call forever. The code comes from a file because
    quotes in a -Command/-File argument get mangled on the way.
    A null object argument (e.g. SelectByID2's Callout) must be
    [Runtime.InteropServices.DispatchWrapper]::new([object]$null), else
    DISP_E_TYPEMISMATCH. Methods with out parameters (GetObjectByPersistReference3)
    don't work late-bound: call them through the interface type of
    SolidWorks.Interop.sldworks.dll instead ([type].GetMethod(...).Invoke).
#>
param(
    [Parameter(Mandatory)][string]$CodeFile,
    [int]$TimeoutSec = 60
)

$ErrorActionPreference = 'Stop'
$code = Get-Content -Raw -LiteralPath $CodeFile
$job = Start-Job -ArgumentList $code -ScriptBlock {
    param($body)
    $ErrorActionPreference = 'Stop'
    function Com($object, [string]$name, [object[]]$memberArgs = @(), [switch]$Get) {
        $kind = if ($Get) { 'GetProperty' } else { 'InvokeMethod' }
        [System.__ComObject].InvokeMember($name, [Reflection.BindingFlags]$kind, $null, $object, $memberArgs)
    }
    $sw = [Runtime.InteropServices.Marshal]::GetActiveObject('SldWorks.Application')
    & ([scriptblock]::Create($body))
}
try {
    if (-not (Wait-Job $job -Timeout $TimeoutSec)) {
        throw "TIMEOUT: SolidWorks didn't finish within $TimeoutSec s (a modal dialog? run: sw.ps1 dialogs)"
    }
    Receive-Job $job
    if ($job.State -eq 'Failed') { exit 1 }
}
finally {
    Remove-Job $job -Force -ErrorAction SilentlyContinue
}
