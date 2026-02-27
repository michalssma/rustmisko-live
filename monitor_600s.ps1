$log = 'C:\RustMiskoLive\logs\alert_bot.log'
$pending = 'C:\RustMiskoLive\data\pending_claims.txt'
$hist = 'C:\RustMiskoLive\data\bet_history.txt'
$out = 'C:\RustMiskoLive\logs\monitor_600s_report.txt'

$startTime = Get-Date
if (Test-Path $log) { $startLogSize = (Get-Item $log).Length } else { $startLogSize = 0 }
if (Test-Path $pending) { $startPendingCount = (Get-Content $pending).Count } else { $startPendingCount = 0 }
if (Test-Path $hist) { $startHistCount = (Get-Content $hist).Count } else { $startHistCount = 0 }

$header = @(
  "MONITOR_START=$($startTime.ToString('o'))",
  "START_LOG_SIZE=$startLogSize",
  "START_PENDING=$startPendingCount",
  "START_HISTORY=$startHistCount"
)
$header | Set-Content $out -Encoding UTF8

Start-Sleep -Seconds 600

$endTime = Get-Date
$new = ''
if (Test-Path $log) {
  $raw = Get-Content $log -Raw
  if ($raw.Length -gt $startLogSize) {
    $new = $raw.Substring([int]$startLogSize)
  }
}

function Count-Pattern([string]$text, [string]$pattern) {
  return ([regex]::Matches($text, $pattern, [System.Text.RegularExpressions.RegexOptions]::IgnoreCase)).Count
}

$m_auto_attempt = Count-Pattern $new 'AUTO-BET(?: ODDS)? #\d+:'
$m_auto_odds_attempt = Count-Pattern $new 'AUTO-BET ODDS #\d+:'
$m_auto_fail = Count-Pattern $new 'AUTO-BET(?: ODDS)? #\d+ FAILED'
$m_auto_odds_fail = Count-Pattern $new 'AUTO-BET ODDS #\d+ FAILED'
$m_auto_rejected = Count-Pattern $new 'AUTO-BET(?: ODDS)? #\d+ REJECTED'
$m_cond_not_active = Count-Pattern $new 'Condition is not active'
$m_real_lt_min = Count-Pattern $new 'Real odds less than min odds'
$m_dedup = Count-Pattern $new 'DEDUP'
$m_retry = Count-Pattern $new 'retry \d+/\d+|condition paused'
$m_no_azuro = Count-Pattern $new 'NO AZURO ODDS'
$m_not_actionable = Count-Pattern $new 'score not actionable'
$m_parsed_yes = Count-Pattern $new 'Parsed YES reply'

$newPending = @()
if (Test-Path $pending) {
  $allP = Get-Content $pending
  if ($allP.Count -gt $startPendingCount) {
    $newPending = $allP[$startPendingCount..($allP.Count-1)]
  }
}

$newHist = @()
if (Test-Path $hist) {
  $allH = Get-Content $hist
  if ($allH.Count -gt $startHistCount) {
    $newHist = $allH[$startHistCount..($allH.Count-1)]
  }
}

$placed_count = $newPending.Count
$sum_stake = 0.0
$sum_odds = 0.0
foreach ($ln in $newPending) {
  $p = $ln -split '\|'
  if ($p.Count -ge 6) {
    $sum_stake += [double]$p[4]
    $sum_odds += [double]$p[5]
  }
}
if ($placed_count -gt 0) { $avg_odds = [math]::Round($sum_odds/$placed_count,3) } else { $avg_odds = 0 }

$report = @(
  "MONITOR_END=$($endTime.ToString('o'))",
  "DURATION_SEC=$([int]($endTime-$startTime).TotalSeconds)",
  "ATTEMPTS_ALL=$m_auto_attempt",
  "ATTEMPTS_ODDS=$m_auto_odds_attempt",
  "FAILED_ALL=$m_auto_fail",
  "FAILED_ODDS=$m_auto_odds_fail",
  "REJECTED_ALL=$m_auto_rejected",
  "BLOCK_COND_NOT_ACTIVE=$m_cond_not_active",
  "BLOCK_REAL_LT_MIN=$m_real_lt_min",
  "BLOCK_DEDUP_LOG=$m_dedup",
  "RETRY_EVENTS=$m_retry",
  "NO_AZURO_ODDS=$m_no_azuro",
  "SCORE_NOT_ACTIONABLE=$m_not_actionable",
  "PARSED_YES=$m_parsed_yes",
  "NEW_PENDING_COUNT=$placed_count",
  "NEW_PENDING_STAKE_SUM=$([math]::Round($sum_stake,2))",
  "NEW_PENDING_AVG_ODDS=$avg_odds",
  "NEW_HISTORY_COUNT=$($newHist.Count)",
  "NEW_PENDING_ROWS:"
)
$report | Add-Content $out -Encoding UTF8
if ($placed_count -gt 0) {
  $newPending | Add-Content $out -Encoding UTF8
}
