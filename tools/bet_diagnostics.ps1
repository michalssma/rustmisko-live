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

# ── 2. minOdds rejects specifically ──
$minOddsRejects = @($failed | Where-Object { $_.is_minodds_reject -eq $true })
$dedupRejects = @($failed | Where-Object { $_.is_dedup -eq $true })
$otherRejects = @($failed | Where-Object { $_.is_minodds_reject -ne $true -and $_.is_dedup -ne $true })

Write-Host ''
Write-Host '2) FAILURE BREAKDOWN' -ForegroundColor Yellow
Write-Host "   minOdds rejects:    $($minOddsRejects.Count)"
Write-Host "   Dedup (409):        $($dedupRejects.Count)"
Write-Host "   Other (paused/etc): $($otherRejects.Count)"
if ($totalAttempts -gt 0) {
    $minOddsRate = [math]::Round($minOddsRejects.Count / $totalAttempts * 100, 1)
    Write-Host "   minOdds reject rate: ${minOddsRate}% of all attempts"
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
elseif ($minOddsRejects.Count -eq 0) {
    Write-Host '   NO minOdds REJECTS - odds freshness is not an issue' -ForegroundColor Green
    Write-Host '   WS primary switch is P2 optimization, not P0'
}
else {
    $minOddsRatePct = [math]::Round($minOddsRejects.Count / $totalAttempts * 100, 1)
    if ($minOddsRatePct -ge 20) {
        # Check latency correlation
        if ($placedWithTiming.Count -ge 3 -and $minOddsWithTiming.Count -ge 3) {
            $placedP50 = Get-Percentile @($placedWithTiming | ForEach-Object { $_.pipeline_ms }) 50
            $minOddsP50 = Get-Percentile @($minOddsWithTiming | ForEach-Object { $_.pipeline_ms }) 50
            if ($minOddsP50 -gt ($placedP50 * 1.5)) {
                Write-Host "   STALE ODDS CONFIRMED - minOdds rejects $minOddsRatePct pct, latency correlated" -ForegroundColor Red
                Write-Host "   minOdds p50=$($minOddsP50)ms >> placed p50=$($placedP50)ms"
                Write-Host '   -> WS is MUST-HAVE. Implement Phase 2 guardrail.'
            }
            else {
                Write-Host "   HIGH REJECT RATE $minOddsRatePct pct but latency NOT correlated" -ForegroundColor Yellow
                Write-Host "   minOdds p50=$($minOddsP50)ms ~= placed p50=$($placedP50)ms"
                Write-Host '   -> Likely MIN_ODDS_FACTOR too tight. Try 0.95 instead of 0.97.'
            }
        }
        else {
            Write-Host "   HIGH REJECT RATE $minOddsRatePct pct - need more timing data for correlation" -ForegroundColor Yellow
        }
    }
    else {
        Write-Host "   LOW REJECT RATE $minOddsRatePct pct - WS is nice-to-have P2" -ForegroundColor Green
    }
}

# ── 7. Per-sport breakdown ──
Write-Host ''
Write-Host '6) PER-SPORT BREAKDOWN' -ForegroundColor Yellow
$allBets = @($all | Where-Object { $_.event -eq 'PLACED' -or $_.event -eq 'BET_FAILED' })
$sports = @{}
foreach ($b in $allBets) {
    $sport = if ($b.match_key) { ($b.match_key -split '::')[0] } else { 'unknown' }
    if (-not $sports.ContainsKey($sport)) { $sports[$sport] = @{placed=0; failed=0; minodds=0} }
    if ($b.event -eq 'PLACED') { $sports[$sport].placed++ }
    else { 
        $sports[$sport].failed++
        if ($b.is_minodds_reject -eq $true) { $sports[$sport].minodds++ }
    }
}
foreach ($kv in $sports.GetEnumerator() | Sort-Object { $_.Value.placed + $_.Value.failed } -Descending) {
    $total = $kv.Value.placed + $kv.Value.failed
    $rate = if ($total -gt 0) { [math]::Round($kv.Value.failed / $total * 100, 0) } else { 0 }
    Write-Host "   $($kv.Key): $($kv.Value.placed) placed, $($kv.Value.failed) failed ($rate%), $($kv.Value.minodds) minOdds"
}

Write-Host ''
Write-Host '========================================' -ForegroundColor Cyan
Write-Host ''
