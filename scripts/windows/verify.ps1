<#
.SYNOPSIS
Runs the Windows verification runbook (W1-W8 in .overall_plan.md section 6.3) and writes a results table.

.DESCRIPTION
Builds into <repo>\target-win so a WSL or Linux build in <repo>\target is never touched.
W4, W5, W7 and W8 need an elevated shell. W6 needs a non-elevated scan: from an elevated shell it is
started with `runas /trustlevel:0x20000`. Logs, databases and results.md go to -OutDir.
Exit code 0 when no step failed (skipped steps are allowed), 1 otherwise.

.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\verify.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\verify.ps1 -Step W5,W6,W7
#>
param(
    [string[]]$Step = @('W1', 'W2', 'W3', 'W4', 'W5', 'W6', 'W7', 'W8'),
    # Scratch tree for W5-W8. Recreated on every run; must be empty or made by this script.
    [string]$TestRoot = 'C:\dd_test',
    [string]$OutDir = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# `powershell -File ... -Step W5,W8` binds one string "W5,W8", so split it here.
$Step = @($Step | ForEach-Object { $_ -split ',' } | Where-Object { $_ })

$Repo =(Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$env:CARGO_TARGET_DIR = Join-Path $Repo 'target-win'
if (-not $OutDir) {
    $OutDir = Join-Path $env:CARGO_TARGET_DIR ('verify\' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$Exe = Join-Path $env:CARGO_TARGET_DIR 'debug\dedupe.exe'
$Marker = '.dedupe-verify'
$script:Results = New-Object System.Collections.Generic.List[object]
$script:Baseline = $null
Set-Location $Repo

function Test-Elevated {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# Runs a native command, writes its combined output to <OutDir>\<Name>.log (UTF-8) and returns
# @{ Code; Lines; Out; Err } (Out = stdout, Err = stderr). Native stderr must not turn into a
# terminating error, hence 'Continue'; it arrives as ErrorRecord objects, which splits the streams.
function Invoke-Logged([string]$Name, [string]$File, [string[]]$ArgList) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $all = @(& $File @ArgList 2>&1)
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    $lines = @($all | ForEach-Object { "$_" })
    $out = @($all | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] } | ForEach-Object { "$_" })
    $err = @($all | Where-Object { $_ -is [System.Management.Automation.ErrorRecord] } | ForEach-Object { "$_" })
    $lines | Set-Content -Path (Join-Path $OutDir "$Name.log") -Encoding UTF8
    Write-Host ("[{0}] exit {1}, {2} lines -> {3}.log" -f $Name, $code, $lines.Count, $Name)
    return @{ Code = $code; Lines = $lines; Out = $out; Err = $err }
}

function Add-Result([string]$Id, [string]$Result, [string]$Evidence) {
    $script:Results.Add([pscustomobject]@{ Step = $Id; Result = $Result; Evidence = $Evidence })
    Write-Host ("{0}: {1} - {2}" -f $Id, $Result, $Evidence)
}

# Last line matching $Pattern, or the last output line, as decisive evidence for a failure.
function Get-Decisive($Lines, [string]$Pattern = 'error|panicked|FAILED|failed') {
    $hit = @($Lines | Where-Object { $_ -match $Pattern }) | Select-Object -Last 1
    if ($hit) { return $hit.Trim() }
    $last = @($Lines) | Select-Object -Last 1
    if ($last) { return $last.Trim() }
    return '(no output)'
}

# One sorted string per duplicate group: "<full_hash>|<member paths, sorted, ;-joined>".
function Get-GroupSet([string[]]$JsonLines) {
    # Keep only the JSON document, in case a status line reached stdout.
    $start = [array]::FindIndex([string[]]$JsonLines, [Predicate[string]] { param($l) $l.StartsWith('{') })
    $end = [array]::FindLastIndex([string[]]$JsonLines, [Predicate[string]] { param($l) $l.StartsWith('}') })
    if ($start -lt 0 -or $end -lt $start) { throw "no JSON document in report output" }
    $doc = ($JsonLines[$start..$end] -join "`n") | ConvertFrom-Json
    $set = foreach ($g in @($doc.groups)) {
        $paths = foreach ($m in @($g.members)) {
            if ($m.PSObject.Properties['path']) { [string]$m.path } else { '<bytes>' }
        }
        '{0}|{1}' -f $g.full_hash, ((@($paths) | Sort-Object) -join ';')
    }
    return (@($set) | Sort-Object) -join "`n"
}

function Get-Report([string]$Name, [string]$Db) {
    $r = Invoke-Logged $Name $Exe @('report', '--db', $Db, '--format', 'json')
    if ($r.Code -ne 0) { throw "report on $Db exited $($r.Code): $(Get-Decisive $r.Lines)" }
    return Get-GroupSet $r.Out
}

function New-TestTree {
    if (Test-Path $TestRoot) {
        $items = @(Get-ChildItem -Force $TestRoot)
        if ($items.Count -gt 0 -and -not (Test-Path (Join-Path $TestRoot $Marker))) {
            throw "$TestRoot exists and was not made by this script; pass another -TestRoot"
        }
        Remove-Item -Recurse -Force $TestRoot
    }
    $c = Join-Path $TestRoot 'a\b\c'
    New-Item -ItemType Directory -Force -Path $c | Out-Null
    Set-Content -Path (Join-Path $TestRoot $Marker) -Value 'created by scripts\windows\verify.ps1' -Encoding ASCII
    $rng = New-Object System.Random 42
    $dup = New-Object byte[] 65536
    $rng.NextBytes($dup)
    $unique = New-Object byte[] 65536
    $rng.NextBytes($unique)
    [IO.File]::WriteAllBytes((Join-Path $c 'dup1.bin'), $dup)
    [IO.File]::WriteAllBytes((Join-Path $c 'dup2.bin'), $dup)
    [IO.File]::WriteAllBytes((Join-Path $c 'unique.bin'), $unique)
    Set-Content -Path (Join-Path $c 'small.txt') -Value 'small file' -Encoding ASCII
    return $c
}

function Ensure-Exe {
    if (-not (Test-Path $Exe)) {
        $r = Invoke-Logged 'build-dedupe' 'cargo' @('build', '-p', 'dedupe', '--locked')
        if ($r.Code -ne 0) { throw "cargo build -p dedupe exited $($r.Code): $(Get-Decisive $r.Lines)" }
    }
}

function Remove-Db([string]$Db) {
    foreach ($suffix in '', '-wal', '-shm') {
        if (Test-Path "$Db$suffix") { Remove-Item -Force "$Db$suffix" }
    }
}

function Invoke-Scan([string]$Name, [string]$Root, [string]$Db, [string[]]$Extra = @()) {
    $r = Invoke-Logged $Name $Exe (@('scan', $Root, '--db', $Db, '--progress', 'never') + $Extra)
    if ($r.Code -ne 0) { throw "$Name exited $($r.Code): $(Get-Decisive $r.Lines)" }
    return $r
}

function Step-W1 {
    $r = Invoke-Logged 'W1-build' 'cargo' @('build', '--workspace', '--locked')
    $warn = @($r.Lines | Where-Object { $_ -match 'warning: `dedupe-(backend-[a-z]+|win-sys)`' })
    if ($r.Code -ne 0) { Add-Result 'W1' 'FAIL' (Get-Decisive $r.Lines) }
    elseif ($warn.Count -gt 0) { Add-Result 'W1' 'FAIL' $warn[0].Trim() }
    else { Add-Result 'W1' 'PASS' 'cargo build --workspace exit 0, no backend or win-sys warnings' }
}

function Step-W2 {
    $r = Invoke-Logged 'W2-test' 'cargo' @('test', '--workspace', '--all-targets', '--locked', '--no-fail-fast')
    $passed = 0; $failed = 0; $ignored = 0
    foreach ($line in $r.Lines) {
        if ($line -match 'test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored') {
            $passed += [int]$Matches[1]; $failed += [int]$Matches[2]; $ignored += [int]$Matches[3]
        }
    }
    $summary = "$passed passed, $failed failed, $ignored ignored"
    if ($r.Code -ne 0 -or $failed -gt 0) {
        Add-Result 'W2' 'FAIL' "$summary; $(Get-Decisive $r.Lines 'panicked|FAILED|error')"
    } else {
        Add-Result 'W2' 'PASS' $summary
    }
}

function Step-W3 {
    $clippy = Invoke-Logged 'W3-clippy' 'cargo' @('clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings')
    $fmt = Invoke-Logged 'W3-fmt' 'cargo' @('fmt', '--all', '--check')
    if ($clippy.Code -ne 0) { Add-Result 'W3' 'FAIL' ("clippy: " + (Get-Decisive $clippy.Lines 'error')) }
    elseif ($fmt.Code -ne 0) { Add-Result 'W3' 'FAIL' ("fmt: " + (Get-Decisive $fmt.Lines 'Diff in')) }
    else { Add-Result 'W3' 'PASS' 'clippy -D warnings and fmt --check exit 0' }
}

function Step-W4 {
    if (-not $script:Elevated) { Add-Result 'W4' 'SKIP' 'needs an elevated shell'; return }
    $r = Invoke-Logged 'W4-mft_vs_walk' 'cargo' @('test', '-p', 'dedupe-backend-windows', '--test', 'mft_vs_walk', '--locked', '--', '--ignored', '--nocapture')
    if ($r.Code -ne 0) { Add-Result 'W4' 'FAIL' (Get-Decisive $r.Lines 'panicked|FAILED|differ|fallback') }
    else { Add-Result 'W4' 'PASS' (Get-Decisive $r.Lines 'test result') }
}

function Step-W5 {
    if (-not $script:Elevated) { Add-Result 'W5' 'SKIP' 'needs an elevated shell'; return }
    Ensure-Exe
    $c = New-TestTree
    $mftDb = Join-Path $OutDir 'mft.db'
    $walkDb = Join-Path $OutDir 'walk.db'
    Remove-Db $mftDb
    Remove-Db $walkDb
    $mftScan = Invoke-Scan 'W5-scan-mft' $c $mftDb
    Invoke-Scan 'W5-scan-walk' $c $walkDb @('--no-mft') | Out-Null
    $mftSet = Get-Report 'W5-report-mft' $mftDb
    $walkSet = Get-Report 'W5-report-walk' $walkDb
    $lookup = Invoke-Logged 'W5-lookup' $Exe @('lookup', (Join-Path $c 'dup1.bin'), '--db', $mftDb, '--format', 'json')
    # "scan run N finished (enumerator: mft|walker-fallback|...)" names the path that really ran.
    $enumLine = @($mftScan.Lines | Where-Object { $_ -match 'enumerator: ([\w-]+)' }) | Select-Object -Last 1
    $enumerator = if ($enumLine -and $enumLine -match 'enumerator: ([\w-]+)') { $Matches[1] } else { 'unknown' }
    if (-not $mftSet) { Add-Result 'W5' 'FAIL' 'MFT report has no duplicate group'; return }
    if ($mftSet -ne $walkSet) { Add-Result 'W5' 'FAIL' "group sets differ: mft=[$mftSet] walk=[$walkSet]"; return }
    if ($lookup.Code -ne 0) { Add-Result 'W5' 'FAIL' ("lookup: " + (Get-Decisive $lookup.Lines)); return }
    if (-not (@($lookup.Out) -match 'dup2\.bin')) { Add-Result 'W5' 'FAIL' 'lookup of dup1.bin does not list dup2.bin'; return }
    $script:Baseline = $mftSet
    Add-Result 'W5' 'PASS' ("MFT and walker reports match ($(@($mftSet -split "`n").Count) group(s)); lookup finds dup2.bin; first scan enumerator: $enumerator")
}

function Step-W6 {
    if ($null -eq $script:Baseline) { Add-Result 'W6' 'SKIP' 'needs W5 to pass in the same run'; return }
    $c = Join-Path $TestRoot 'a\b\c'
    $nDb = Join-Path $OutDir 'n.db'
    Remove-Db $nDb
    $out = Join-Path $OutDir 'W6-scan.out'
    $err = Join-Path $OutDir 'W6-scan.err'
    $done = Join-Path $OutDir 'W6-scan.done'
    foreach ($f in $out, $err, $done) { if (Test-Path $f) { Remove-Item -Force $f } }
    if ($script:Elevated) {
        # runas runs a .cmd file, which avoids nested quoting; the paths have no spaces.
        $cmdFile = Join-Path $OutDir 'W6-scan.cmd'
        @(
            '@echo off',
            ('"{0}" scan "{1}" --db "{2}" --progress never > "{3}" 2> "{4}"' -f $Exe, $c, $nDb, $out, $err),
            ('echo %ERRORLEVEL% > "{0}"' -f $done)
        ) | Set-Content -Path $cmdFile -Encoding ASCII
        $prev = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $runas = @(& runas.exe /trustlevel:0x20000 $cmdFile 2>&1 | ForEach-Object { "$_" })
        $ErrorActionPreference = $prev
        if ($LASTEXITCODE -ne 0) { Add-Result 'W6' 'SKIP' ("runas failed: " + (Get-Decisive $runas)); return }
        $deadline = (Get-Date).AddMinutes(10)
        while (-not (Test-Path $done) -and (Get-Date) -lt $deadline) { Start-Sleep -Seconds 2 }
        if (-not (Test-Path $done)) { Add-Result 'W6' 'FAIL' 'de-elevated scan did not finish within 10 minutes'; return }
        Start-Sleep -Seconds 1
        $code = [int]((Get-Content $done | Select-Object -First 1).Trim())
        $errLines = @(Get-Content $err)
    } else {
        $r = Invoke-Logged 'W6-scan' $Exe @('scan', $c, '--db', $nDb, '--progress', 'never')
        $code = $r.Code
        $errLines = @($r.Err)
    }
    $notes = @($errLines | Where-Object { $_ -match '^note:' })
    if ($code -ne 0) { Add-Result 'W6' 'FAIL' "scan exited ${code}: $(Get-Decisive $errLines)"; return }
    if ($notes.Count -ne 1) { Add-Result 'W6' 'FAIL' "expected exactly one 'note:' line, got $($notes.Count)"; return }
    $set = Get-Report 'W6-report' $nDb
    if ($set -ne $script:Baseline) { Add-Result 'W6' 'FAIL' "groups differ from W5: [$set]"; return }
    Add-Result 'W6' 'PASS' "de-elevated scan matches W5; $($notes[0].Trim())"
}

function Step-W7 {
    if (-not $script:Elevated) { Add-Result 'W7' 'SKIP' 'needs an elevated shell'; return }
    if ($null -eq $script:Baseline) { Add-Result 'W7' 'SKIP' 'needs W5 to pass in the same run'; return }
    $c = Join-Path $TestRoot 'a\b\c'
    $mftDb = Join-Path $OutDir 'mft.db'
    $runs = @(
        @{ Name = 'W7-1-no-mft'; Extra = @('--no-mft') },
        @{ Name = 'W7-2-mft'; Extra = @() },
        @{ Name = 'W7-3-rebuild-cache'; Extra = @('--rebuild-mft-cache') },
        @{ Name = 'W7-4-cache'; Extra = @() },
        @{ Name = 'W7-5-cache'; Extra = @() }
    )
    foreach ($run in $runs) {
        Invoke-Scan $run.Name $c $mftDb $run.Extra | Out-Null
        $set = Get-Report ($run.Name + '-report') $mftDb
        if ($set -ne $script:Baseline) { Add-Result 'W7' 'FAIL' "$($run.Name): report differs from W5: [$set]"; return }
    }
    Add-Result 'W7' 'PASS' 'five rescans (walker, MFT, cache rebuild, cache twice) keep the W5 report'
}

# Sends Ctrl-C to the console of process $ProcessId from a separate powershell.exe, so this script's
# own console never receives the event. The helper script is written to -OutDir. Returns
# @{ Ok; Error; EventTicks } (EventTicks = UTC ticks taken right after the last event).
function Send-CtrlC([int]$ProcessId, [int]$Count = 1, [int]$GapMs = 200, [string]$Name = 'W8-sendctrlc') {
    $helper = Join-Path $OutDir 'W8-sendctrlc.ps1'
    @'
param([int]$TargetPid, [int]$Count, [int]$GapMs)
$ErrorActionPreference = 'Stop'
Add-Type -Namespace DD -Name K -MemberDefinition @"
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool AttachConsole(uint pid);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool GenerateConsoleCtrlEvent(uint ev, uint group);
"@
[void][DD.K]::FreeConsole()
if (-not [DD.K]::AttachConsole([uint32]$TargetPid)) {
    Write-Output ('AttachConsole failed, win32 error ' + [Runtime.InteropServices.Marshal]::GetLastWin32Error())
    exit 2
}
# Ignore the event in this helper; the target keeps its own handler.
[void][DD.K]::SetConsoleCtrlHandler([IntPtr]::Zero, $true)
for ($i = 0; $i -lt $Count; $i++) {
    if ($i -gt 0) { Start-Sleep -Milliseconds $GapMs }
    if (-not [DD.K]::GenerateConsoleCtrlEvent(0, 0)) {
        Write-Output ('GenerateConsoleCtrlEvent failed, win32 error ' + [Runtime.InteropServices.Marshal]::GetLastWin32Error())
        exit 3
    }
    Write-Output ('EVENT ' + [DateTime]::UtcNow.Ticks)
}
Start-Sleep -Milliseconds 200
[void][DD.K]::FreeConsole()
exit 0
'@ | Set-Content -Path $helper -Encoding ASCII
    $r = Invoke-Logged $Name 'powershell.exe' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $helper, '-TargetPid', "$ProcessId", '-Count', "$Count", '-GapMs', "$GapMs")
    if ($r.Code -ne 0) { return @{ Ok = $false; Error = (Get-Decisive $r.Lines 'failed'); EventTicks = 0L } }
    $ev = @($r.Out | Where-Object { $_ -match '^EVENT \d+$' }) | Select-Object -Last 1
    $ticks = if ($ev) { [long]($ev -replace '^EVENT ', '') } else { 0L }
    return @{ Ok = $true; Error = ''; EventTicks = $ticks }
}

# Starts `dedupe scan` in its own minimized console (so it has a console to signal) and returns the
# dedupe.exe pid. Output goes to <OutDir>\<Name>.out / .err; the exit code to <Name>.done.
# cmd /v:on expands !ERRORLEVEL! after the scan; the .cmd itself returns at once.
function Start-ScanInOwnConsole([string]$Name, [string[]]$ScanArgs) {
    $out = Join-Path $OutDir "$Name.out"
    $err = Join-Path $OutDir "$Name.err"
    $done = Join-Path $OutDir "$Name.done"
    foreach ($f in $out, $err, $done) { if (Test-Path $f) { Remove-Item -Force $f } }
    $cmdFile = Join-Path $OutDir "$Name.cmd"
    $argText = ($ScanArgs | ForEach-Object { '"{0}"' -f $_ }) -join ' '
    @(
        '@echo off',
        ('start "dd-w8" /min cmd /v:on /c ""{0}" {1} > "{2}" 2> "{3}" & echo !ERRORLEVEL!> "{4}""' -f $Exe, $argText, $out, $err, $done)
    ) | Set-Content -Path $cmdFile -Encoding ASCII
    $t0 = (Get-Date).AddSeconds(-1)
    # Not `& cmd /c ... | Out-Null`: the started scan inherits that pipe, so the call blocks until it ends.
    Start-Process -FilePath 'cmd.exe' -ArgumentList @('/c', "`"$cmdFile`"") -WindowStyle Hidden
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Date) -lt $deadline) {
        $p = @(Get-Process dedupe -ErrorAction SilentlyContinue | Where-Object { $_.StartTime -gt $t0 } | Sort-Object StartTime -Descending) | Select-Object -First 1
        if ($p) { return [int]$p.Id }
        Start-Sleep -Milliseconds 50
    }
    throw "$Name : dedupe.exe did not appear within 10 s"
}

function Wait-ScanDone([string]$Done, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $Done) {
            $text = @(Get-Content $Done -ErrorAction SilentlyContinue | Select-Object -First 1)
            if ($text.Count -gt 0 -and "$($text[0])".Trim() -match '^\d+$') { return [int]"$($text[0])".Trim() }
        }
        Start-Sleep -Milliseconds 50
    }
    return $null
}

# One W8 case: start the scan, wait $DelayMs, send $Signals Ctrl-C events $GapMs apart, wait for exit.
# Returns @{ ProcessId; Code; Early; SendError; Latency; Err } (Code $null on timeout; Latency = seconds
# from the last event to the done file, one decimal; Early = scan ended before the signal).
function Invoke-W8Case([string]$Name, [string[]]$ScanArgs, [int]$DelayMs, [int]$Signals, [int]$GapMs = 200) {
    $done = Join-Path $OutDir "$Name.done"
    $res = @{ ProcessId = 0; Code = $null; Early = $false; SendError = ''; Latency = ''; Err = @() }
    $res.ProcessId = Start-ScanInOwnConsole $Name $ScanArgs
    Start-Sleep -Milliseconds $DelayMs
    if (Test-Path $done) { $res.Early = $true; $res.Code = Wait-ScanDone $done 5; return $res }
    $s = Send-CtrlC $res.ProcessId $Signals $GapMs "$Name-sendctrlc"
    if (-not $s.Ok) {
        Stop-Process -Id $res.ProcessId -Force -ErrorAction SilentlyContinue
        $res.SendError = $s.Error
        return $res
    }
    $res.Code = Wait-ScanDone $done 30
    if ($null -eq $res.Code) {
        # Timeout: do not leave a stuck dedupe.exe behind.
        Stop-Process -Id $res.ProcessId -Force -ErrorAction SilentlyContinue
        return $res
    }
    $secs = ((Get-Item $done).LastWriteTimeUtc.Ticks - $s.EventTicks) / 1e7
    $res.Latency = ([Math]::Max(0.0, $secs)).ToString('0.0', [Globalization.CultureInfo]::InvariantCulture)
    $errFile = Join-Path $OutDir "$Name.err"
    if (Test-Path $errFile) { $res.Err = @(Get-Content $errFile) }
    return $res
}

function Step-W8 {
    if (-not $script:Elevated) { Add-Result 'W8' 'SKIP' 'needs an elevated shell'; return }
    if ($null -eq $script:Baseline) { Add-Result 'W8' 'SKIP' 'needs W5 to pass in the same run'; return }
    Ensure-Exe
    $cache = Join-Path $OutDir 'w8cache'
    if (Test-Path $cache) { Remove-Item -Recurse -Force $cache }
    New-Item -ItemType Directory -Force -Path $cache | Out-Null
    $prevCache = $env:DEDUPE_CACHE_DIR
    $env:DEDUPE_CACHE_DIR = $cache
    try {
        # W8a: single Ctrl-C during the full MFT read.
        $db = Join-Path $OutDir 'w8a.db'
        Remove-Db $db
        $a = Invoke-W8Case 'W8a' @('scan', $TestRoot, '--db', $db, '--progress', 'never', '--rebuild-mft-cache') 1500 1
        if ($a.SendError) { Add-Result 'W8' 'SKIP' "Send-CtrlC failed: $($a.SendError)"; return }
        if ($a.Early) { Add-Result 'W8a' 'SKIP' "scan finished before the signal (exit $($a.Code))" }
        elseif ($null -eq $a.Code) { Add-Result 'W8a' 'FAIL' 'scan still running 30 s after Ctrl-C; process killed' }
        elseif ($a.Code -ne 130) { Add-Result 'W8a' 'FAIL' "exit $($a.Code), expected 130; latency $($a.Latency) s" }
        elseif ([double]$a.Latency -gt 10) { Add-Result 'W8a' 'FAIL' "exit 130 but latency $($a.Latency) s, limit 10 s" }
        elseif (-not (@($a.Err) -match 'cancelling')) { Add-Result 'W8a' 'FAIL' "no 'cancelling' line on stderr; latency $($a.Latency) s" }
        elseif (@(Get-ChildItem -Recurse -Force -Filter '*.tmp' $cache -ErrorAction SilentlyContinue).Count -gt 0) { Add-Result 'W8a' 'FAIL' "temp file left in w8cache; latency $($a.Latency) s" }
        else {
            Invoke-Scan 'W8a-resume' $TestRoot $db @('--resume') | Out-Null
            $set = Get-Report 'W8a-report' $db
            if ($set -ne $script:Baseline) { Add-Result 'W8a' 'FAIL' "group set after resume differs from W5: [$set]" }
            else { Add-Result 'W8a' 'PASS' "exit 130, latency $($a.Latency) s, 'cancelling' printed, no .tmp left; resume matches W5" }
        }

        # W8b: second Ctrl-C 200 ms after the first.
        $db = Join-Path $OutDir 'w8b.db'
        Remove-Db $db
        $b = Invoke-W8Case 'W8b' @('scan', $TestRoot, '--db', $db, '--progress', 'never', '--rebuild-mft-cache') 1500 2 200
        if ($b.SendError) { Add-Result 'W8b' 'SKIP' "Send-CtrlC failed: $($b.SendError)" }
        elseif ($b.Early) { Add-Result 'W8b' 'SKIP' "scan finished before the signal (exit $($b.Code))" }
        elseif ($null -eq $b.Code) { Add-Result 'W8b' 'FAIL' 'scan still running 30 s after the second Ctrl-C; process killed' }
        elseif ($b.Code -ne 130) { Add-Result 'W8b' 'FAIL' "exit $($b.Code), expected 130; latency $($b.Latency) s" }
        elseif ([double]$b.Latency -gt 2) { Add-Result 'W8b' 'FAIL' "exit 130 but latency $($b.Latency) s after the second signal, limit 2 s" }
        else {
            Invoke-Scan 'W8b-rescan' $TestRoot $db | Out-Null
            $set = Get-Report 'W8b-report' $db
            # The sweep only removes temp files older than 10 minutes, so the killed scan's own
            # <name>.<pid>.tmp may legitimately remain; any other temp file after the rescan is a failure.
            $tmps = @(Get-ChildItem -Recurse -Force -Filter '*.tmp' $cache -ErrorAction SilentlyContinue)
            $other = @($tmps | Where-Object { $_.Name -notmatch "\.$($b.ProcessId)\.tmp$" })
            if ($set -ne $script:Baseline) { Add-Result 'W8b' 'FAIL' "group set after rescan differs from W5: [$set]" }
            elseif ($other.Count -gt 0) { Add-Result 'W8b' 'FAIL' "temp file left after rescan: $($other[0].Name)" }
            else {
                $own = $tmps.Count - $other.Count
                Add-Result 'W8b' 'PASS' "exit 130, latency $($b.Latency) s after the second signal; rescan matches W5; killed pid's .tmp left: $own (swept only after 10 min)"
            }
        }

        # W8c: Ctrl-C with a cache present (cache load, USN delta). A priming scan writes the cache first.
        $db = Join-Path $OutDir 'w8c.db'
        Remove-Db $db
        Invoke-Scan 'W8c-prime' $TestRoot $db | Out-Null
        $c = Invoke-W8Case 'W8c' @('scan', $TestRoot, '--db', $db, '--progress', 'never') 300 1
        if ($c.SendError) { Add-Result 'W8c' 'SKIP' "Send-CtrlC failed: $($c.SendError)" }
        elseif ($c.Early) { Add-Result 'W8c' 'PASS' "exit $($c.Code); finished before the signal" }
        elseif ($null -eq $c.Code) { Add-Result 'W8c' 'FAIL' 'scan still running 30 s after Ctrl-C; process killed' }
        elseif ($c.Code -eq 130) { Add-Result 'W8c' 'PASS' "exit 130, latency $($c.Latency) s" }
        elseif ($c.Code -eq 0) { Add-Result 'W8c' 'PASS' "exit 0, finished before the signal took effect; latency $($c.Latency) s" }
        else { Add-Result 'W8c' 'FAIL' "exit $($c.Code), expected 130 or 0; latency $($c.Latency) s" }
    } finally {
        if ($null -eq $prevCache) { Remove-Item Env:\DEDUPE_CACHE_DIR -ErrorAction SilentlyContinue } else { $env:DEDUPE_CACHE_DIR = $prevCache }
    }
}

$script:Elevated = Test-Elevated
Write-Host "repo $Repo; target $env:CARGO_TARGET_DIR; out $OutDir; elevated $script:Elevated"
foreach ($id in $Step) {
    $fn = "Step-$($id.ToUpper())"
    if (-not (Get-Command $fn -ErrorAction SilentlyContinue)) { Add-Result $id 'SKIP' 'unknown step'; continue }
    try { & $fn } catch { Add-Result $id.ToUpper() 'FAIL' ("script error: " + $_.Exception.Message) }
}

$commit = '(git not found)'
try {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $head = & git -C $Repo rev-parse --short HEAD 2>$null
    $ErrorActionPreference = $prev
    if ($LASTEXITCODE -eq 0 -and $head) { $commit = "$head" }
} catch { }
$fs = '(unknown)'
try { $fs = (Get-Volume -DriveLetter $TestRoot.Substring(0, 1)).FileSystem } catch { }

$md = New-Object System.Collections.Generic.List[string]
$md.Add('| Step | Result | Evidence |')
$md.Add('|---|---|---|')
foreach ($row in $script:Results) {
    $md.Add(('| {0} | {1} | {2} |' -f $row.Step, $row.Result, ($row.Evidence -replace '\|', '/' -replace '\r?\n', ' ')))
}
$md.Add('')
$md.Add(('Commit tested: {0}  Windows version: {1}  Filesystem: {2}  Script shell elevated: {3} (W6 runs de-elevated)' -f $commit, [Environment]::OSVersion.VersionString, $fs, $script:Elevated))
$md | Set-Content -Path (Join-Path $OutDir 'results.md') -Encoding UTF8
Write-Host ''
$md | ForEach-Object { Write-Host $_ }
Write-Host "results: $(Join-Path $OutDir 'results.md')"

if (@($script:Results | Where-Object { $_.Result -eq 'FAIL' }).Count -gt 0) { exit 1 }
exit 0
