# CONTEXT.md — Systémový kontext pro RustMiskoLive

Aktualizováno: 2026-02-24

## Co tento projekt dělá

RustMiskoLive je **automatizovaný CS2 esports arbitrážní systém**. Detekuje cenové rozdíly (edge) mezi tradičními bookmakery (1xbit, HLTV featured) a decentralizovanou platformou Azuro Protocol (on-chain, Polygon). Při nalezení příležitosti pošle alert na Telegram; uživatel potvrdí a systém reálně sází on-chain.

## Status: 🔴 LIVE PRODUKCE

Systém běží s reálnými penězi na Polygon (33.77 USDT). Executor je v LIVE režimu.

## Klíčové komponenty

| Komponenta | Tech | Port | Status |
|---|---|---|---|
| Feed Hub | Rust, tokio | WS 8080, HTTP 8081 | ✅ LIVE |
| Alert Bot | Rust, tokio | — | ✅ LIVE |
| Executor | Node.js, viem | 3030 | ✅ LIVE |
| HLTV scraper | Tampermonkey v3 | — | ✅ LIVE |
| Bo3.gg scraper | Tampermonkey v3 | — | ✅ Ready |
| Azuro poller | Rust (in feed-hub) | — | ✅ LIVE |

## Kde je kód

| Soubor | Účel |
|--------|------|
| `src/feed_hub.rs` | Hlavní binary — WS + HTTP server, opportunities engine |
| `src/azuro_poller.rs` | Azuro GraphQL poller (4 chainy) |
| `src/feed_db.rs` | SQLite persistence (WAL mode) |
| `src/bin/alert_bot.rs` | Telegram alert bot + executor integration |
| `executor/index.js` | Node.js executor sidecar (Azuro bet/cashout) |
| `userscripts/hltv_live_scraper.user.js` | HLTV Tampermonkey scraper v3 |
| `userscripts/odds_scraper.user.js` | Bo3.gg odds scraper v3 |
| `crates/logger/` | JSONL event logging |

## Wallet

- Address: `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
- Chain: Polygon (137)
- Token: USDT (`0xc2132D05D31c914a87C6611C10748AEb04B58e8F`)
- Balance: 33.77 USDT + ~2.09 POL (gas)
- Azuro Relayer: approved UNLIMITED

## Azuro Protocol

- Typ: Decentralizovaný on-chain bookmaker (AMM pool)
- KYC: ŽÁDNÉ — wallet-only
- Subgraph: `thegraph-1.onchainfeed.org` (data-feed, NE client!)
- Chainy: Polygon, Gnosis, Base, Chiliz
- Bet flow: EIP712 → Relayer → on-chain
- Frontend: bookmaker.xyz
- RPC: `https://1rpc.io/matic`
