# AKTUALNI_PROGRESS

Aktualizováno: **2026-02-28 04:15**
Repo: `C:\RustMiskoLive`

## Source of truth (teď)

Tento soubor je jediný „live" přehled stavu. Ostatní strategické `.md` ber jako plán/historii.

## Runtime stav (ověřeno 04:15)

- Executor: **ONLINE** — port 3030
- Feed-hub: **ONLINE** (PID 436, port 8081/8080)
- Alert-bot: **ONLINE** — PID 43936, s novými safety guardy
- **USDT balance: $51.95**
- **MATIC balance: 20.10**
- AzuroBet NFTs: **92 celkem** (viz detail níže)
- Fortuna scraper: **v3.2** (draw filter, adaptive polling, fast scroll)
- Data kvalita: **92.5%** correct Fortuna odds (49/53)
- Cross-book overlap: 8 Fortuna×Azuro matchů

## Kompletní NFT audit (2026-02-28)

### Forenzní analýza — ON-CHAIN VERIFIED

| Kategorie | Počet | Detail |
|-----------|-------|--------|
| **WON** | 39 | Vyplaceno |
| **LOST** | 30 | Prohrané |
| **CANCELED** | 12 | Refundováno |
| **PENDING** | ~59 | Čekají na settlement |
| **Celkem** | 92 | |

### Finanční rekoncialiace

```
Total wagered:     ~$205.30
Total returned:    ~$198.21 (won payouts + cancel refunds)
Net P&L:           -$7.09 (ROI -3.45%)
Current balance:   $51.95 USDT
Bets in history:   81
Auto-claim:        funkční, $4.39 právě vyzvednuté
```

### Klíčový objev: Azuro relayer

- **VŠECHNY bety jsou automaticky vyplaceny Azuro relayerem** (`0x8dA05c00...`)
- Relayer volá `Core.resolvePayout(tokenId)` → nastaví `isPaid=true` → pošle USDT
- Auto-claim v alert-bot volá `/auto-claim` na executor každých ~5 minut jako safety net

## Opravy provedené 2026-02-28

### 1. CRITICAL: Identické Azuro odds guard
- **Bug:** Azuro basketball odds ALL 1.84/1.84 (oracle nerozlišuje týmy)
- **Dopad:** 12 betů za falešných 1.84 odds (4× Detroit, CA Union, Boston Celtics, Dallas Mavericks...)
- **Fix:** `penalty += 6` pokud `(odds_team1 - odds_team2).abs() < 0.02` → confidence "LOW" → skip
- **Fix 2:** Anomaly path: přidán `!azuro_odds_identical` check + chybějící `MIN_ODDS` check

### 2. Anomaly path MIN_ODDS bug
- **Bug:** Anomaly auto-bet path nekontroloval `AUTO_BET_MIN_ODDS` → Team Aether prošel za 1.07
- **Fix:** Přidán `azuro_odds >= AUTO_BET_MIN_ODDS` do anomaly conditions

### 3. Fortuna scraper v3.0 → v3.2
- **Draw filter:** rawOdds → post-process filtruje remíza/draw/X/tie → 92.5% kvalita (z ~40%)
- **Concatenated text fix:** regex split "TeamName1.42" → label + value
- **Smart odds selection:** team-name matching when >2 odds
- **Adaptive polling:** 2200ms live / 3400ms idle / +700ms inflight
- **Fast scroll:** 2200ms burst, 1.25vh steps, instant behavior
- **Performance hotfix:** DOM count cached 10s, cap check throttled 15s, max 600 links
- **table-tennis normalization:** `stolni-tenis` URL pattern

### 4. Auto-claim executed
- 4 WON bety nebyly claimnuté → triggnut `/auto-claim` → 2 claimed ($4.39), 2 ještě pending settlement
- Balance: $47.56 → $51.95

### 5. Project cleanup
- Smazán `nul` file (2.55 GB!), ~30 temp/error/debug souborů
- Crate error files, stale audit artifacts vyčištěny

### 6. Triple exposure fix (z dřívější session)
- `base_already_bet` guard na obou pathech (score-edge + anomaly)
- Fluxo vs Oddik triple $9 loss se už nezopakuje

## Audit posledních 10 betů (2026-02-28)

| # | Team | Odds | Výsledek | Hodnocení |
|---|------|------|----------|-----------|
| 72 | Fluxo (map2) | 2.76 | LOST | ⚠️ High odds |
| 73 | Fluxo (map3) | 2.49 | LOST | ⚠️ Triple exposure |
| 74 | Detroit Pistons | 1.84 | PENDING | ❌ BOGUS Azuro odds |
| 75 | CA Union | 1.84 | WON | ❌ BOGUS (lucky win) |
| 76 | Uruguay | 1.73 | PENDING | ✅ Legit |
| 77 | Mibr | 1.70 | PENDING | ✅ Legit |
| 78 | Comunicaciones | 1.35 | PENDING | ✅ Legit |
| 79 | Boston Celtics | 1.84 | PENDING | ❌ BOGUS Azuro odds |
| 80 | Team Aether | 1.07 | CANCELED | ❌ TOO LOW odds |
| 81 | Dallas Mavericks | 1.84 | PENDING | ❌ BOGUS Azuro odds |

**Verdikt:** 4/10 betů = bogus 1.84 data, 1 = too low → **FIX NASAZEN** (identické odds guard + min odds check)

## Slepé uličky (neopakovat!)

1. Subgraph `thegraph.azuro.org` — mrtvý (0 results)
2. Subgraph `thegraph.onchainfeed.org` — vrací 0 betů pro naši wallet
3. Polygonscan V1 API — deprecated
4. Etherscan V2 bez API klíče — unauthorized
5. RPC getLogs indexed topics — public nodes odmítají
6. LP.viewPayout pro settled bety — reverts (isPaid=true)
7. Ruční ABI psaní — VŽDY používat `@azuro-org/toolkit` (coreAbi, lpAbi)
8. **`nul` file v root** — Windows redirect to NUL device creates 2.55 GB file!

## Git stav

Vše pushnuté na `main` — viz `git log --oneline -3`.
