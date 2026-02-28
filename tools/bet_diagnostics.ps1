<#
.SYNOPSIS
  Bet failure diagnostics — analyzes ledger.jsonl for minOdds reject rate,
  pipeline latency correlation, and failure reasons.
  
  Run after 24h of instrumented data collection.
  Equivalent of GPT's jq queries, adapted for Windows PowerShell.

.USAGE
  powershell -NoProfile -File tools\bet_diagnostics.ps1
#>

$ErrorActionPreference = 'SilentlyContinue'
$ledgerPath = Join-Path $PSScriptRoot '..\data\ledger.jsonl'

if (-not (Test-Path $ledgerPath)) {
    Write-Host "ERROR: $ledgerPath not found" -ForegroundColor Red
    exit 1
}

$lines = Get-Content $ledgerPath
$all = @()
foreach ($line in $lines) {
    try { $all += ($line | ConvertFrom-Json) } catch {}
}

Write-Host ''
Write-Host '========================================' -ForegroundColor Cyan
Write-Host ' BET DIAGNOSTICS REPORT' -ForegroundColor Cyan
$reportDate = Get-Date -Format 'yyyy-MM-dd HH:mm'
Write-Host " $reportDate" -ForegroundColor Cyan
Write-Host '========================================' -ForegroundColor Cyan
Write-Host ''

# ── 1. Overall reject rate ──
$placed = @($all | Where-Object { $_.event -eq 'PLACED' })
$failed = @($all | Where-Object { $_.event -eq 'BET_FAILED' })
$totalAttempts = $placed.Count + $failed.Count

Write-Host "1) CELKOVY REJECT RATE" -ForegroundColor Yellow
Write-Host "   PLACED:     $($placed.Count)"
Write-Host "   BET_FAILED: $($failed.Count)"
if ($totalAttempts -gt 0) {
    $rejectRate = [math]::Round($failed.Count / $totalAttempts * 100, 1)
    Write-Host "   Reject rate: ${rejectRate}% ($($failed.Count)/$totalAttempts)"
} else {
    Write-Host "   No bet attempts found"
}

# ── 2. Failure breakdown by reason_code ──
$conditionStateRejects = @($failed | Where-Object { $_.is_condition_state_reject -eq $true -or $_.reason_code -eq 'ConditionNotRunning' })
$minOddsRejects = @($failed | Where-Object { $_.is_minodds_reject -eq $true -or $_.reason_code -eq 'MinOddsReject' })
$dedupRejects = @($failed | Where-Object { $_.is_dedup -eq $true -or $_.reason_code -eq 'Dedup' })
$fatalRejects = @($failed | Where-Object { $_.reason_code -eq 'Fatal' })
# Legacy entries without reason_code: classify by error string
$legacyUnclassified = @($failed | Where-Object { $_.reason_code -eq $null -and $_.is_condition_state_reject -eq $null })
foreach ($entry in $legacyUnclassified) {
    $errLow = ($entry.error + '').ToLower()
    if ($errLow -match 'not active|paused|not exist') { $conditionStateRejects += $entry }
    elseif ($errLow -match 'min odds|minodds|real odds') { $minOddsRejects += $entry }
    elseif ($errLow -match 'dedup|already bet') { $dedupRejects += $entry }
}

Write-Host ''
Write-Host '2) FAILURE BREAKDOWN (by reason_code)' -ForegroundColor Yellow
Write-Host "   ConditionNotRunning: $($conditionStateRejects.Count)  <-- PRIMARY DRIVER"
Write-Host "   MinOddsReject:      $($minOddsRejects.Count)"
Write-Host "   Dedup (409):        $($dedupRejects.Count)"
Write-Host "   Fatal:              $($fatalRejects.Count)"
if ($totalAttempts -gt 0) {
    $condRate = [math]::Round($conditionStateRejects.Count / $totalAttempts * 100, 1)
    $minOddsRate = [math]::Round($minOddsRejects.Count / $totalAttempts * 100, 1)
    Write-Host "   condition_not_active_rate: $condRate%"
    Write-Host "   minOdds_reject_rate: $minOddsRate%"
}

# ── 3. Pipeline latency: PLACED vs minOdds rejects ──
function Get-Percentile {
    param([double[]]$Values, [int]$Pct)
    if ($Values.Count -eq 0) { return 'N/A' }
    $sorted = $Values | Sort-Object
    $idx = [math]::Floor($sorted.Count * $Pct / 100)
    if ($idx -ge $sorted.Count) { $idx = $sorted.Count - 1 }
    return $sorted[$idx]
}

# Only use entries WITH timing data (post-instrumentation)
$placedWithTiming = @($placed | Where-Object { $_.pipeline_ms -ne $null })
$minOddsWithTiming = @($minOddsRejects | Where-Object { $_.pipeline_ms -ne $null })

Write-Host ''
Write-Host '3) PIPELINE LATENCY - PLACED vs minOdds rejects' -ForegroundColor Yellow
Write-Host "   (Only entries with timing instrumentation)"
Write-Host ""
if ($placedWithTiming.Count -gt 0) {
    $placedPipelines = @($placedWithTiming | ForEach-Object { $_.pipeline_ms })
    Write-Host "   PLACED (n=$($placedWithTiming.Count)):"
    Write-Host "     p50: $(Get-Percentile $placedPipelines 50)ms"
    Write-Host "     p95: $(Get-Percentile $placedPipelines 95)ms"
    Write-Host "     max: $($placedPipelines | Measure-Object -Maximum | Select-Object -ExpandProperty Maximum)ms"
} else {
    Write-Host "   PLACED: No instrumented entries yet" -ForegroundColor DarkGray
}

if ($minOddsWithTiming.Count -gt 0) {
    $minOddsPipelines = @($minOddsWithTiming | ForEach-Object { $_.pipeline_ms })
    Write-Host "   minOdds rejects (n=$($minOddsWithTiming.Count)):"
    Write-Host "     p50: $(Get-Percentile $minOddsPipelines 50)ms"
    Write-Host "     p95: $(Get-Percentile $minOddsPipelines 95)ms"
    Write-Host "     max: $($minOddsPipelines | Measure-Object -Maximum | Select-Object -ExpandProperty Maximum)ms"
} else {
    Write-Host "   minOdds rejects: No instrumented entries yet" -ForegroundColor DarkGray
}

# ── 4. RTT distribution ──
$allWithTiming = @($all | Where-Object { $_.rtt_ms -ne $null -and ($_.event -eq 'PLACED' -or $_.event -eq 'BET_FAILED') })

Write-Host ''
Write-Host '4) EXECUTOR RTT (ms)' -ForegroundColor Yellow
if ($allWithTiming.Count -gt 0) {
    $rtts = @($allWithTiming | ForEach-Object { $_.rtt_ms })
    Write-Host "   n=$($rtts.Count)"
    Write-Host "   p50: $(Get-Percentile $rtts 50)ms"
    Write-Host "   p95: $(Get-Percentile $rtts 95)ms"
    Write-Host "   max: $($rtts | Measure-Object -Maximum | Select-Object -ExpandProperty Maximum)ms"
} else {
    Write-Host "   No instrumented entries yet" -ForegroundColor DarkGray
}

# ── 5. Top failure reasons ──
Write-Host ''
Write-Host '5) TOP FAILURE REASONS' -ForegroundColor Yellow
if ($failed.Count -gt 0) {
    $failed | ForEach-Object { $_.error } | Group-Object | Sort-Object Count -Descending |
        Select-Object -First 10 | ForEach-Object {
            Write-Host "   $($_.Count)x $($_.Name)"
        }
} else {
    Write-Host "   No failures recorded" -ForegroundColor DarkGray
}

# ── 6. VERDICT ──
Write-Host ''
Write-Host '========================================' -ForegroundColor Cyan
Write-Host ' VERDICT' -ForegroundColor Cyan
Write-Host '========================================' -ForegroundColor Cyan

if ($totalAttempts -lt 10) {
    Write-Host '   INSUFFICIENT DATA - need >= 10 bet attempts with timing' -ForegroundColor DarkYellow
    Write-Host "   Currently: $totalAttempts total, $($placedWithTiming.Count + $minOddsWithTiming.Count) with timing"
    Write-Host "   Wait for more bets (daily limit resets at midnight UTC)"
}
else {
    # === Track A: Condition state rejects (primary driver) ===
    $condRatePct = if ($totalAttempts -gt 0) { [math]::Round($conditionStateRejects.Count / $totalAttempts * 100, 1) } else { 0 }
    if ($conditionStateRejects.Count -gt 0) {
        Write-Host "   TRACK A (ConditionNotRunning): $condRatePct% of attempts" -ForegroundColor Yellow
        if ($condRatePct -ge 10) {
            Write-Host '     HIGH condition-state fail rate.' -ForegroundColor Red
            Write-Host '     -> P0: WS state feed or condition-state gating needed' -ForegroundColor Red
        }
        else {
            Write-Host '     Low condition-state fail rate. Acceptable.' -ForegroundColor Green
        }
    }
    else {
        Write-Host '   TRACK A: Zero ConditionNotRunning rejects' -ForegroundColor Green
    }

    # === Track B: minOdds rejects ===
    $minOddsRatePct = if ($totalAttempts -gt 0) { [math]::Round($minOddsRejects.Count / $totalAttempts * 100, 1) } else { 0 }
    if ($minOddsRejects.Count -gt 0) {
        Write-Host "   TRACK B (MinOddsReject): $minOddsRatePct% of attempts" -ForegroundColor Yellow
        if ($minOddsRatePct -ge 20) {
            if ($placedWithTiming.Count -ge 3 -and $minOddsWithTiming.Count -ge 3) {
                $placedP50 = Get-Percentile @($placedWithTiming | ForEach-Object { $_.pipeline_ms }) 50
                $minOddsP50 = Get-Percentile @($minOddsWithTiming | ForEach-Object { $_.pipeline_ms }) 50
                if ($minOddsP50 -gt ($placedP50 * 1.5)) {
                    Write-Host "     Latency correlated: p50 $($minOddsP50)ms >> $($placedP50)ms" -ForegroundColor Red
                    Write-Host '     -> WS odds feed MUST-HAVE' -ForegroundColor Red
                }
                else {
                    Write-Host "     NO latency correlation: p50 $($minOddsP50)ms ~= $($placedP50)ms" -ForegroundColor Yellow
                    Write-Host '     -> Relax MIN_ODDS_FACTOR (0.97 -> 0.95)' -ForegroundColor Yellow
                }
            }
            else {
                Write-Host '     Need more timing data for latency correlation' -ForegroundColor Yellow
            }
        }
        else {
            Write-Host '     Low minOdds rate. WS odds feed is P2.' -ForegroundColor Green
        }
    }
    else {
        Write-Host '   TRACK B: Zero MinOddsReject. Odds freshness OK.' -ForegroundColor Green
    }

    # === WS Decision ===
    Write-Host ''
    if ($condRatePct -ge 10) {
        Write-Host '   WS VERDICT: MUST-HAVE as STATE FEED (Active/Paused awareness)' -ForegroundColor Red
    }
    elseif ($minOddsRatePct -ge 20) {
        Write-Host '   WS VERDICT: MUST-HAVE as ODDS FEED (stale odds mitigation)' -ForegroundColor Red
    }
    else {
        Write-Host '   WS VERDICT: Nice-to-have P2 optimization' -ForegroundColor Green
    }
}

# ── 7. Per-sport breakdown ──
Write-Host ''
Write-Host '6) PER-SPORT BREAKDOWN' -ForegroundColor Yellow
$allBets = @($all | Where-Object { $_.event -eq 'PLACED' -or $_.event -eq 'BET_FAILED' })
$sports = @{}
foreach ($b in $allBets) {
    $sport = if ($b.match_key) { ($b.match_key -split '::')[0] } else { 'unknown' }
    if (-not $sports.ContainsKey($sport)) { $sports[$sport] = @{placed=0; failed=0; cond_state=0; minodds=0} }
    if ($b.event -eq 'PLACED') { $sports[$sport].placed++ }
    else { 
        $sports[$sport].failed++
        # Classify: check new fields first, fallback to error string for legacy
        $isCond = $b.is_condition_state_reject -eq $true -or $b.reason_code -eq 'ConditionNotRunning'
        $isMinOdds = $b.is_minodds_reject -eq $true -or $b.reason_code -eq 'MinOddsReject'
        if (-not $isCond -and -not $isMinOdds -and $b.reason_code -eq $null) {
            $errLow = ($b.error + '').ToLower()
            if ($errLow -match 'not active|paused|not exist') { $isCond = $true }
            elseif ($errLow -match 'min odds|minodds|real odds') { $isMinOdds = $true }
        }
        if ($isCond) { $sports[$sport].cond_state++ }
        if ($isMinOdds) { $sports[$sport].minodds++ }
    }
}
foreach ($kv in $sports.GetEnumerator() | Sort-Object { $_.Value.placed + $_.Value.failed } -Descending) {
    $total = $kv.Value.placed + $kv.Value.failed
    $rate = if ($total -gt 0) { [math]::Round($kv.Value.failed / $total * 100, 0) } else { 0 }
    Write-Host "   $($kv.Key): $($kv.Value.placed) placed, $($kv.Value.failed) failed ($rate%), $($kv.Value.cond_state) CondNotActive, $($kv.Value.minodds) minOdds"
}

# ── 8. Condition age analysis ──
$withAge = @($allBets | Where-Object { $_.condition_age_ms -ne $null })
$placedWithAge = @($placed | Where-Object { $_.condition_age_ms -ne $null })
$failedWithAge = @($conditionStateRejects | Where-Object { $_.condition_age_ms -ne $null })

Write-Host ''
Write-Host '7) CONDITION AGE AT BET TIME (ms)' -ForegroundColor Yellow
Write-Host '   (How stale was the last Active sighting from GQL poll)'
if ($placedWithAge.Count -gt 0) {
    $ages = @($placedWithAge | ForEach-Object { $_.condition_age_ms })
    Write-Host "   PLACED (n=$($placedWithAge.Count)): p50=$(Get-Percentile $ages 50)ms  p95=$(Get-Percentile $ages 95)ms"
}
else {
    Write-Host '   PLACED: No entries with condition_age_ms yet' -ForegroundColor DarkGray
}
if ($failedWithAge.Count -gt 0) {
    $ages = @($failedWithAge | ForEach-Object { $_.condition_age_ms })
    Write-Host "   CondNotActive (n=$($failedWithAge.Count)): p50=$(Get-Percentile $ages 50)ms  p95=$(Get-Percentile $ages 95)ms"
}
else {
    Write-Host '   CondNotActive: No entries with condition_age_ms yet' -ForegroundColor DarkGray
}
if ($placedWithAge.Count -ge 3 -and $failedWithAge.Count -ge 3) {
    $placedAge50 = Get-Percentile @($placedWithAge | ForEach-Object { $_.condition_age_ms }) 50
    $failedAge50 = Get-Percentile @($failedWithAge | ForEach-Object { $_.condition_age_ms }) 50
    if ($failedAge50 -gt ($placedAge50 * 1.5)) {
        Write-Host "   CORRELATION: failed bets have STALER conditions (p50: $($failedAge50)ms vs $($placedAge50)ms)" -ForegroundColor Red
        Write-Host '   -> WS state feed is HIGH PRIORITY (reduces staleness)' -ForegroundColor Red
    }
    else {
        Write-Host "   NO AGE CORRELATION: p50 failed=$($failedAge50)ms vs placed=$($placedAge50)ms" -ForegroundColor Green
        Write-Host '   -> State changes happen faster than any poll can catch' -ForegroundColor Green
    }
}

Write-Host ''
Write-Host '========================================' -ForegroundColor Cyan
Write-Host ''
