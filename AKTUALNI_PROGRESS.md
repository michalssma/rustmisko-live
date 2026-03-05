# AKTUALNI_PROGRESS

Aktualizováno: **2026-03-05 15:00**
Repo: `C:\RustMiskoLive`

## Source of truth (teď)

Tento soubor je jediný „live" přehled stavu. Ostatní strategické `.md` ber jako plán/historii.

## Runtime stav (ověřeno 2026-03-05)

- Executor: **ONLINE** — port 3030
- Feed-hub: **ONLINE** (port 8081/8080), `/health` vrací JSON `{gql_age_ms, ws_age_ms}`
- Alert-bot: **ONLINE** — WS_STATE_GATE=false (GQL fallback), condition-dead blacklist active
- **USDT balance: ~$26.99** (session 2026-03-05)
- AzuroBet NFTs: viz ledger.jsonl
- Dashboard: **INSTALLED** — port 7777 (`node dashboard/server.js`; PIN setup: `node dashboard/setup.js`)

## NFT stav (on-chain verified)

| Kategorie | Počet | Detail |
|-----------|-------|--------|
| **Already Paid (WON)** | 70 | Vyplaceno Azuro relayerem |
| **LOST** | 49 | Prohrané |
| **PENDING** | 0 | Čekají na settlement |
| **CLAIMABLE** | 0 | Nic k vyzvednutí |
| **Celkem** | 119 | |

## Profitability tuning 2026-03-05 (data-driven 4-bod plán)

Na základě analýzy 119 betů (ledger.jsonl) — strategie + sport + odds buckety:

### Klíčová zjištění
- **Anomaly WR 56.3%**, ale break-even potřebuje 62.6% → ztrátové
- **Edge WR 39.5%**, break-even 50.0% → ztrátové při nízké edge
- **Edge ≥ 30%: WR 61.1%**, margin +16pp, PnL +$5.94 na 18 betů ✅
- **Tennis anomaly 1.50-1.70: WR 80%**, PnL +$2.61 ✅
- **Esports anomaly: -EV ve VŠECH odds bucketech** → OFF
- **Football anomaly: WR 40%** → OFF

### Provedené změny (alert_bot.rs)
| Bod | Změna | Před | Po |
|-----|-------|------|-----|
| 1 | min_edge_pct všechny sporty | 11-15% | **30%** |
| 2a | FF_FOOTBALL_ANOMALY_GOALDIFF2 | true | **false** |
| 2b | ANOMALY_MAX_ODDS (nová konstanta) | — | **1.70** |
| 2c | Esports anomaly guard | aktivní | **false (OFF)** |
| 4 | AUTO_BET_STAKE_LOW_USD (tennis/basket) | 0.0 | **$0.50** |

### Očekávaný efekt
- Méně betů, výrazně vyšší WR
- Pouze tennis anomaly (odds ≤1.70) a edge ≥30% všechny sporty
- Tennis/basketball score-edge nyní real ($0.50) místo paper-trading
- Po potvrzení 30-50 betů → zvýšení stakes na $2-3

## Git stav (aktuální commity)

```
3aa279d  unify: Created follow-up polling on all 3 bet paths
62c4270  fix: Won->alreadyPaid, startup msg both paths, manual bet Created follow-up polling
45b1713  tune: HIGH>=12%, sources>=2, football+basketball anomaly ON, zombie inflight TTL fix
b86acf1  fix(Phase 2.4): tchajwan/tchajpej/cinskatajpej -> chinesetaipei mapping
f7bff50  feat: matching fix + observability + strategy tuning (5 phases)
d4469f2  fix: add WS_STATE_GATE=true to both startup scripts
5910b4e  feat: real-only portfolio on-chain reconcile + WS diagnostics
```

## Opravy provedené 2026-03-01

### 1. Won:0 → alreadyPaid fix (commit 62c4270)
- **Bug:** Executor `/my-bets` vrací top-level `alreadyPaid`, ale alert-bot četl neexistující `won` field → vždy 0
- **Fix:** `mb.get("won")` → `mb.get("alreadyPaid")` na 2 místech (portfolio /status + /bets command)

### 2. Startup message — obě auto-bet paths (commit 62c4270)
- **Bug:** Startup message ukazoval jen Path A (Score Edge), chyběl Path B (Odds Anomaly)
- **Fix:** Přepsán startup message s oběma paths + shared limits

### 3. Created→follow-up polling — manual (commit 62c4270)
- **Bug:** Executor vrací `State: Created` (relayer ACK), ale on-chain tx může revertovat 10-30s později. Žádný follow-up.
- **Příklad:** GENG bet — State: Created, ale tx reverted, `error: TransactionFailed`. Bot hlásil ✅ SUCCESS.
- **Fix:** Async tokio task po každém manual betu — wait 20s → GET `/bet/:id` → TG alert pokud Rejected

### 4. Created→follow-up — unified all paths (commit 3aa279d)
- **Rozšíření:** Stejná follow-up logika přidána na Path A (Score Edge) a Path B (Odds Anomaly)
- **Výsledek:** Všechny 3 bet paths (auto-edge, auto-anomaly, manual) mají identický follow-up

### 5. 5-Phase Matching Fix (commit f7bff50)
- NFKD Unicode normalizace (`é→e`, `ž→z`, `č→c`, ...)
- Country translation (`novýzéland→newzealand`, `tchajwan→chinesetaipei`, ...)
- Sport alias (`hockey→ice-hockey`, `lol→league-of-legends`)
- Token-subset pair matching s guardrails
- Strategy tuning: cooldowns 90/60s, MAX_CONCURRENT_PENDING=8, loss streak pause

### 6. Throttle tuning + zombie fix (commit 45b1713)
- HIGH edge threshold → 12% (z dřívějšího)
- MIN_MARKET_SOURCES = 2
- Football + basketball anomaly path ON
- Zombie inflight TTL fix — stale pending_claims cleanup

### 7. Tchajwan hotfix (commit b86acf1)
- Přidáno mapování tchajwan/tchajpej/čínská tajpej → chinesetaipei

## Slepé uličky (neopakovat!)

1. Subgraph `thegraph.azuro.org` — mrtvý (0 results)
2. Subgraph `thegraph.onchainfeed.org` — vrací 0 betů pro naši wallet
3. Polygonscan V1 API — deprecated
4. Etherscan V2 bez API klíče — unauthorized
5. RPC getLogs indexed topics — public nodes odmítají
6. LP.viewPayout pro settled bety — reverts (isPaid=true)
7. Ruční ABI psaní — VŽDY používat `@azuro-org/toolkit` (coreAbi, lpAbi)
8. **`nul` file v root** — Windows redirect to NUL device creates 2.55 GB file!
9. **Executor `/my-bets` vrací `alreadyPaid`**, NE `won` — nikdy číst field `won`
10. **`State: Created` ≠ bet success** — relayer ACK, tx může revertovat. VŽDY follow-up polling.

## Git stav

Vše pushnuté na `main` — viz `git log --oneline -10`.
