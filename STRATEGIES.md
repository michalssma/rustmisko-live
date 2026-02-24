# RustMiskoLive — Projednané strategie

# Naposledy aktualizováno: 2026-02-25

# Stav: AZURO PROTOCOL = PRIMÁRNÍ EXECUTION PLATFORMA

---

## ✅ AKTIVNÍ STRATEGIE

### ✅ Azuro Protocol (Cross-Platform CS2 Arb) → THE WINNING STRATEGY

**Status: INTEGROVÁNO A PRODUKČNÍ (Azuro poller v feed_hub)**

Decentralizovaný on-chain bookmaker na Polygon/Gnosis/Base. **MASIVNÍ CS2 pokrytí** — desítky zápasů denně s live odds.

**Proč Azuro vyhrává:**
- **NO KYC** — wallet-only, žádná registrace, žádné geo-blocky
- **Polygon USDC** — nízké gas fees, rychlé transakce
- **GraphQL API** — structured data, žádný DOM scraping, spolehlivé
- **AMM pool** — odds driven by liquidity pool, ne bookmaker
- **Cross-platform arb** — porovnáváme 1xbit/hltv odds vs azuro on-chain odds
- **Automated execution** — EIP712 signature → Relayer → on-chain bet placement

**Technické detaily:**
- Subgraph Polygon: `https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-polygon-v3`
- Subgraph Gnosis: `https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-gnosis-v3`
- WebSocket live: `wss://streams.onchainfeed.org/v1/streams/feed`
- CS2 sport: `id: 1061`, `slug: cs2`
- Odds format: fixed-point `value / 10^12` → decimal
- Frontend: bookmaker.xyz
- Dokumentace: gem.azuro.org

**Implementováno v kódu:**
- `src/azuro_poller.rs` — GraphQL poller, 30s interval
- Injektuje do FeedHubState jako `bookmaker: "azuro_polygon"` / `"azuro_gnosis"`
- Cross-book arb detection funguje automaticky v `build_opportunities()`

---

### ✅ Tampermonkey + Feed Hub (Data Fusion)

**Status: PRODUKČNÍ**

Browser-based scraping + Rust WS server = nejspolehlivější combo pro live esport data.
- HLTV scraper v2+ (live matches + featured odds)
- Bo3.gg odds scraper v3 (multi-bookmaker, 36-43 entries per scan)
- Feed Hub: WS 8080 + HTTP 8081

---

## ❌ ZAMÍTNUTO / VYŠETŘENO A ZAVRŽENO

### ❌ SX Bet (Esports Oracle Lag)

**ZAMÍTNUTO: ZERO CS2 markets**

Původně označeno jako "THE WINNING STRATEGY" — ALE API vyšetření ukázalo:
- sportId=9 ("E Sports") má POUZE LoL LPL (2 zápasy: Weibo vs IG, Bilibili vs NiP)
- **ŽÁDNÉ CS2 markets. Vůbec.**
- Oracle lag strategie (10-25 min) je teoreticky validní, ale bez CS2 marketů nepoužitelná

**Verdikt:** SX Bet je mrtvý pro naše účely. Azuro ho kompletně nahradil.

---

### ❌ Polymarket

**ZAMÍTNUTO: ZERO esports**

Events API prozkoumáno s tagy esports/gaming/cs2 — vrací POUZE:
- Politika (Biden, Trump, Starmer)
- Geopolitika (Ukraine/Russia)
- Sporty (FIFA WC 2026, NHL, NBA)
- Jediný historický esports market: LoL Worlds 2020 (uzavřen, $84K volume)

**Verdikt:** Polymarket nemá a nebude mít per-match esports betting.

---

### ❌ Overtime / Thales

**ZAMÍTNUTO: DEPRECATED**

API endpointy nefunkční. Projekt patrně migoval nebo ukončil provoz.

---

### ❌ Betfair Exchange

**BLOKOVÁNO: CZ geoblocking**

Betfair.com i developer.betfair.com hlásí "Czech Republic unavailable".
Stream API je technicky ideální pro in-play lag arb, ale bez přístupu nepoužitelné.

**Co by pomohlo:** UK VPN + UK legal entity. Risk: ToS Section 6.3 zakazuje VPN.

---

### ❌ Smarkets

**BLOKOVÁNO: CZ 404**

smarkets.com/register vrací 404 z CZ. 2% commission by byla ideální pro arb.

---

### ❌ Pinnacle API

**BLOKOVÁNO: 401 bez auth**

Free read-only API vyžaduje funded account pro přihlašovací údaje.

---

### ❌ OddsPortal / Tipsport

**ZAMÍTNUTO: nestabilní scraping / interní API bez dokumentace**

---

## 🟡 BUDOUCÍ ROZŠÍŘENÍ

### 🟡 Azuro WebSocket (Live Odds Stream)

**Status: Endpoint známý, neimplementováno**

`wss://streams.onchainfeed.org/v1/streams/feed` — sub-second odds updates místo 30s polling.
Implementovat až po ověření základního polling flow.

### 🟡 Azuro Bet Execution

**Status: API prostudováno, neimplementováno**

EIP712 signing → Relayer submission. Vyžaduje:
1. Polygon wallet s USDC
2. ethers-rs nebo alloy pro signing
3. Relayer API integration

### 🟡 odds-api.io (Small League Mispricing)

**Status: API key k dispozici, neotestováno**

```
ODDSAPI_KEY=edf29a96be1a0f82a5f2507494e05f88d4d1508912fd54d2878c187767247b13
```

100 req/h free tier. Endpoint `/arbitrage-bets` vrací hotové arb příležitosti.

---

## Závěr: Aktuální priorita

```
PRIMÁRNÍ:  Azuro Protocol × Tampermonkey odds → cross-platform CS2 arb
SEKUNDÁRNÍ: Azuro WebSocket pro real-time + wallet execution
TERCIÁRNÍ: odds-api.io pro doplňkové small-league mispricing
```

Azuro je JEDINÁ viable crypto platforma pro CS2 per-match betting.
Systém je architektonicky hotový, zbývá execution layer.
