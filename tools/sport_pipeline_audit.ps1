param(
    [string[]]$Sports = @("basketball", "valorant", "dota-2"),
    [int]$LookbackDays = 7,
    [string]$LedgerPath = "data/ledger.jsonl",
    [string]$HealthUrl = "http://127.0.0.1:8081/health",
    [string]$StateUrl = "http://127.0.0.1:8081/state",
    [string]$OutputPath = ""
)

$ErrorActionPreference = "Stop"

function Try-JsonRequest([string]$url) {
    try {
        return Invoke-RestMethod -Uri $url -TimeoutSec 10
    } catch {
        return $null
    }
}

function Prefix([string]$matchKey) {
    if ([string]::IsNullOrWhiteSpace($matchKey)) { return "unknown" }
    return ($matchKey -split "::")[0]
}

$fromTs = [datetimeoffset]::UtcNow.AddDays(-1 * $LookbackDays)
$rows = Get-Content $LedgerPath | ForEach-Object {
    try { $_ | ConvertFrom-Json } catch { $null }
} | Where-Object {
    $_ -and $_.ts -and ([datetimeoffset]::Parse($_.ts) -ge $fromTs)
}

$health = Try-JsonRequest $HealthUrl
$state = Try-JsonRequest $StateUrl

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("# Sport Pipeline Audit")
$lines.Add("")
$lines.Add("LookbackDays: $LookbackDays")
$lines.Add("")

foreach ($sport in $Sports) {
    $lines.Add("## $sport")
    $lines.Add("")

    $readiness = $null
    if ($health -and $health.sport_readiness) {
        $readiness = $health.sport_readiness | Where-Object { $_.sport -eq $sport } | Select-Object -First 1
    }

    if ($readiness) {
        $lines.Add("Readiness: live_total=$($readiness.live_total), actionable=$($readiness.actionable), ratio_pct=$($readiness.actionable_ratio_pct)")
    } else {
        $lines.Add("Readiness: no explicit sport_readiness entry")
    }

    $recent = $rows | Where-Object { (Prefix ([string]$_.match_key)) -eq $sport }
    $byEvent = $recent | Group-Object event | Sort-Object Name
    if ($byEvent.Count -gt 0) {
        $eventSummary = ($byEvent | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ", "
        $lines.Add("Recent ledger: $eventSummary")
    } else {
        $lines.Add("Recent ledger: no recent events with direct prefix match")
    }

    $currentLive = @()
    if ($state -and $state.live) {
        $currentLive = $state.live | Where-Object {
            ($_.payload.sport -eq $sport) -or ((Prefix ([string]$_.match_key)) -eq $sport)
        } | Select-Object -First 10
    }

    if ($currentLive.Count -gt 0) {
        $lines.Add("Current live sample:")
        foreach ($item in $currentLive) {
            $lines.Add("- $($item.match_key) | payload.sport=$($item.payload.sport) | score=$($item.payload.score1)-$($item.payload.score2) | status=$($item.payload.status)")
        }
    } else {
        $lines.Add("Current live sample: none matched directly")
    }

    switch ($sport) {
        "basketball" {
            $lines.Add("Audit focus: team matching, market identity, Azuro condition/outcome IDs, score garbage filtering, ConditionNotRunning clusters.")
        }
        "valorant" {
            $lines.Add("Audit focus: readiness under explicit valorant labels vs generic esports fallback, map-vs-match identity, score semantics.")
        }
        "dota-2" {
            $lines.Add("Audit focus: score semantics, explicit sport labeling, map-state meaning, Azuro market identity.")
        }
        default {
            $lines.Add("Audit focus: readiness, matching, market identity, live score semantics.")
        }
    }

    $lines.Add("")
}

$output = ($lines -join [Environment]::NewLine)

if ($OutputPath) {
    [System.IO.File]::WriteAllText($OutputPath, $output)
}

$output