# Azuro WS Shadow → Primary: Acceptance Checklist

**Shipped:** commit a1b42b3 on `main` (2026-02-28)  
**Code:** `src/azuro_poller.rs` → `run_shadow_ws()` (line 521, +331 LOC)  
**Status:** SHADOW MODE (runs parallel to GQL polling, no decision authority)

---

## Context & Why

Score-edge strategy needs fresh odds to set `minOdds` accurately.  
GQL subgraph polling has inherent lag (block confirmations + indexer delay).  
If odds move between our poll and the on-chain bet, relayer rejects with  
`"Real odds less than min odds"` → wasted gas + missed opportunity.

WS gives push-based `currentOdds` updates → tighter `minOdds` → fewer rejects.

---

## Phase 1: Shadow Mode Validation (CURRENT)

### Metrics to Collect (1-3 days)

| # | Metric | How to Measure | Target |
|---|--------|---------------|--------|
| 1 | **WS uptime %** | `time_connected / total_time` from `WsShadowMetrics` | ≥ 95% |
| 2 | **Reconnects/day** | `WsShadowMetrics.reconnects` counter | ≤ 10/day |
| 3 | **Dead air max** | Longest gap between WS updates for live conditions | ≤ 30s |
| 4 | **Updates/min** | `WsShadowMetrics.updates_received / minutes_running` | ≥ 1/min per active condition |
| 5 | **WS vs GQL delta** | Compare `ws_odds_update_ts` vs `gql_poll_ts` for same condition | WS ≥ 2s earlier p50 |
| 6 | **Memory growth** | `subscribed_polygon.len()` + `subscribed_base.len()` over 24h | Stable (no unbounded growth) |

### Current Known Gaps (P2)

- **No unsubscribe**: `subscribed_polygon` only grows (diff adds, never removes dead conditions)
  - Risk: memory leak over days; stale subscriptions consume bandwidth
  - Mitigation: sets are HashSet, reconnect clears both → natural cleanup on reconnect
  - Fix plan: on reconnect, diff `subscribed - current_conditions` and don't resubscribe dead ones (already done via `condition_rx.borrow_and_update()`)

- **No update-based odds routing**: WS odds are received but not yet used for `minOdds` calculation
  - Current flow: alert_bot reads odds from `state` (populated by GQL poll)
  - Target flow: alert_bot reads from WS-updated `state` field (requires `watch` channel integration)

---

## Phase 2: Guardrail → minOdds Source (NEXT)

Once shadow metrics pass acceptance, use WS odds as `minOdds` validation:

1. **Guardrail mode**: Before placing bet, compare `state.azuro_w1` (GQL) vs `ws_latest_odds` (WS)
   - If delta > 5%, abort bet (odds moved too much since our decision)
   - Log: `WS_GUARDRAIL_BLOCK` event with both odds + timestamps
   
2. **Primary mode**: Replace GQL odds with WS odds for `minOdds` calculation
   - `minOdds = ws_current_odds * MIN_ODDS_FACTOR`
   - Keep GQL poll running as fallback (if WS disconnected > 30s, revert to GQL)

---

## Phase 3: Acceptance Criteria for Primary Switch

All must pass for ≥ 24h continuous operation:

### Reliability (3 metrics)
- [ ] WS uptime ≥ 95% (< 72 min downtime/day)
- [ ] Reconnects ≤ 10/day (not a reconnect storm)
- [ ] Dead air max ≤ 30s for live conditions

### Latency (2 metrics)  
- [ ] WS update → bet send latency p50 ≤ 500ms, p95 ≤ 2s
- [ ] WS leads GQL by ≥ 2s median for same condition odds changes

### Quality (3 guardrails)
- [ ] `minOdds` reject rate drops ≥ 50% vs GQL-only baseline
- [ ] Accepted bets/day does NOT decrease (no regression)
- [ ] Duplicate-prevented/idempotence blocks do NOT increase (WS not causing spam)

---

## Diagnostic Queries (ledger.jsonl)

After instrumentation ships (BET_FAILED + timing in ledger), run these:

```powershell
# 1. Total reject rate
$all = Get-Content data/ledger.jsonl | Where-Object { $_ -match '"PLACED"|"BET_FAILED"' }
$placed = ($all | Where-Object { $_ -match '"PLACED"' }).Count
$failed = ($all | Where-Object { $_ -match '"BET_FAILED"' }).Count
"Reject rate: $failed / $($placed + $failed) = $([math]::Round($failed / ($placed + $failed) * 100, 1))%"

# 2. minOdds rejects specifically
$minOdds = ($all | Where-Object { $_ -match 'is_minodds_reject.*true' }).Count
"minOdds rejects: $minOdds / $($placed + $failed) = $([math]::Round($minOdds / ($placed + $failed) * 100, 1))%"

# 3. Pipeline latency distribution (p50/p95)
$pipelines = $all | ForEach-Object { 
    $j = $_ | ConvertFrom-Json; if ($j.pipeline_ms) { $j.pipeline_ms } 
} | Sort-Object
$p50 = $pipelines[[math]::Floor($pipelines.Count * 0.5)]
$p95 = $pipelines[[math]::Floor($pipelines.Count * 0.95)]
"Pipeline latency: p50=${p50}ms p95=${p95}ms"

# 4. RTT distribution
$rtts = $all | ForEach-Object { 
    $j = $_ | ConvertFrom-Json; if ($j.rtt_ms) { $j.rtt_ms } 
} | Sort-Object
$rp50 = $rtts[[math]::Floor($rtts.Count * 0.5)]
$rp95 = $rtts[[math]::Floor($rtts.Count * 0.95)]
"Executor RTT: p50=${rp50}ms p95=${rp95}ms"

# 5. Correlation: minOdds rejects vs pipeline age
Get-Content data/ledger.jsonl | Where-Object { $_ -match 'is_minodds_reject.*true' } | 
    ForEach-Object { $j = $_ | ConvertFrom-Json; "$($j.pipeline_ms)ms | odds=$($j.requested_odds) min=$($j.min_odds) | $($j.match_key)" }
```

---

## Decision Tree

```
After 24h of BET_FAILED instrumentation data:
│
├─ minOdds rejects > 20% of all attempts
│  ├─ AND pipeline_ms correlates with reject rate
│  │  └─ → WS is MUST-HAVE (stale GQL odds are root cause)
│  │     Action: Implement Phase 2 guardrail mode
│  │
│  └─ AND pipeline_ms does NOT correlate
│     └─ → Problem is MIN_ODDS_FACTOR too tight, not stale odds
│        Action: Relax MIN_ODDS_FACTOR from 0.97 to 0.95
│
├─ minOdds rejects < 20%
│  ├─ AND most failures are "condition not active" / "paused"
│  │  └─ → Normal Azuro behavior (score events pause conditions)
│  │     Action: Keep current retry logic, consider longer backoff
│  │
│  └─ AND failures are diverse (insufficient, nonce, etc.)
│     └─ → Infrastructure issues, not odds freshness
│        Action: Fix specific failure modes first
│
└─ Very few BET_FAILED entries (< 5% rate)
   └─ → System working well, WS is nice-to-have P2 optimization
      Action: Keep shadow, review in 1 week
```

---

*Created: 2026-02-28 | Last updated: 2026-02-28*
