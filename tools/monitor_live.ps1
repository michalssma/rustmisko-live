param([int]$Minutes = 30, [int]$IntervalSec = 60)

$out    = "C:\RustMiskoLive\logs\monitor_30m.txt"
$ledger = "C:\RustMiskoLive\data\ledger.jsonl"
$pnlF   = "C:\RustMiskoLive\data\daily_pnl.json"
$logDir = "C:\RustMiskoLive\logs"
$pid_target = 38252

$snap0  = (Get-Content $ledger | Measure-Object -Line).Lines
$t0     = Get-Date
$endT   = $t0.AddMinutes($Minutes)

$header = @"
=== MONITOR START $($t0.ToString('HH:mm:ss')) | baseline=$snap0 lines | PID=$pid_target ===
"@
Set-Content $out $header

$tick = 0
while ((Get-Date) -lt $endT) {
    Start-Sleep -Seconds $IntervalSec
    $tick++
    $now   = Get-Date
    $pid_ok = (Get-Process -Id $pid_target -EA SilentlyContinue) -ne $null

    $ledger_n = (Get-Content $ledger -EA SilentlyContinue | Measure-Object -Line).Lines
    $delta    = $ledger_n - $snap0

    $today   = $now.ToString("yyyy-MM-dd")
    $logFile = "$logDir\$today.jsonl"
    $logSz   = if (Test-Path $logFile) { (Get-Item $logFile).Length } else { 0 }

    $pnl     = if (Test-Path $pnlF)   { Get-Content $pnlF } else { "{}" }

    # Last 5 ledger lines
    $last5   = (Get-Content $ledger -EA SilentlyContinue | Select-Object -Last 5) -join "`n"

    # Detect new PLACED/WON/LOST since baseline
    $all = Get-Content $ledger -EA SilentlyContinue
    $newLines = $all | Select-Object -Last $delta
    $placed = ($newLines | Where-Object { $_ -match '"event":"PLACED"' }).Count
    $won    = ($newLines | Where-Object { $_ -match '"event":"WON"' }).Count
    $lost   = ($newLines | Where-Object { $_ -match '"event":"LOST"' }).Count
    $dup_check = ($newLines | Where-Object { $_ -match '"event":"WON"|"event":"LOST"' } |
                  ForEach-Object { ($_ | ConvertFrom-Json -EA SilentlyContinue).bet_id } |
                  Group-Object | Where-Object { $_.Count -gt 1 }).Count

    $block = @"
--- TICK $tick @ $($now.ToString('HH:mm:ss')) ---
PID $pid_target alive: $pid_ok | Ledger: $ledger_n (+$delta) | Log: $logSz bytes
New since start: PLACED=$placed WON=$won LOST=$lost | DUPLICATES=$dup_check
Daily PnL: $pnl
Last 5 ledger:
$last5

"@
    Add-Content $out $block
}

$final_n = (Get-Content $ledger -EA SilentlyContinue | Measure-Object -Line).Lines
Add-Content $out "=== MONITOR END $((Get-Date).ToString('HH:mm:ss')) | total_new=$(${final_n} - $snap0) ==="
