$ErrorActionPreference = 'SilentlyContinue'

$ROOT = if ($PSScriptRoot) { $PSScriptRoot } elseif ($MyInvocation.MyCommand.Path) { Split-Path -Parent $MyInvocation.MyCommand.Path } else { 'C:\RustMiskoLive' }
$LOG_DIR = Join-Path $ROOT 'logs'
$NIGHT_LOG = Join-Path $LOG_DIR 'night_watch.log'
$MONITOR_SCRIPT = Join-Path $ROOT 'monitor_600s.ps1'

$existing = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
    Where-Object {
        ($_.Name -eq 'powershell.exe' -or $_.Name -eq 'pwsh.exe')
            -and $_.CommandLine -match 'night_watch\.ps1'
            -and $_.ProcessId -ne $PID
    }
if ($existing) {
    exit 0
}

if (-not (Test-Path $LOG_DIR)) {
    New-Item -ItemType Directory -Path $LOG_DIR | Out-Null
}

Add-Content $NIGHT_LOG "[$((Get-Date).ToString('s'))] startup root=$ROOT pid=$PID"

while ($true) {
    $cycleStart = Get-Date
    Add-Content $NIGHT_LOG "[$($cycleStart.ToString('s'))] cycle start"

    if (Test-Path $MONITOR_SCRIPT) {
        try {
            & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $MONITOR_SCRIPT | Out-Null
        } catch {
            Add-Content $NIGHT_LOG "[$($cycleStart.ToString('s'))] monitor error: $($_.Exception.Message)"
        }
    } else {
        Add-Content $NIGHT_LOG "[$($cycleStart.ToString('s'))] monitor script missing: $MONITOR_SCRIPT"
    }

    $feedStatus = try {
        Invoke-RestMethod -Uri 'http://127.0.0.1:8081/health' -TimeoutSec 10 | ConvertTo-Json -Compress
    } catch {
        '{"feed":"down"}'
    }
    $execStatus = try {
        Invoke-RestMethod -Uri 'http://127.0.0.1:3030/health' -TimeoutSec 10 | ConvertTo-Json -Compress
    } catch {
        '{"executor":"down"}'
    }

    Add-Content $NIGHT_LOG "[$($cycleStart.ToString('s'))] cycle done | feed=$feedStatus | exec=$execStatus"
    Start-Sleep -Seconds 600
}