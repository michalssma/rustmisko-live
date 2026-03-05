# Strategie

Aktualizováno: **2026-03-05 (post-profitability tuning)**

## Aktivní produkční strategie

### 1. Path A: Score-edge (primární)
- **Trigger**: live score změna → edge nad sport-specifickým limitem
- **Min edge: 30%** pro VŠECHNY sporty (data-driven: WR 61.1% při ≥30%, margin +16pp)
- **Esports/CS2**: min edge **30%**, stake **$3**, `match_or_map` market
- **Football**: min edge **30%**, stake **$3**, `match_winner`
- **Tennis**: min edge **30%**, stake **$0.50** (nově real — dříve paper-trading $0)
- **Basketball**: min edge **30%**, stake **$0.50** (nově real — dříve paper-trading $0)
- **Volleyball/Hockey/Baseball/Cricket/Boxing**: min edge **30%**, stake **$1**
- **Cíl**: využít zpoždění adjustace kurzů po skóre změně

### 2. Path B: Odds anomaly (sekundární — omezená)
- **Trigger**: HIGH confidence, 2+ market sources, bounded discrepancy
- **Stake**: **$0.50–$1.00** (dynamicky dle odds: `base × (1.25/odds)^1.5`, cap $1.00)
- **Max odds**: **1.70** (ANOMALY_MAX_ODDS — data: tennis ≥1.70 = 30% WR katastrofa)
- **Min discrepancy**: **28%**
- **Guards**: `!azuro_odds_identical` + MIN_ODDS + MAX_ODDS + `azuro_odds ≤ 1.70`
- **⚫ ESPORTS: OFF** — WR 52.4% vs break-even 60.8%, -$14.55 PnL ve VŠECH odds bucketech
- **⚫ FOOTBALL: OFF** — `FF_FOOTBALL_ANOMALY_GOALDIFF2 = false`, WR 40%, -$4.54 PnL
- **✅ TENNIS: ON** — odds 1.50-1.70: WR 80%, PnL +$2.61
- **✅ BASKETBALL: ON** — WR 4/4 (malý vzorek, monitorujeme)
- **Cíl**: pouze nejbezpečnější anomálie (tennis low-odds)

### 3. Manual bet (Telegram command)
- **Trigger**: user reply `YES`, `3 YES $5`, atd.
- **Default stake**: $3, max odds 2.00
- **Alert max age**: 25s (prevents stale bets)

## Risk guardy

### Layer 1: Vstupní filtry
- Anomaly max odds: **1.70** (ANOMALY_MAX_ODDS) — break-even při 59% WR = $1/0.59 = 1.695$
- Max odds age: 20s (stale data skip)
- Min edge: **30%** všechny sporty (data-driven: 61.1% WR při ≥30%)
- Identické Azuro odds guard (`penalty += 6`)
- Esports anomaly: **OFF** (guard returns false)
- Football anomaly: **OFF** (FF_FOOTBALL_ANOMALY_GOALDIFF2 = false)

### Layer 2: Dedup & Cooldowns
- Per-condition dedup (s re-bet upgrade)
- Score edge cooldown: 60s per match
- Alert cooldown: 90s per match+score+side
- Inflight lock + SIGNAL_TTL 3s

### Layer 3: Exposure Management
- Per-bet cap: 5% bankrollu (micro tier)
- Per-condition cap: 10%
- Per-match cap: 15%
- Daily loss cap: 30% bankrollu NEBO $30 hard limit
- Min stake: $0.50 (pod tím → skip)
- Min bankroll: $20 (pod tím → no auto-bet)

### Layer 4: Data Quality
- Cross-validation HLTV vs Chance (mismatch → hard skip + resync freeze)
- Watchdog: 120s bez feed-hub dat → SAFE MODE
- WS State Gate: pre-flight condition Active check z Azuro WebSocket

### Layer 5: Settlement Safety
- Claim pre-filter (claimable && !pending)
- Azuro relayer auto-claim (7d timeout)
- Safe auto-prune (viewPayout==0 ověření)
- Permanent ledger (append-only)
- **Created→follow-up polling** — 20s async check na všech 3 bet paths

### Layer 6: Streak & Inflight Protection
- Max concurrent pending: 8
- Loss streak: 3 consecutive LOST → 300s pauza
- Per-sport exposure cap
- Inflight % bankrollu cap

## Co není aktivní strategie

- nápady bez implementace (Betfair/Polymarket/funding arbitráže) jsou backlog, ne live rozhodování.

## Source of truth

- runtime čísla: `AKTUALNI_PROGRESS.md`
- implementace logiky: `src/bin/alert_bot.rs`, `src/feed_hub.rs`, `executor/index.js`
