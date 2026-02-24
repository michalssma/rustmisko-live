# AKTUALNI_PROGRESS — handoff pro Sonneta

Aktualizováno: 2026-02-25
Repo: RustMiskoLive (`C:\RustMiskoLive`)

## 🚀 STAV: FEED HUB + AZURO INTEGRATION (LIVE PRODUKCE)

### Aktuální priorita

Hlavním cílem je **cross-platform arbitráž** mezi tradičními bookery (1xbit, HLTV featured) a **Azuro Protocol** (on-chain, NO KYC, Polygon USDC). Systém běží jako Feed Hub — WS server na portu 8080 s HTTP API na portu 8081. Azuro poller je integrován přímo v Rustu.

---

### Architektura (aktuální)

```
┌─────────────────────────────┐
│   TAMPERMONKEY USERSCRIPTS  │
│                             │
│  HLTV scraper v2+           │──── live matches + featured odds
│  (391 lines, TextNode walk) │     → WS → Feed Hub
│                             │
│  Bo3.gg odds scraper v3     │──── multi-bookmaker odds (1xbit)
│  (496 lines, TreeWalker)    │     → WS → Feed Hub
└──────────┬──────────────────┘
           │ WebSocket (port 8080)
           ▼
┌─────────────────────────────┐
│  FEED HUB (Rust, tokio)     │
│                             │
│  WS ingest → parse → store  │
│  Azuro GraphQL poller ←─────│──── polls Polygon+Gnosis subgraphs
│  match_key() normalization  │     every 30s for CS2 on-chain odds
│  OddsKey{match_key,bookie}  │
│  Staleness cleanup (120s)   │
│                             │
│  HTTP API (port 8081):      │
│    /health                  │
│    /state                   │
│    /opportunities           │
└──────────┬──────────────────┘
           │
           ▼
┌─────────────────────────────┐
│  OPPORTUNITIES ENGINE       │
│                             │
│  1. score_momentum          │──── live score ahead, odds lagging
│  2. tight_spread_underdog   │──── low-juice line, underdog value
│  3. arb_cross_book          │──── cross-platform arb detection
│     (1xbit vs azuro_polygon │     ← THIS IS THE MONEY MAKER
│      or hltv vs azuro)      │
└─────────────────────────────┘
```

---

### Co je hotovo a runtime ověřeno

1. **Feed Hub** — WS server (tokio-tungstenite) + raw TCP HTTP server
   - Multi-bookmaker `OddsKey {match_key, bookmaker}` architektura
   - Order-independent `match_key()` (alphabetical team name sorting, normalization)
   - SQLite persistence (WAL mode) via `feed_db.rs`
   - Staleness cleanup — entries starší 120s automaticky odstraněny
   - JSONL event logging

2. **Tampermonkey scrapers**
   - **HLTV v2+**: URL slug parsing + TextNode walker for odds, featured bookmaker detection
   - **Bo3.gg v3**: TreeWalker pattern, `cleanTeamSlug()`, 36-43 valid odds per scan

3. **Opportunities engine** — 3 detection types:
   - `score_momentum`: score leads with lagging odds
   - `tight_spread_underdog`: tight spread (<3%) + high underdog odds (>2.5)
   - `arb_cross_book`: **cross-bookmaker arbitrage** (best odds from 2 bookies < 100%)
   - Historically detected: 21.89%, 5.91%, 2.91%, 2.72% edge signals

4. **Azuro Protocol integration** (NOVÉ!)
   - `azuro_poller.rs` — Rust-native GraphQL poller
   - Polluje Polygon + Gnosis subgraphs každých 30s
   - Parsuje CS2 hry s aktivními podmínkami (match_winner market)
   - Konvertuje Azuro fixed-point odds (10^12) na decimální
   - Injektuje jako `bookmaker: "azuro_polygon"` / `"azuro_gnosis"` do FeedHubState
   - Cross-platform arb detection funguje automaticky (1xbit vs azuro)

---

### Platformy — vyšetřeno

| Platforma   | CS2 coverage | Status |
|-------------|-------------|--------|
| **Azuro**   | ✅ MASIVNÍ   | **INTEGROVÁNO** — CS2 sport id 1061, desítky zápasů denně |
| SX Bet      | ❌ ŽÁDNÉ     | Pouze LoL LPL (2 zápasy). Zero CS2 markets. |
| Polymarket  | ❌ ŽÁDNÉ     | Zero esports. Pouze politika/geopolitika. |
| Overtime    | ❌ DEPRECATED | API nefunkční |

---

### Azuro Protocol — klíčové info

- **Typ**: Decentralizovaný on-chain bookmaker (AMM pool)
- **Chains**: Polygon (USDC), Gnosis, Base
- **KYC**: ŽÁDNÉ — wallet-only přístup
- **API**: GraphQL subgraph (The Graph)
  - Polygon: `https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-polygon-v3`
  - Gnosis: `https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-gnosis-v3`
- **WebSocket**: `wss://streams.onchainfeed.org/v1/streams/feed` (live odds stream)
- **Frontend**: bookmaker.xyz
- **CS2 turnaje**: CCT, ESL Challenger, PGL Bucharest, BetBoom RUSH B, NODWIN Clutch, European Pro League
- **Bet flow**: EIP712 signature → Relayer → on-chain execution
- **Smart contracts**: HostCore (lifecycle), LiveCore (accept), Relayer

---

### Co systém REÁLNĚ dělá teď

```
Continuous loop:
  1. Tampermonkey scrapers → WS → Feed Hub (live matches + odds z 1xbit/hltv)
  2. Azuro poller → GraphQL → Feed Hub (on-chain CS2 odds z Polygon/Gnosis)
  3. match_key normalization → OddsKey storage
  4. /opportunities endpoint → cross-bookmaker arb detection
  5. Edge detected → JSON response (pro budoucí automated execution)
```

---

### Co stále NENÍ hotové (pravdivě)

1. **Automated execution** — zatím `observe_only`, žádné reálné sázky
2. **Wallet integration** — EIP712 signing pro Azuro bet placement
3. **Azuro liquidity parsing** — subgraph vrací pool data, ale ještě neextrahujeme `liquidity_usd`
4. **Team name normalization cross-platform** — "FURIA" vs "furia esports" matching
5. **Telegram alerts** — notifikace při arb detekci
6. **Live odds WebSocket** — `wss://streams.onchainfeed.org` pro sub-second updates (místo 30s polling)

---

### Jak reprodukovat

```powershell
# Terminal 1: Feed Hub
$env:FEED_HUB_BIND="0.0.0.0:8080"
$env:FEED_HTTP_BIND="0.0.0.0:8081"
$env:FEED_DB_PATH="data/feed.db"
cargo run --bin feed-hub

# Terminal 2: Check it
Invoke-RestMethod http://localhost:8081/health
Invoke-RestMethod http://localhost:8081/state
Invoke-RestMethod http://localhost:8081/opportunities

# Chrome: Enable Tampermonkey scripts on HLTV + Bo3.gg
```

### Poznámka k pravdivosti

Tento soubor je záměrně bez optimism bias: popisuje přesně to, co je v repu a co bylo runtime ověřeno, včetně limitů.
