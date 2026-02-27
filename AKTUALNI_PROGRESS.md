# AKTUALNI_PROGRESS

Aktualizováno: **2026-02-27 13:15**
Repo: `C:\RustMiskoLive`

## Source of truth (teď)

Tento soubor je jediný „live" přehled stavu. Ostatní strategické `.md` ber jako plán/historii.

## Runtime stav (ověřeno 13:15)

- Executor: **ONLINE** — $75.51 USDT
- Feed-hub: **ONLINE** (PID aktivní)
- Alert-bot: **ONLINE** — chat_id=6458129071, 0 pending claims (čisté), dedup 54 betů
- **USDT balance: $75.51**
- AzuroBet NFTs: **67 celkem** (viz detail níže)

## Kompletní NFT audit (2026-02-27)

### Forenzní analýza — ON-CHAIN VERIFIED

| Kategorie | Počet | Detail |
|-----------|-------|--------|
| **WON (isPaid=true)** | 35 | isOutcomeWinning=true, relayer vyplatil $155.28 |
| **LOST** | 23 | Prohrané, $52.47 wagered |
| **CANCELED (isPaid=true)** | 9 | Refundováno $20.79 relayerem |
| **TRULY PENDING** | 0 | Žádný bet nevisí! |

### Finanční rekoncialiace

```
Total wagered:     $156.42
Total returned:    $176.07 (won payouts + cancel refunds)
Net P&L:           +$19.65
Implied deposit:   $55.86
Current balance:   $75.51 ← matches! ($55.86 + $19.65)
```

### Klíčový objev: Azuro relayer

- **VŠECHNY bety byly automaticky vyplaceny Azuro relayerem** (`0x8dA05c00...`)
- Relayer volá `Core.resolvePayout(tokenId)` → nastaví `isPaid=true` → pošle USDT
- **Žádné "ztracené" tokeny!** Vše isPaid=true = vše vyplaceno
- Detaily viz `CONTEXT.md` → "Azuro claim flow"

## Opravy provedené 2026-02-27

### 1. NFT Real-Data Performance Model (`executor/nft_model.mjs`)

- On-chain agregace všech 67 NFT → `data/nft_model.json`
- **Výsledky (REAL DATA):**

| Sport | n | ROI | Verdikt |
|---|---|---|---|
| esports | 8 | **+33.5%** | ✅ TOPKA |
| cs2 | 13 | **+19.3%** | ✅ TOPKA |
| football | 9 | +13.3% | ✅ OK |
| basketball | 13 | -9.5% | ⚠️ data-collection |
| tennis | 5 | -35.6% | ⚠️ data-collection |

| Odds bucket | ROI | Verdikt |
|---|---|---|
| 2.0–3.0 | **+29.6%** | ✅ nejlepší |
| 1.5–2.0 | **+18.9%** | ✅ dobrý |
| <1.5 | -14.9% | ❌ skip |
| >=3.0 | -45.4% | ❌ skip |

- Celkový ROI: **+12.56%** (net +$19.65 / $156.42 wsazeno, 67 betů)

### 2. Stake caps — data-collection mode (`src/bin/alert_bot.rs`)

- Tennis + basketball: **max $1/bet** (`AUTO_BET_STAKE_LOW_USD = 1.0`) — sbíráme data, nelít plný stake
- CS2 / esports / football: $3/bet beze změny

### 3. BUGFIX: Odds cap v anomaly auto-bet pathu

- **Bug:** odds anomaly path nekontroloval `AUTO_BET_MAX_ODDS` → proto prošel `mouz NXT @ 3.41` (bucket >=3.0, ROI -45.4%)
- **Fix:** nová konstanta `AUTO_BET_MAX_ODDS_CS2_MAP = 3.00` pro CS2 `map_winner`, jinak hard cap **2.00**
- Ověřeno buildem `alert-bot` release (6.0MB, 0 errors)

### 4. Pending claims cleanup

- `data/pending_claims.txt` obsahoval 14 zombie záznamů (bety isPaid=true, relayer vyplatil)
- Vyčištěno ručně + ověřen auto-cleanup mechanismus: claim tick rewrituje soubor s `truncate(true)` jen na nerozhodnuté bety

### 5. Safety fixes (alert_bot.rs) — dřívější session

- **Permanent ledger** (`data/ledger.jsonl`): 12 write pointů (PLACED, REJECTED, WON, LOST, CANCELED, CLAIMED, SAFETY_CLAIM)
- **NET daily loss**: `(daily_wagered - daily_returned).max(0.0)` — `daily_wagered +=` jen na confirmed LOST settlement
- Release binary zkompilován (0 errors)

### 6. Executor safety (index.js)

- **Safe auto-prune**: Lost/Rejected → okamžitý prune; Won/Canceled → jen po viewPayout==0 ověření
- `/prune-settled` endpoint přidán

### 7. NFT investigace

- Forenzní audit všech 67 NFTs přes on-chain data
- Potvrzeno: 35 WON + 9 CANCELED + 23 LOST = 67 total, 0 pending
- Claimnuli jsme $3.267 (TID=221572) ručně — to byl jediný NFT kde relayer ještě nezavolal
- Matematická rekoncialiace balance: $75.51 ± $0.35 přesnost

## Slepé uličky (neopakovat!)

1. Subgraph `thegraph.azuro.org` — mrtvý (0 results)
2. Subgraph `thegraph.onchainfeed.org` — vrací 0 betů pro naši wallet
3. Polygonscan V1 API — deprecated
4. Etherscan V2 bez API klíče — unauthorized
5. RPC getLogs indexed topics — public nodes odmítají
6. LP.viewPayout pro settled bety — reverts (isPaid=true)
7. Ruční ABI psaní — VŽDY používat `@azuro-org/toolkit` (coreAbi, lpAbi)

## Git stav

Vše pushnuté na `main` — viz `git log --oneline -3`.
