# Strategie

Aktualizováno: **2026-03-01**

## Aktivní produkční strategie

### 1. Path A: Score-edge (primární)
- **Trigger**: live score změna → edge nad sport-specifickým limitem
- **Esports/CS2**: min edge 12%, stake **$3**, `match_or_map` market
- **Football**: min edge 18%, stake **$3**, `match_winner`
- **Tennis**: min edge 12%, stake **$1** (data-collection)
- **Basketball**: min edge 12%, stake **$1** (data-collection)
- **Volleyball/Hockey/Baseball/Cricket/Boxing**: min edge 15%, stake **$1**
- **Cíl**: využít zpoždění adjustace kurzů po skóre změně

### 2. Path B: Odds anomaly (sekundární)
- **Trigger**: HIGH confidence, 2+ market sources, bounded discrepancy
- **Stake**: **$2** (pro všechny sporty)
- **Odds range**: 1.15–2.50 (CS2 map: 1.15–3.00)
- **Guards**: `!azuro_odds_identical` + MIN_ODDS + MAX_ODDS check
- **Sporty**: ALL (football + basketball anomaly ON)
- **Cíl**: chytat čisté anomálie s cross-book potvrzením

### 3. Manual bet (Telegram command)
- **Trigger**: user reply `YES`, `3 YES $5`, atd.
- **Default stake**: $3, max odds 2.00
- **Alert max age**: 25s (prevents stale bets)

## Risk guardy

### Layer 1: Vstupní filtry
- Min/max odds: 1.15–2.50 (CS2 map: až 3.00)
- Max odds age: 20s (stale data skip)
- Sport-specific min edge: 12%–18%
- Identické Azuro odds guard (`penalty += 6`)

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
