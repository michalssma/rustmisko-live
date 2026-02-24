# AKTUALNI_PROGRESS — handoff pro Sonneta

Aktualizováno: 2026-02-24
Repo: RustMiskoLive (`C:\RustMiskoLive`)

## 🔴 STAV: LIVE PRODUKCE — REÁLNÉ PENÍZE NA POLYGON

### Aktuální priorita

Systém je **PLNĚ FUNKČNÍ a LIVE** — detekuje CS2 arbitráže, posílá Telegram alerty, a po potvrzení (YES) reálně sází na Azuro Protocol (Polygon, USDT). **Executor běží v LIVE režimu s reálnou peněženkou.**

---

### Architektura (aktuální — PRODUKCE)

```
┌─────────────────────────────┐
│   TAMPERMONKEY USERSCRIPTS  │
│                             │
│  HLTV scraper v3            │──── live matches + featured odds
│  (499 lines, auto-refresh)  │     → WS → Feed Hub
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
│    /health, /state, /opps   │
└──────────┬──────────────────┘
           │
           ▼
┌─────────────────────────────┐
│  ALERT BOT (Rust, tokio)    │
│                             │
│  Polls /opportunities 10s   │
│  Telegram alerts s #ID      │
│  Confidence scoring 0-100   │
│  Reply: "YES $5" → executor │
│  Auto-cashout tracking      │
│  Dry-run vs LIVE detection  │
└──────────┬──────────────────┘
           │ HTTP POST
           ▼
┌─────────────────────────────┐
│  EXECUTOR (Node.js, viem)   │
│  Port 3030 — LIVE MODE      │
│                             │
│  /bet    → Azuro on-chain   │
│  /cashout → early cashout   │
│  /approve → USDT allowance  │
│  /balance → wallet balance  │
│  /health  → system status   │
│                             │
│  Wallet: 0x8226D38e...      │
│  Balance: 33.77 USDT        │
│  Chain: Polygon (137)       │
│  Relayer: UNLIMITED approve │
└─────────────────────────────┘
```

---

### Co je hotovo a LIVE v produkci

1. **Feed Hub** — WS server + HTTP API
   - Multi-bookmaker `OddsKey {match_key, bookmaker}` architektura
   - Order-independent `match_key()` normalizace
   - SQLite persistence (WAL mode) + JSONL logging
   - Staleness cleanup (120s)
   - Porty: WS 8080, HTTP 8081

2. **HLTV Tampermonkey scraper v3** (auto-refresh)
   - Auto-refresh každé 3 min (prevence stale DOM)
   - Stale detection (90s bez změny → early refresh)
   - Finished match detection (score ≥13)
   - "Refresh Now" button + countdown timer
   - sessionStorage pro preservování sent count

3. **Bo3.gg odds scraper v3** — TreeWalker, multi-bookmaker

4. **Azuro Protocol integration** — `azuro_poller.rs`
   - 4 chainy: Polygon, Gnosis, Base, Chiliz (30s poll)
   - CS2 games s aktivními podmínkami (match_winner market)
   - Injektuje jako `azuro_polygon` / `azuro_base` etc.

5. **Opportunities Engine** — 3 detekční typy:
   - `score_momentum` — live score ahead, odds lagging
   - `odds_anomaly` — tight spread + underdog value
   - `arb_cross_book` — cross-platform arb (DISABLED v alertech, covered by odds_anomaly)

6. **Alert Bot** (`src/bin/alert_bot.rs`) — Telegram bot
   - Numbered alerts (#1, #2, ...) s confidence score
   - YES parser: `3 YES $5`, `3 YES`, `YES $5`, `YES` (latest)
   - Dry-run vs LIVE detection v Telegram zprávách
   - Active bets tracking + auto-cashout

7. **Executor Sidecar** (`executor/index.js`) — Node.js
   - **LIVE MODE** — reálné on-chain transakce na Polygon
   - Azuro V3 bet placement přes `@azuro-org/toolkit` + `viem`
   - Endpoints: /bet, /cashout, /approve, /balance, /health
   - RPC: `https://1rpc.io/matic`
   - Wallet: `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
   - Balance: **33.77 USDT**
   - Relayer allowance: **UNLIMITED** (approved tx: `0x48cec4ba...`)
   - Podporuje i DRY-RUN mód (bez PRIVATE_KEY)

---

### Wallet & On-Chain Info

| Položka | Hodnota |
|---------|---------|
| Wallet | `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00` |
| Chain | Polygon (137) |
| USDT Contract | `0xc2132D05D31c914a87C6611C10748AEb04B58e8F` |
| USDT Balance | 33.77 |
| POL Balance | ~2.09 (gas) |
| Azuro LP | `0x0FA7FB5407eA971694652E6E16C12A52625DE1b8` |
| Azuro Relayer | `0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d` |
| Relayer Allowance | UNLIMITED |
| RPC | `https://1rpc.io/matic` |

---

### Platformy — vyšetřeno

| Platforma   | CS2 coverage | Status |
|-------------|-------------|--------|
| **Azuro**   | ✅ MASIVNÍ   | **INTEGROVÁNO + LIVE EXECUTION** |
| SX Bet      | ❌ ŽÁDNÉ     | Pouze LoL LPL. Zero CS2. |
| Polymarket  | ❌ ŽÁDNÉ     | Zero esports. |
| Overtime    | ❌ DEPRECATED | Nefunkční. |

---

### Co systém REÁLNĚ dělá teď

```
Continuous loop (LIVE):
  1. Tampermonkey scrapers → WS → Feed Hub (live matches + odds)
  2. Azuro poller → GraphQL → Feed Hub (on-chain CS2 odds)
  3. Alert bot polluje /opportunities každých 10s
  4. Detekce edge → Telegram alert (#N, confidence, doporučení)
  5. Miša odpoví "YES $5" → executor POST /bet → ON-CHAIN Azuro bet
  6. Transakce na Polygon → sledovatelné na polygonscan.com
  7. Auto-cashout monitoring aktivních betů
```

---

### Jak spustit (kompletní)

```powershell
# Terminal 1: Feed Hub
$env:RUST_LOG="info"
$env:FEED_DB_PATH="data/feed.db"
$env:FEED_HUB_BIND="0.0.0.0:8080"
$env:FEED_HTTP_BIND="0.0.0.0:8081"
cargo run --bin feed-hub

# Terminal 2: Executor (LIVE)
cd executor
$env:PRIVATE_KEY="0x..."  # Polygon private key
$env:PORT="3030"
$env:RPC_URL="https://1rpc.io/matic"
node index.js

# Terminal 3: Alert Bot
$env:RUST_LOG="info"
$env:TELEGRAM_BOT_TOKEN="7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk"
$env:TELEGRAM_CHAT_ID="6458129071"
$env:FEED_HUB_URL="http://127.0.0.1:8081"
$env:EXECUTOR_URL="http://127.0.0.1:3030"
.\target\debug\alert_bot.exe

# Chrome: HLTV scraper v3 + Bo3.gg odds scraper v3 v Tampermonkey
```

### Budoucí vylepšení

1. **Azuro WebSocket** — `wss://streams.onchainfeed.org` pro sub-second odds (místo 30s polling)
2. **Team name fuzzy matching** — cross-platform normalizace
3. **Kelly criterion** — automatický stake sizing
4. **Multi-chain optimization** — Polygon vs Base vs Gnosis fees
5. **Azuro liquidity parsing** — lepší confidence skóre

### Poznámka k pravdivosti

Tento soubor popisuje přesný stav systému k 2026-02-24. Systém je LIVE s reálnými penězi. Každý YES = on-chain transakce.
