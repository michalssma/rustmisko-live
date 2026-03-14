param(
    [string]$LedgerPath = "data/ledger.jsonl",
    [string]$FromDate = "",
    [string]$ToDate = "",
    [string]$OutputPath = ""
)

$ErrorActionPreference = "Stop"

if (-not $FromDate) {
    $FromDate = (Get-Date).ToString("yyyy-MM-dd")
}
if (-not $ToDate) {
    $ToDate = $FromDate
}

$fromTs = [datetimeoffset]::Parse("${FromDate}T00:00:00+00:00")
$toTs = [datetimeoffset]::Parse("${ToDate}T23:59:59+00:00")

function Get-MatchPrefix([string]$matchKey) {
    if ([string]::IsNullOrWhiteSpace($matchKey)) { return "unknown" }
    return ($matchKey -split "::")[0]
}

function Get-MarketKey($row, $betMetadata) {
    if ($row.PSObject.Properties.Name -contains "market_key" -and -not [string]::IsNullOrWhiteSpace($row.market_key)) {
        return [string]$row.market_key
    }
    if (($row.PSObject.Properties.Name -contains "bet_id") -and $betMetadata.ContainsKey([string]$row.bet_id)) {
        $metaMarket = [string]$betMetadata[[string]$row.bet_id].market_key
        if (-not [string]::IsNullOrWhiteSpace($metaMarket) -and $metaMarket -ne "unknown") {
            return $metaMarket
        }
    }
    $matchKey = [string]$row.match_key
    if ($matchKey -match "::([A-Za-z0-9_]+_winner)$") {
        return $Matches[1].ToLowerInvariant()
    }
    return "match_winner"
}

function Get-PathName($row, $betMetadata) {
    if ($row.PSObject.Properties.Name -contains "path" -and -not [string]::IsNullOrWhiteSpace($row.path)) {
        $path = [string]$row.path
        if ($path -ne "loaded" -and $path -ne "unknown") {
            return $path
        }
    }
    if (($row.PSObject.Properties.Name -contains "bet_id") -and $betMetadata.ContainsKey([string]$row.bet_id)) {
        $metaPath = [string]$betMetadata[[string]$row.bet_id].path
        if (-not [string]::IsNullOrWhiteSpace($metaPath)) {
            return $metaPath
        }
    }
    if ($row.PSObject.Properties.Name -contains "path" -and -not [string]::IsNullOrWhiteSpace($row.path)) {
        return [string]$row.path
    }
    return "unknown"
}

$rows = Get-Content $LedgerPath | ForEach-Object {
    try { $_ | ConvertFrom-Json } catch { $null }
} | Where-Object {
    $_ -and $_.ts -and ([datetimeoffset]::Parse($_.ts) -ge $fromTs) -and ([datetimeoffset]::Parse($_.ts) -le $toTs)
}

$betMetadata = @{}
foreach ($row in $rows) {
    if (-not ($row.PSObject.Properties.Name -contains "bet_id") -or [string]::IsNullOrWhiteSpace($row.bet_id)) {
        continue
    }

    $betId = [string]$row.bet_id
    if (-not $betMetadata.ContainsKey($betId)) {
        $betMetadata[$betId] = [ordered]@{
            path = ""
            market_key = ""
        }
    }

    if (($row.PSObject.Properties.Name -contains "path") -and -not [string]::IsNullOrWhiteSpace($row.path)) {
        $path = [string]$row.path
        if ($path -ne "loaded" -and $path -ne "unknown") {
            $betMetadata[$betId].path = $path
        } elseif ([string]::IsNullOrWhiteSpace($betMetadata[$betId].path)) {
            $betMetadata[$betId].path = $path
        }
    }

    if (($row.PSObject.Properties.Name -contains "market_key") -and -not [string]::IsNullOrWhiteSpace($row.market_key)) {
        $marketKey = [string]$row.market_key
        if ($marketKey -ne "unknown") {
            $betMetadata[$betId].market_key = $marketKey
        } elseif ([string]::IsNullOrWhiteSpace($betMetadata[$betId].market_key)) {
            $betMetadata[$betId].market_key = $marketKey
        }
    }
}

$reportRows = @{}

foreach ($row in $rows) {
    $prefix = Get-MatchPrefix ([string]$row.match_key)
    $market = Get-MarketKey $row $betMetadata
    $path = Get-PathName $row $betMetadata
    $bucketKey = "$prefix|$market|$path"

    if (-not $reportRows.ContainsKey($bucketKey)) {
        $reportRows[$bucketKey] = [ordered]@{
            prefix = $prefix
            market_key = $market
            path = $path
            placed = 0
            accepted = 0
            rejected = 0
            failed = 0
            won = 0
            lost = 0
            canceled = 0
            odds_drift_alerts = 0
            placed_stake = 0.0
            realized_pnl = 0.0
            edge_samples = 0
            edge_gap_sum = 0.0
        }
    }

    $bucket = $reportRows[$bucketKey]
    $event = [string]$row.event
    $amount = if ($row.PSObject.Properties.Name -contains "amount_usd") { [double]$row.amount_usd } else { 0.0 }
    $payout = if ($row.PSObject.Properties.Name -contains "payout_usd") { [double]$row.payout_usd } else { 0.0 }

    switch ($event) {
        "PLACED" {
            $bucket.placed += 1
            $bucket.placed_stake += $amount
            if (($row.PSObject.Properties.Name -contains "score_implied_pct") -and ($row.PSObject.Properties.Name -contains "azuro_implied_pct")) {
                $bucket.edge_samples += 1
                $bucket.edge_gap_sum += ([double]$row.score_implied_pct - [double]$row.azuro_implied_pct)
            }
        }
        "ON_CHAIN_ACCEPTED" { $bucket.accepted += 1 }
        "ON_CHAIN_REJECTED" { $bucket.rejected += 1 }
        "BET_FAILED" { $bucket.failed += 1 }
        "WON" {
            $bucket.won += 1
            $bucket.realized_pnl += ($payout - $amount)
        }
        "LOST" {
            $bucket.lost += 1
            $bucket.realized_pnl -= $amount
        }
        "CANCELED" {
            $bucket.canceled += 1
            $bucket.realized_pnl += ($payout - $amount)
        }
        "ODDS_DRIFT_ALERT" { $bucket.odds_drift_alerts += 1 }
    }
}

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("# Daily Edge Report")
$lines.Add("")
$lines.Add("Range: $FromDate -> $ToDate")
$lines.Add("")
$lines.Add("| Prefix | Market | Path | Placed | Accepted | Failed | Rejected | Won | Lost | Canceled | Drift | Stake | Realized PnL | Avg Model Gap |")
$lines.Add("| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")

$reportRows.Values |
    Sort-Object @{ Expression = { $_.realized_pnl }; Descending = $true }, @{ Expression = { $_.placed_stake }; Descending = $true } |
    ForEach-Object {
        $avgGap = if ($_.edge_samples -gt 0) { [math]::Round($_.edge_gap_sum / $_.edge_samples, 2) } else { $null }
        $gapText = if ($null -eq $avgGap) { "-" } else { [string]$avgGap }
        $lines.Add("| $($_.prefix) | $($_.market_key) | $($_.path) | $($_.placed) | $($_.accepted) | $($_.failed) | $($_.rejected) | $($_.won) | $($_.lost) | $($_.canceled) | $($_.odds_drift_alerts) | $([math]::Round($_.placed_stake, 2)) | $([math]::Round($_.realized_pnl, 2)) | $gapText |")
    }

$driftRows = $rows | Where-Object { $_.event -eq 'ODDS_DRIFT_ALERT' }
if ($driftRows.Count -gt 0) {
    $lines.Add("")
    $lines.Add("## Odds Drift Alerts")
    $lines.Add("")
    $lines.Add("| Match | Market | Path | Requested | Accepted | Delta % | Stake |")
    $lines.Add("| --- | --- | --- | ---: | ---: | ---: | ---: |")
    $driftRows |
        Sort-Object @{ Expression = { [math]::Abs([double]$_.delta_pct) }; Descending = $true } |
        Select-Object -First 10 |
        ForEach-Object {
            $market = Get-MarketKey $_ $betMetadata
            $path = Get-PathName $_ $betMetadata
            $stake = if ($_.PSObject.Properties.Name -contains 'stake') { [double]$_.stake } else { 0.0 }
            $requested = if ($_.PSObject.Properties.Name -contains 'requested_odds') { [double]$_.requested_odds } else { 0.0 }
            $accepted = if ($_.PSObject.Properties.Name -contains 'accepted_odds') { [double]$_.accepted_odds } else { 0.0 }
            $deltaPct = if ($_.PSObject.Properties.Name -contains 'delta_pct') { [double]$_.delta_pct } else { 0.0 }
            $lines.Add("| $([string]$_.match_key) | $market | $path | $requested | $accepted | $([math]::Round($deltaPct, 2)) | $([math]::Round($stake, 2)) |")
        }
}

$output = ($lines -join [Environment]::NewLine)

if ($OutputPath) {
    [System.IO.File]::WriteAllText($OutputPath, $output)
}

$output