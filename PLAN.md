# RustMiskoLive — Implementační plán

Aktualizováno: 2026-02-24
Status: **PHASE 6 COMPLETE — LIVE EXECUTION**

---

## Architektura (aktuální)

```
┌──────────────────────────────────────────────────────────────────┐
│                        DATA SOURCES                             │
│                                                                  │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────────────┐ │
│  │ HLTV.org    │  │ Bo3.gg       │  │ Azuro Protocol          │ │
│  │ Tampermonkey│  │ Tampermonkey │  │ (Rust GraphQL poller)   │ │
│  │ v3 scraper  │  │ v3 scraper   │  │ 4 chainy, 30s poll      │ │
│  │ auto-refresh│  │ 1xbit odds   │  │                         │ │
│  └──────┬──────┘  └──────┬───────┘  └──────────┬──────────────┘ │
│         │ WS             │ WS                   │ reqwest        │
└─────────┼────────────────┼──────────────────────┼────────────────┘
          ▼                ▼                      ▼
┌──────────────────────────────────────────────────────────────────┐
│                     FEED HUB (Rust, tokio)                      │
│  WS 8080 + HTTP 8081 + SQLite + Azuro poller                    │
└──────────────────────────────┬───────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│                     ALERT BOT (Rust, tokio)                     │
│  Telegram alerts + YES/NO reply handling + confidence scoring   │
└──────────────────────────────┬───────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│                EXECUTOR (Node.js, viem, @azuro-org/toolkit)     │
│  Port 3030 — LIVE MODE — on-chain bet/cashout na Polygon        │
│  Wallet: 0x8226D38e... | 33.77 USDT | UNLIMITED allowance      │
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
- [x] Staleness cleanup (120s)

### ✅ PHASE 2 — Browser Scraping (HOTOVO)
- [x] HLTV v3: auto-refresh, stale detection, countdown, Refresh Now button
- [x] Bo3.gg v3: TreeWalker, multi-bookmaker, 36-43 entries
- [x] WS connection to Feed Hub
- [x] Order-independent match_key normalization

### ✅ PHASE 3 — Opportunities Engine (HOTOVO)
- [x] score_momentum detection
- [x] odds_anomaly detection (formerly tight_spread_underdog)
- [x] arb_cross_book detection (disabled in alerts, covered by odds_anomaly)
- [x] /opportunities HTTP endpoint
- [x] Edge sorting (descending)

### ✅ PHASE 4 — Azuro Integration (HOTOVO)
- [x] Platform research: SX Bet ❌, Polymarket ❌, Overtime ❌, Azuro ✅
- [x] `azuro_poller.rs` — 4 chainy (Polygon, Gnosis, Base, Chiliz)
- [x] CS2 match_winner parsing s conditionId + outcomeId propagací
- [x] Injection jako `azuro_polygon` / `azuro_base` etc.

### ✅ PHASE 5 — Alert Bot (HOTOVO)
- [x] Telegram bot s numbered alerts (#1, #2, ...)
- [x] Confidence scoring (0-100)
- [x] YES parser: `3 YES $5`, `3 YES`, `YES $5`, `YES`
- [x] Executor HTTP integration
- [x] Dry-run vs LIVE detection
- [x] Active bets tracking

### ✅ PHASE 6 — Execution Layer (HOTOVO — LIVE)
- [x] Node.js executor sidecar (`executor/index.js`)
- [x] Azuro V3 bet placement via `@azuro-org/toolkit` + `viem`
- [x] Polygon wallet setup (USDT)
- [x] USDT approval for Azuro Relayer (UNLIMITED)
- [x] /bet, /cashout, /approve, /balance, /health endpoints
- [x] DRY-RUN mode (bez PRIVATE_KEY)
- [x] LIVE mode s reálným private key
- [x] RPC: `https://1rpc.io/matic`

### 📋 PHASE 7 — Optimization (NEXT)
- [ ] Azuro WebSocket live odds (`wss://streams.onchainfeed.org`)
- [ ] Team name fuzzy matching cross-platform
- [ ] Kelly criterion stake sizing
- [ ] Max loss per day limity
- [ ] Multi-chain optimization (Polygon vs Base fees)
- [ ] Historical profitability tracking + reporting
- [ ] Azuro liquidity extraction pro lepší sizing

---

## Kde je kód

| Soubor | Účel |
|--------|------|
| `src/feed_hub.rs` | Feed Hub binary — WS + HTTP + opportunities |
| `src/azuro_poller.rs` | Azuro GraphQL poller (4 chainy) |
| `src/feed_db.rs` | SQLite persistence |
| `src/bin/alert_bot.rs` | Telegram alert bot + executor |
| `executor/index.js` | Node.js executor (Azuro on-chain) |
| `userscripts/hltv_live_scraper.user.js` | HLTV scraper v3 |
| `userscripts/odds_scraper.user.js` | Bo3.gg odds scraper v3 |
| `crates/logger/` | JSONL logging |
