param(
    [int]$DurationMinutes = 1440,
    [int]$IntervalSeconds = 300,
    [string]$FeedHubUrl = "http://127.0.0.1:8081",
    [string]$ExecutorUrl = "http://127.0.0.1:3030",
    [string]$LedgerPath = "data/ledger.jsonl",
    [string]$AlertBotLogPath = "logs/alert_bot.log",
    [string]$OutputPath = ""
)

$ErrorActionPreference = "Stop"

function Write-Section([string]$Path, [string]$Text) {
    Add-Content -Path $Path -Value $Text
    Add-Content -Path $Path -Value ""
}

function Invoke-JsonGet([string]$Url) {
    try {
        return Invoke-RestMethod -Uri $Url -Method Get -TimeoutSec 15
    } catch {
        return $null
    }
}

function Parse-LedgerTimestamp($Value) {
    if ($null -eq $Value) {
        return $null
    }

    $parsed = [datetimeoffset]::MinValue
    if ([datetimeoffset]::TryParse([string]$Value, [ref]$parsed)) {
        return $parsed
    }

    return $null
}

function Get-LedgerRows([string]$Path, [datetimeoffset]$StartTs) {
    if (-not (Test-Path $Path)) {
        return @()
    }

    $rows = New-Object System.Collections.Generic.List[object]
    foreach ($line in Get-Content $Path) {
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        try {
            $row = $line | ConvertFrom-Json
            if (-not $row.ts) {
                continue
            }
            $rowTs = Parse-LedgerTimestamp $row.ts
            if ($null -eq $rowTs) {
                continue
            }
            if ($rowTs -ge $StartTs) {
                [void]$rows.Add($row)
            }
        } catch {
        }
    }
    return $rows
}

function Get-LedgerSummary([object[]]$Rows) {
    $summary = [ordered]@{
        PLACED = 0
        ON_CHAIN_ACCEPTED = 0
        BET_FAILED = 0
        WON = 0
        LOST = 0
        CANCELED = 0
    }
    $failReasons = @{}

    foreach ($row in $Rows) {
        $event = [string]$row.event
        if ($summary.Contains($event)) {
            $summary[$event] += 1
        }
        if ($event -eq "BET_FAILED" -and $row.reason_code) {
            $key = [string]$row.reason_code
            if (-not $failReasons.ContainsKey($key)) {
                $failReasons[$key] = 0
            }
            $failReasons[$key] += 1
        }
    }

    return [pscustomobject]@{
        Counts = $summary
        FailReasons = $failReasons
    }
}

function Format-FailReasons($FailReasons) {
    if (-not $FailReasons -or $FailReasons.Count -eq 0) {
        return "none"
    }

    return (($FailReasons.GetEnumerator() |
        Sort-Object Value -Descending |
        Select-Object -First 5 |
        ForEach-Object { "{0}={1}" -f $_.Key, $_.Value }) -join "; ")
}

function Get-RecentLedgerTail([object[]]$Rows, [datetimeoffset]$SinceTs) {
    return $Rows |
        Where-Object {
            $parsed = Parse-LedgerTimestamp $_.ts
            $null -ne $parsed -and $parsed -gt $SinceTs
        } |
        Sort-Object {
            $parsed = Parse-LedgerTimestamp $_.ts
            if ($null -eq $parsed) { [datetimeoffset]::MinValue } else { $parsed }
        } |
        Select-Object -Last 8
}

function Get-AlertBotTail([string]$Path) {
    if (-not (Test-Path $Path)) {
        return @()
    }

    $patterns = 'AUTO-BET', 'BET_FAILED', 'UNDERDOG-ONLY', 'market source', 'SCORE-CONFIRM', 'SAFE MODE'
    $tail = Get-Content $Path -Tail 250 | Where-Object {
        $line = $_
        $patterns | Where-Object { $line -match $_ }
    }
    return $tail | Select-Object -Last 12
}

$workspaceRoot = Split-Path -Parent $PSScriptRoot
$tempDir = Join-Path $workspaceRoot "temp"
if (-not (Test-Path $tempDir)) {
    New-Item -ItemType Directory -Path $tempDir | Out-Null
}

if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $stamp = Get-Date -Format "yyyyMMdd_HHmmss"
    $OutputPath = Join-Path $tempDir ("burnin_watch_24h_{0}.txt" -f $stamp)
}

$startTs = [datetimeoffset]::UtcNow
$endTs = $startTs.AddMinutes($DurationMinutes)
$lastLedgerTs = $startTs.AddSeconds(-1)

Set-Content -Path $OutputPath -Value @(
    "# 24h Burn-in Watch",
    "",
    "start_utc=$($startTs.ToString('o'))",
    "end_utc=$($endTs.ToString('o'))",
    "interval_seconds=$IntervalSeconds",
    "feed_hub_url=$FeedHubUrl",
    "executor_url=$ExecutorUrl"
)

while ([datetimeoffset]::UtcNow -lt $endTs) {
    $now = [datetimeoffset]::UtcNow
    $health = Invoke-JsonGet "$FeedHubUrl/health"
    $state = Invoke-JsonGet "$FeedHubUrl/state"
    $executorHealth = Invoke-JsonGet "$ExecutorUrl/health"
    $balance = Invoke-JsonGet "$ExecutorUrl/balance"
    $myBets = Invoke-JsonGet "$ExecutorUrl/my-bets"

    $ledgerRows = Get-LedgerRows -Path $LedgerPath -StartTs $startTs
    $ledgerSummary = Get-LedgerSummary -Rows $ledgerRows
    $recentLedger = Get-RecentLedgerTail -Rows $ledgerRows -SinceTs $lastLedgerTs
    if ($recentLedger.Count -gt 0) {
        $parsedLastLedgerTs = Parse-LedgerTimestamp (($recentLedger | Select-Object -Last 1).ts)
        if ($null -ne $parsedLastLedgerTs) {
            $lastLedgerTs = $parsedLastLedgerTs
        }
    }

    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add(("## Sample {0}" -f $now.ToString('o')))
    if ($health) {
        $lines.Add(("feed_health: gql_age_ms={0} ws_age_ms={1} ws_event_age_ms={2}" -f $health.gql_age_ms, $health.ws_age_ms, $health.ws_event_age_ms))
    } else {
        $lines.Add("feed_health: unavailable")
    }
    if ($state) {
        $lines.Add(("feed_state: live_items={0} fused_ready={1} connections={2}" -f $state.live_items, $state.fused_ready, $state.connections))
    } else {
        $lines.Add("feed_state: unavailable")
    }
    if ($executorHealth) {
        $lines.Add(("executor_health: balance={0} active_bets={1} wallet={2}" -f $executorHealth.balance, $executorHealth.active_bets, $executorHealth.wallet))
    } else {
        $lines.Add("executor_health: unavailable")
    }
    if ($balance) {
        $lines.Add(("executor_balance: betToken={0} native={1}" -f $balance.betToken, $balance.native))
    }
    if ($myBets) {
        $lines.Add(("my_bets: total={0} pending={1} lost={2} alreadyPaid={3} claimable={4}" -f $myBets.total, $myBets.pending, $myBets.lost, $myBets.alreadyPaid, $myBets.claimable))
    } else {
        $lines.Add("my_bets: unavailable")
    }

    $counts = $ledgerSummary.Counts
    $lines.Add(("ledger_since_start: placed={0} accepted={1} failed={2} won={3} lost={4} canceled={5}" -f $counts.PLACED, $counts.ON_CHAIN_ACCEPTED, $counts.BET_FAILED, $counts.WON, $counts.LOST, $counts.CANCELED))
    $lines.Add(("fail_reasons: {0}" -f (Format-FailReasons -FailReasons $ledgerSummary.FailReasons)))

    if ($recentLedger.Count -gt 0) {
        $lines.Add("recent_ledger:")
        foreach ($row in $recentLedger) {
            $lines.Add(("  - {0} | {1} | {2} | {3} | odds={4} | amount={5}" -f $row.ts, $row.event, $row.match_key, $row.path, $row.odds, $row.amount_usd))
        }
    }

    $alertTail = Get-AlertBotTail -Path $AlertBotLogPath
    if ($alertTail.Count -gt 0) {
        $lines.Add("alert_bot_tail:")
        foreach ($line in $alertTail) {
            $lines.Add(("  - {0}" -f $line))
        }
    }

    Write-Section -Path $OutputPath -Text ($lines -join [Environment]::NewLine)
    Start-Sleep -Seconds $IntervalSeconds
}

$finalRows = Get-LedgerRows -Path $LedgerPath -StartTs $startTs
$finalSummary = Get-LedgerSummary -Rows $finalRows
$finalLines = New-Object System.Collections.Generic.List[string]
$finalLines.Add("# Final Summary")
$finalLines.Add(("completed_utc={0}" -f ([datetimeoffset]::UtcNow.ToString('o'))))
$finalLines.Add(("ledger: placed={0} accepted={1} failed={2} won={3} lost={4} canceled={5}" -f $finalSummary.Counts.PLACED, $finalSummary.Counts.ON_CHAIN_ACCEPTED, $finalSummary.Counts.BET_FAILED, $finalSummary.Counts.WON, $finalSummary.Counts.LOST, $finalSummary.Counts.CANCELED))
$finalLines.Add(("fail_reasons: {0}" -f (Format-FailReasons -FailReasons $finalSummary.FailReasons)))

$fromDate = $startTs.UtcDateTime.ToString("yyyy-MM-dd")
$toDate = ([datetimeoffset]::UtcNow).UtcDateTime.ToString("yyyy-MM-dd")
$edgeReport = & (Join-Path $PSScriptRoot "daily_edge_report.ps1") -FromDate $fromDate -ToDate $toDate
$finalLines.Add("")
$finalLines.Add("daily_edge_report:")
$finalLines.Add($edgeReport)

Write-Section -Path $OutputPath -Text ($finalLines -join [Environment]::NewLine)
Write-Output $OutputPath