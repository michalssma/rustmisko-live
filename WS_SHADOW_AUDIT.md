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

**2026-02-28 baseline update:**  
Instrumented data shows **minOdds rejects = 0/60 attempts (0%)**.  
All 5 failures are `"Condition is not active"` (ConditionNotRunning), 100% basketball.  
Primary fail driver is **condition lifecycle (Active→Paused→Resolved)**, not stale odds.  
WS value is now mainly as **state feed** (Active/Paused awareness), not odds feed.

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

## Phase 2: Guardrail → State Feed + minOdds Source (NEXT)

Once shadow metrics pass acceptance, use WS primarily as **condition state feed**:

### 2A. State Guardrail (PRIORITY — reduces ConditionNotRunning failures)

1. **WS state tracking**: On each `ConditionUpdated` event, update shared state:
   - `condition_states: HashMap<String, (state: String, updated_at: Instant)>`
   - States: `"Active"`, `"Paused"`, `"Created"`, `"Resolved"`, `"Canceled"`
   - `ConditionUpdated.data.state` carries this info
   
2. **Pre-flight gate**: Before placing bet, check WS condition state:
   - If state == `"Active"` AND `updated_at` < 5s ago → proceed
   - If state != `"Active"` → drop immediately (no executor call, no retries wasted)
   - If no WS data for condition (WS disconnected) → fall through to GQL flow
   - Log: `CONDITION_STATE_GATE` event with state + age
   
3. **Acceptance metric**: `condition_not_active_rate` drops ≥ 50% vs pre-gate baseline

### 2B. Odds Guardrail (secondary — only if minOdds rejects become >10%)

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

### Quality (5 guardrails)
- [ ] `condition_not_active_rate` drops ≥ 50% vs pre-state-gate baseline (Track A)
- [ ] `minOdds` reject rate stays ≤ 10% (Track B – not currently a driver)
- [ ] Accepted bets/day does NOT decrease (no regression)
- [ ] Duplicate-prevented/idempotence blocks do NOT increase (WS not causing spam)
- [ ] `condition_age_ms` p50 for PLACED drops vs GQL-only baseline

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

## Decision Tree (Updated 2026-02-28 with baseline data)

```
After 24h of BET_FAILED instrumentation data:
│
├─ condition_not_active_rate > 10% (Track A — CURRENT PRIMARY DRIVER)
│  ├─ AND condition_age_ms correlates (failed >> placed)
│  │  └─ → WS STATE FEED is MUST-HAVE (stale state detection)
│  │     Action: Implement Phase 2A state guardrail
│  │
│  └─ AND condition_age_ms does NOT correlate (same freshness)
│     └─ → Conditions flip faster than any poll can catch
│        Action: WS STATE FEED still helps (real-time flip detection)
│        Alt: Increase retry backoff for basketball
│
├─ minOdds rejects > 20% (Track B — currently 0%)
│  ├─ AND pipeline_ms correlates with reject rate
│  │  └─ → WS ODDS FEED is MUST-HAVE (stale GQL odds)
│  │     Action: Implement Phase 2B odds guardrail
│  │
│  └─ AND pipeline_ms does NOT correlate
│     └─ → MIN_ODDS_FACTOR too tight
│        Action: Relax from 0.97 to 0.95
│
├─ Both rates low (< 10% condition, < 10% minOdds)
│  └─ → System working well, WS is P2 optimization
│     Action: Keep shadow, review in 1 week
│
└─ Very few BET_FAILED entries (< 5% total rate)
   └─ → WS is nice-to-have P2
      Action: Focus on other improvements
```

### Baseline (2026-02-28):
- condition_not_active_rate: **8.3%** (5/60 attempts) — **all basketball**
- minOdds_reject_rate: **0%** (0/60)
- Executor RTT: p50=124ms, p95=163ms
- Worst sport: basketball 42% fail rate (5/12)

### Instrumentation Fields (commit TBD):
- `reason_code`: `"ConditionNotRunning"` | `"MinOddsReject"` | `"Dedup"` | `"Fatal"` | `"Unknown"`
- `is_condition_state_reject`: boolean
- `condition_age_ms`: staleness of last GQL sighting at bet time
- Run `powershell -File tools/bet_diagnostics.ps1` for full report
---

*Created: 2026-02-28 | Last updated: 2026-02-28 16:30 (condition-state pivot)*