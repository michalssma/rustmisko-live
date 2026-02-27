# CONTEXT

Aktualizováno: **2026-02-27 16:30**

## Co projekt dělá

RustMiskoLive je lokální automatizační stack pro sběr live kurzů/skóre, detekci edge příležitostí a exekuci sázek přes Azuro executor na Polygon chain.

## Aktivní komponenty

- `feed-hub` (Rust): ingest WS feedů + HTTP `/state` a `/opportunities`
  - **Sport-specifické edge modely**: tennis_model, football_model, basketball_model, esport_map_model
  - `detailed_score` field pro kompletní stav hry (sety, gamy, minuty, čtvrtiny)
- `alert-bot` (Rust): alerting, auto-bet logika (LIVE score edges only), cashout/claim orchestrace
  - **Permanent ledger** (`data/ledger.jsonl`) — append-only log všech akcí (12 write pointů)
  - **NET daily loss** — formula: `(daily_wagered - daily_returned).max(0.0)`
- `executor` (Node.js): endpointy `/bet`, `/cashout`, `/check-payout`, `/claim`, `/my-bets`, `/auto-claim`
  - **`/my-bets` a `/auto-claim` — ON-CHAIN NFT enumeration** (žádná subgraph závislost!)
  - **Safe auto-prune** — Lost/Rejected: okamžitý prune; Won/Canceled: až po `viewPayout==0` ověření
- `userscripts/tipsport_odds_scraper.user.js` (v3.0): Tipsport odds/live feed + **detailed_score**
- `userscripts/hltv_live_scraper.user.js` (v3.1): HLTV CS2 live + odds

## Auto-bet strategie (v5.1 — data-driven, 2026-02-27)

### LIVE score edges → auto-bet
- **Esports / CS2** (topky): `map_winner` preferred, min edge 12%, stake **$3**
- **Football**: `match_winner`, min edge 18%, stake **$3**
- **Tennis**: `match_winner`, min edge 15%, stake **$1** (data-collection, ROI -35.6%)
- **Basketball**: `match_winner`, min edge 12%, stake **$1** (data-collection, ROI -9.5%)

### Odds caps (HARD limity)
- Vše: **max 2.00** (`AUTO_BET_MAX_ODDS`)
- **CS2 map_winner výjimka**: max **3.00** (`AUTO_BET_MAX_ODDS_CS2_MAP`) — score edge spolehlivější
- Odds anomaly path **také kontroluje** tento cap (byl bugfix 2026-02-27)

### NFT Real-Data model (67 betů, ROI +12.56%)
- Profitable buckety: odds **1.5–2.0** (+18.9%) a **2.0–3.0** (+29.6%)
- Ztrátové buckety: **<1.5** (-14.9%), **>=3.0** (-45.4%) → nikdy auto-bet
- Výstup: `data/nft_model.json`, skript: `executor/nft_model.mjs`

### Ostatní
- **Odds anomaly** (live 2+ zdroje) → auto-bet s výše uvedenými caps
- **Per-condition dedup** — nikdy dva bety na stejnou condition
- **Inflight lock** — race condition ochrana
- **Daily loss limit** — stop při NET P&L < -$20 (jen confirmed settled losses)

## Sport modely (feed-hub)

| Sport      | Model              | Data parsed z detailed_score         |
| ---------- | ------------------ | ------------------------------------ |
| Tennis     | `tennis_model`     | Sety, gamy, podání (\*), tiebreak    |
| Football   | `football_model`   | Minuta, poločas, skóre po poločasech |
| Basketball | `basketball_model` | Bodový rozdíl, čtvrtiny              |
| Esports    | `esport_map_model` | Map score (z HLTV + Tipsport)        |

## Ověřené prostředí

- Chain: Polygon (`137`)
- Bet token: USDT (`0xc2132D05D31c914a87C6611C10748AEb04B58e8F`)
- Wallet: `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
- AzuroBet NFT: `0x7A1c3FEf712753374C4DCe34254B96faF2B7265B`
- Core (**LiveCore**): `0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7`
- LP: `0x0FA7FB5407eA971694652E6E16C12A52625DE1b8`
- **Relayer (ProxyFront)**: `0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d` — Azuro relayer, NE my
- **Správný subgraph**: `https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-polygon-v3`
- **API endpoint**: `https://api.onchainfeed.org/api/v1/public`

## Azuro claim flow — DŮLEŽité

### Jak funguje vyplácení (verified 2026-02-27)

1. Oracle resolve conditions → `Core.conditions(condId).state` se změní na `Resolved(1)` nebo `Canceled(2)`
2. **Azuro relayer** automaticky volá `Core.resolvePayout(tokenId)` na VŠECHNY bety v dané condition
3. `resolvePayout` nastaví `bets(tokenId).isPaid = true` a pošle USDT z LP na wallet bet ownera
4. Pro CANCELED bety: relayer refunduje původní sázku (amount, ne payout)
5. Pro WON bety: relayer posílá celý payout
6. Pro LOST bety: relayer nastaví isPaid=true, žádná výplata

### Co to znamená v praxi

- **NEMUSÍME aktivně claimovat!** Azuro relayer to dělá automaticky za nás
- `isPaid=true` ≠ "my jsme to claimli" — znamená "relayer to zpracoval"
- `viewPayout()` REVERTS pro isPaid=true tokeny (obě: Core i LP)
- `withdrawPayout()` a `resolvePayout()` taky REVERTS pro isPaid=true
- NFT zůstávají v našem walletu i po výplatě (nejsou burnované)

### Kdy MY musíme claimovat ručně

- Výjimečně: pokud relayer nestihne/selže (timeout 7 dní = `claimTimeout = 604800s`)
- V tom případě: `LP.withdrawPayout(core, tokenId)` nebo `LP.withdrawPayouts(core, [tokenIds])`
- Ale v praxi jsme to nikdy nepotřebovali — relayer je spolehlivý

## NFT audit procedura — "Jak ověřit stav betů"

### Správný postup (ověřeno)

```
1. Načti bettor NFTs:   AzuroBet.tokenOfOwnerByIndex(wallet, 0..n)
2. Pro každý tokenId:   Core.bets(tokenId) → {conditionId, amount, payout, outcomeId, timestamp, isPaid, lastDepositId}
3. Stav condition:       Core.conditions(conditionId) → {totalNetBets, settledAt, lastDepositId, winningOutcomesCount, state, oracle}
   - state: 0=Created, 1=Resolved, 2=Canceled, 3=Paused
4. Win/Loss:             Core.isOutcomeWinning(conditionId, outcomeId) → bool (jen pro Resolved!)
5. Payout check:         Core.viewPayout(tokenId) → uint128 (reverts pokud isPaid=true!)
```

### Interpretace výsledků

| isPaid | condition state | viewPayout | Význam |
|--------|----------------|------------|--------|
| false  | Resolved       | > 0        | **WON — čeká na claim** (relayer ještě nezavolal) |
| false  | Resolved       | 0          | LOST — nic k claimu |
| false  | Canceled       | > 0        | CANCELED — čeká na refund |
| false  | Created        | revert     | PENDING — match ještě neskončil |
| true   | Resolved       | revert     | **JIŽ VYPLACENO relayerem** (WON i LOST) |
| true   | Canceled       | revert     | **JIŽ REFUNDOVÁNO relayerem** |

### Správné ABI (z `@azuro-org/toolkit`)

```js
import * as t from '@azuro-org/toolkit';
// t.coreAbi — LiveCore ABI (bets, conditions, viewPayout, isOutcomeWinning, resolvePayout)
// t.lpAbi   — LP ABI (viewPayout, withdrawPayout, withdrawPayouts, relayer, claimTimeout)
```

### SLEPÉ ULIČKY — co NEFUNGUJE (neopakovat!)

1. **Subgraph `thegraph.azuro.org`** — ZASTARALÝ, vrací 0 výsledků pro naši wallet
2. **Subgraph `thegraph.onchainfeed.org`** — vrací 0 betů pro naši wallet (důvod neznámý)
3. **Polygonscan V1 API** — deprecated od 2026, vrací "switch to V2"
4. **Etherscan V2 API bez API klíče** — vrací "Missing/Invalid API Key"
5. **RPC getLogs pro USDT Transfer** — public RPC odmítá indexed topic queries (Missing parameters)
6. **LP.viewPayout(core, tokenId)** — reverts pro isPaid=true → NEPOUŽÍVAT pro klasifikaci settled betů
7. **Ruční psaní ABI** — Core ABI se liší od standardní verze. VŽDY používat `@azuro-org/toolkit`

## Plánované rozšíření

- Tampermonkey scraper pro **1xbit** (LIVE sekce všech sportů)
- Tampermonkey scraper pro **Fortuna**
- Vyladění sport modelů (Bo5 tenis, overtime fotbal, etc.)

## Důležité pravidlo pro dokumentaci

- Aktuální čísla (balance, pending, procesy) drž pouze v `AKTUALNI_PROGRESS.md`.
- Ostatní `.md` používej jako strategii/plán, ne jako live telemetry.
