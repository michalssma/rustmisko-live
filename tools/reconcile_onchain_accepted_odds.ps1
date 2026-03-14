param(
    [string]$LedgerPath = "data/ledger.jsonl",
    [string]$ActiveBetsPath = "data/active_bets.json",
    [string]$ExecutorUrl = "http://127.0.0.1:3030",
    [switch]$Apply,
    [string]$OutputPath = ""
)

$ErrorActionPreference = "Stop"

function Get-NumberOrNull($value) {
    if ($null -eq $value) { return $null }
    if ($value -is [double] -or $value -is [single] -or $value -is [decimal]) {
        return [double]$value
    }
    if ($value -is [int] -or $value -is [long]) {
        return [double]$value
    }
    $parsed = 0.0
    if ([double]::TryParse([string]$value, [ref]$parsed)) {
        return $parsed
    }
    return $null
}

function NearlyEqual($left, $right, [double]$epsilon = 0.009) {
    if ($null -eq $left -or $null -eq $right) { return $false }
    return [math]::Abs([double]$left - [double]$right) -le $epsilon
}

function Invoke-ExecutorBet($executorUrl, $betId) {
    try {
        return Invoke-RestMethod -Uri "$executorUrl/bet/$betId" -TimeoutSec 8
    } catch {
        return $null
    }
}

if (-not (Test-Path $LedgerPath)) {
    throw "Ledger not found: $LedgerPath"
}

$activeBetsById = @{}
if (Test-Path $ActiveBetsPath) {
    $activeRaw = Get-Content $ActiveBetsPath -Raw
    if (-not [string]::IsNullOrWhiteSpace($activeRaw)) {
        $activeBets = $activeRaw | ConvertFrom-Json
        foreach ($bet in $activeBets) {
            if ($bet.betId) {
                $activeBetsById[[string]$bet.betId] = $bet
            }
        }
    }
}

$executorCache = @{}
$placedByBetId = @{}
$lines = [System.IO.File]::ReadAllLines((Resolve-Path $LedgerPath))
$updatedLines = New-Object System.Collections.Generic.List[string]
$changes = New-Object System.Collections.Generic.List[object]

foreach ($line in $lines) {
    if ([string]::IsNullOrWhiteSpace($line)) {
        $updatedLines.Add($line)
        continue
    }

    try {
        $row = $line | ConvertFrom-Json
    } catch {
        $updatedLines.Add($line)
        continue
    }

    $event = [string]$row.event
    $betId = if ($row.PSObject.Properties.Name -contains 'bet_id') { [string]$row.bet_id } else { '' }

    if ($event -eq 'PLACED' -and -not [string]::IsNullOrWhiteSpace($betId)) {
        $placedByBetId[$betId] = $row
        $updatedLines.Add($line)
        continue
    }

    if ($event -ne 'ON_CHAIN_ACCEPTED' -or [string]::IsNullOrWhiteSpace($betId)) {
        $updatedLines.Add($line)
        continue
    }

    if (-not $executorCache.ContainsKey($betId)) {
        $executorCache[$betId] = Invoke-ExecutorBet $ExecutorUrl $betId
    }

    $executorRow = $executorCache[$betId]
    $activeRow = if ($activeBetsById.ContainsKey($betId)) { $activeBetsById[$betId] } else { $null }
    $placedRow = if ($placedByBetId.ContainsKey($betId)) { $placedByBetId[$betId] } else { $null }

    $currentOdds = Get-NumberOrNull $row.odds
    $requestedOdds = Get-NumberOrNull $(
        if ($row.PSObject.Properties.Name -contains 'requested_odds') { $row.requested_odds }
        elseif ($null -ne $placedRow -and $placedRow.PSObject.Properties.Name -contains 'requested_odds') { $placedRow.requested_odds }
        else { $null }
    )

    $executorAccepted = Get-NumberOrNull $(
        if ($null -ne $executorRow) {
            if ($executorRow.PSObject.Properties.Name -contains 'acceptedOdds') { $executorRow.acceptedOdds }
            elseif ($executorRow.PSObject.Properties.Name -contains 'odds') { $executorRow.odds }
            else { $null }
        } else { $null }
    )
    $activeAccepted = Get-NumberOrNull $(
        if ($null -ne $activeRow) {
            if ($activeRow.PSObject.Properties.Name -contains 'acceptedOdds') { $activeRow.acceptedOdds }
            elseif ($activeRow.PSObject.Properties.Name -contains 'odds') { $activeRow.odds }
            else { $null }
        } else { $null }
    )

    $truthOdds = if ($null -ne $executorAccepted) { $executorAccepted } else { $activeAccepted }
    $truthSource = if ($null -ne $executorAccepted) { 'executor' } elseif ($null -ne $activeAccepted) { 'active_bets' } else { '' }

    if ($null -eq $truthOdds -or (NearlyEqual $currentOdds $truthOdds)) {
        $updatedLines.Add($line)
        continue
    }

    $row.odds = [math]::Round($truthOdds, 6)
    if ($null -ne $requestedOdds -and -not ($row.PSObject.Properties.Name -contains 'requested_odds')) {
        Add-Member -InputObject $row -NotePropertyName 'requested_odds' -NotePropertyValue ([math]::Round($requestedOdds, 6))
    }
    if ($row.PSObject.Properties.Name -contains 'reconcile_source') {
        $row.reconcile_source = $truthSource
    } else {
        Add-Member -InputObject $row -NotePropertyName 'reconcile_source' -NotePropertyValue $truthSource
    }
    if ($row.PSObject.Properties.Name -contains 'reconcile_ts') {
        $row.reconcile_ts = [DateTime]::UtcNow.ToString('o')
    } else {
        Add-Member -InputObject $row -NotePropertyName 'reconcile_ts' -NotePropertyValue ([DateTime]::UtcNow.ToString('o'))
    }

    $updatedLines.Add(($row | ConvertTo-Json -Compress -Depth 12))
    $changes.Add([pscustomobject]@{
        bet_id = $betId
        match_key = [string]$row.match_key
        token_id = [string]$row.token_id
        requested_odds = $requestedOdds
        ledger_odds_before = $currentOdds
        ledger_odds_after = $truthOdds
        source = $truthSource
    }) | Out-Null
}

$summaryLines = New-Object System.Collections.Generic.List[string]
$summaryLines.Add("# ON_CHAIN_ACCEPTED Odds Reconcile")
$summaryLines.Add("")
$summaryLines.Add("Ledger: $LedgerPath")
$summaryLines.Add("Changes: $($changes.Count)")
$summaryLines.Add("")

if ($changes.Count -gt 0) {
    $summaryLines.Add("| Bet ID | Match | Token | Requested | Before | After | Source |")
    $summaryLines.Add("| --- | --- | --- | ---: | ---: | ---: | --- |")
    foreach ($change in $changes) {
        $summaryLines.Add("| $($change.bet_id) | $($change.match_key) | $($change.token_id) | $($change.requested_odds) | $($change.ledger_odds_before) | $($change.ledger_odds_after) | $($change.source) |")
    }
} else {
    $summaryLines.Add("No mismatched ON_CHAIN_ACCEPTED rows found with stronger truth source.")
}

$summary = $summaryLines -join [Environment]::NewLine

if ($Apply -and $changes.Count -gt 0) {
    $backupPath = "$LedgerPath.bak_$(Get-Date -Format 'yyyyMMdd_HHmmss')"
    Copy-Item $LedgerPath $backupPath -Force
    [System.IO.File]::WriteAllLines((Resolve-Path $LedgerPath), $updatedLines)
    $summary += "`n`nApplied: yes`nBackup: $backupPath"
} elseif ($Apply) {
    $summary += "`n`nApplied: no changes"
}

if ($OutputPath) {
    [System.IO.File]::WriteAllText($OutputPath, $summary)
}

$summary