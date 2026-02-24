# RustMiskoLive — Implementační plán

# Naposledy aktualizováno: 2026-02-25

# Status: PHASE 5 — AZURO CROSS-PLATFORM ARB (LIVE)

---

## Architektura (aktuální)

```
┌──────────────────────────────────────────────────────────────────┐
│                        DATA SOURCES                             │
│                                                                  │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────────────┐ │
│  │ HLTV.org    │  │ Bo3.gg       │  │ Azuro Protocol          │ │
│  │ Tampermonkey│  │ Tampermonkey │  │ (Rust-native GraphQL)   │ │
│  │ v2+ scraper │  │ v3 scraper   │  │ Polygon + Gnosis        │ │
│  │ live+odds   │  │ 1xbit odds   │  │ 30s poll interval       │ │
│  └──────┬──────┘  └──────┬───────┘  └──────────┬──────────────┘ │
│         │ WS             │ WS                   │ reqwest        │
└─────────┼────────────────┼──────────────────────┼────────────────┘
          │                │                      │
          ▼                ▼                      ▼
┌──────────────────────────────────────────────────────────────────┐
│                     FEED HUB (Rust, tokio)                      │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ WS Server (port 8080)                                      │ │
│  │ FeedEnvelope → LiveMatchPayload / OddsPayload              │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ State: HashMap<String, LiveMatchState>                      │ │
│  │        HashMap<OddsKey, OddsState>                          │ │
│  │ OddsKey = { match_key, bookmaker }                          │ │
│  │ match_key = "cs2::team_a_vs_team_b" (alphabetical)          │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ Azuro Poller (azuro_poller.rs)                              │ │
│  │ GraphQL → parse → inject as azuro_polygon/azuro_gnosis      │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ HTTP Server (port 8081)                                     │ │
│  │ GET /health | /state | /opportunities                       │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ SQLite (WAL) + JSONL logs                                   │ │
│  └─────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
          │
          ▼
┌──────────────────────────────────────────────────────────────────┐
│                   OPPORTUNITIES ENGINE                           │
│                                                                  │
│  For each fused match (live + odds from ≥1 bookmaker):           │
│                                                                  │
│  1. SCORE_MOMENTUM:                                              │
│     score_diff ≥ 3 && implied_prob > 40% → fair estimate +15%    │
│     → edge > 3% triggers opportunity                             │
│                                                                  │
│  2. TIGHT_SPREAD_UNDERDOG:                                       │
│     spread < 3% && underdog_odds > 2.5 → +5% fair value          │
│                                                                  │
│  3. ARB_CROSS_BOOK: ← PRIMARY PROFIT SOURCE                     │
│     1/odds_A_team1 + 1/odds_B_team2 < 1.0                       │
│     Example: 1xbit t1@2.10 + azuro_polygon t2@2.05              │
│     → arb = 1/2.10 + 1/2.05 = 0.964 → 3.6% guaranteed profit   │
│                                                                  │
│  Sorted by edge_pct descending                                   │
└──────────────────────────────────────────────────────────────────┘
          │
          ▼ (future)
┌──────────────────────────────────────────────────────────────────┐
│                   EXECUTION LAYER (TODO)                         │
│                                                                  │
│  Azuro: EIP712 signature → Relayer → Polygon smart contract      │
│  Wallet: USDC on Polygon                                         │
│  Risk: Max stake per bet, kelly criterion sizing                 │
│  Alerts: Telegram bot notifications                              │
└──────────────────────────────────────────────────────────────────┘
```

---

## Implementační fáze

### ✅ PHASE 1 — Data Infrastructure (HOTOVO)

- [x] WS server (tokio-tungstenite) na portu 8080
- [x] HTTP API server na portu 8081
- [x] FeedEnvelope parsing (v1, live_match/odds/heartbeat)
- [x] SQLite persistence s WAL mode
- [x] JSONL event logging
- [x] Staleness cleanup (120s cutoff)
- [x] Heartbeat summary (10s interval)

### ✅ PHASE 2 — Browser Scraping (HOTOVO)

- [x] HLTV Tampermonkey scraper v2+ (URL slug parsing, TextNode walker)
- [x] Bo3.gg odds scraper v3 (TreeWalker, multi-bookmaker, 36-43 entries)
- [x] WS connection to Feed Hub
- [x] Order-independent match_key normalization

### ✅ PHASE 3 — Opportunities Engine (HOTOVO)

- [x] score_momentum detection
- [x] tight_spread_underdog detection
- [x] arb_cross_book detection (multi-bookmaker)
- [x] /opportunities HTTP endpoint
- [x] Edge sorting (descending)

### ✅ PHASE 4 — Azuro Integration (HOTOVO)

- [x] Platform research: SX Bet ❌, Polymarket ❌, Overtime ❌, Azuro ✅
- [x] GraphQL subgraph query design (CS2 sport slug, Created status, active conditions)
- [x] `azuro_poller.rs` — Rust-native poller module
- [x] Polygon + Gnosis dual-chain polling (30s interval)
- [x] Azuro odds parsing (fixed-point 10^12 → decimal)
- [x] Team extraction (participants + title fallback)
- [x] Match winner condition extraction (2-outcome filter)
- [x] Injection into FeedHubState as `azuro_polygon` / `azuro_gnosis`
- [x] DB logging of Azuro odds
- [x] Cross-platform arb: 1xbit vs azuro works automatically

### 🔄 PHASE 5 — Execution Layer (NEXT)

- [ ] Polygon wallet setup (USDC)
- [ ] ethers-rs / alloy crate pro EIP712 signing
- [ ] Azuro Relayer API integration
- [ ] Bet placement flow: detect arb → sign → submit → confirm
- [ ] Kelly criterion stake sizing
- [ ] Max loss per day limity
- [ ] Telegram alert bot

### 📋 PHASE 6 — Optimization

- [ ] Azuro WebSocket live odds (`wss://streams.onchainfeed.org`) místo 30s polling
- [ ] Team name fuzzy matching cross-platform
- [ ] Azuro liquidity extraction z subgraph
- [ ] Multi-chain optimization (Polygon vs Gnosis vs Base — nejnižší fees)
- [ ] Historical arb edge tracking + profitability reporting

---

## Kde je kód

| Soubor | Účel |
|--------|------|
| `src/feed_hub.rs` | Hlavní binary — WS + HTTP server, opportunities engine |
| `src/azuro_poller.rs` | Azuro GraphQL poller (Polygon + Gnosis) |
| `src/feed_db.rs` | SQLite persistence (WAL mode) |
| `userscripts/hltv_live_scraper.user.js` | HLTV Tampermonkey scraper v2+ |
| `userscripts/odds_scraper.user.js` | Bo3.gg odds scraper v3 |
| `crates/logger/` | JSONL event logging |
| `crates/arb_detector/` | SX Bet cache (legacy, deprecated) |
| `crates/esports_monitor/` | GosuGamers/VLR.gg scrapers (legacy) |
| `crates/prediction_engine/` | Match prediction (legacy) |

---

## Klíčové endpointy

| Endpoint | Popis |
|----------|-------|
| `ws://0.0.0.0:8080/feed` | WS ingest (Tampermonkey → Feed Hub) |
| `http://0.0.0.0:8081/health` | Health check |
| `http://0.0.0.0:8081/state` | Current state (live + odds) |
| `http://0.0.0.0:8081/opportunities` | Detected arb/value opportunities |
| Azuro Polygon subgraph | `https://thegraph.onchainfeed.org/.../azuro-api-polygon-v3` |
| Azuro Gnosis subgraph | `https://thegraph.onchainfeed.org/.../azuro-api-gnosis-v3` |
| Azuro WebSocket | `wss://streams.onchainfeed.org/v1/streams/feed` |
