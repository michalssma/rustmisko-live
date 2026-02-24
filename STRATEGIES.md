# RustMiskoLive — Projednané strategie

Aktualizováno: 2026-02-24
Stav: **AZURO PROTOCOL = LIVE EXECUTION PLATFORMA**

---

## ✅ AKTIVNÍ STRATEGIE (LIVE)

### ✅ Azuro Protocol (Cross-Platform CS2 Arb) — LIVE EXECUTION

**Status: LIVE — reálné sázky na Polygon**

Decentralizovaný on-chain bookmaker na Polygon. **MASIVNÍ CS2 pokrytí** — desítky zápasů denně.

**Proč Azuro vyhrává:**
- **NO KYC** — wallet-only, žádné geo-blocky
- **Polygon USDT** — nízké gas fees
- **GraphQL API** — structured data, spolehlivé
- **AMM pool** — odds driven by liquidity pool
- **Cross-platform arb** — 1xbit/hltv odds vs azuro on-chain odds
- **Automated execution** — EIP712 → Relayer → on-chain

**Technické detaily:**
- Subgraph: `thegraph-1.onchainfeed.org` (data-feed)
- CS2 sport: `id: 1061`
- RPC: `https://1rpc.io/matic`
- Wallet: `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
- Balance: 33.77 USDT
- Relayer: UNLIMITED allowance

**Implementováno v kódu:**
- `src/azuro_poller.rs` — GraphQL poller, 30s interval, 4 chainy
- `executor/index.js` — Node.js bet/cashout execution
- `src/bin/alert_bot.rs` — Telegram alerts + YES→bet flow

---

### ✅ Tampermonkey + Feed Hub (Data Fusion) — LIVE

**Status: PRODUKČNÍ**

- HLTV scraper v3 (auto-refresh, stale detection)
- Bo3.gg odds scraper v3 (multi-bookmaker)
- Feed Hub: WS 8080 + HTTP 8081

---

## ❌ ZAMÍTNUTO

### ❌ SX Bet
**ZAMÍTNUTO: ZERO CS2 markets.** Pouze LoL LPL (2 zápasy).

### ❌ Polymarket
**ZAMÍTNUTO: ZERO esports.** Pouze politika/geopolitika.

### ❌ Overtime / Thales
**ZAMÍTNUTO: DEPRECATED.** API nefunkční.

### ❌ Betfair Exchange
**BLOKOVÁNO: CZ geoblocking.** Vyžaduje UK VPN + UK entity.

### ❌ Smarkets
**BLOKOVÁNO: CZ 404.**

### ❌ Pinnacle API
**BLOKOVÁNO: 401 bez auth.**

---

## 📋 BUDOUCÍ ROZŠÍŘENÍ

### 🟡 Azuro WebSocket (Live Odds Stream)
`wss://streams.onchainfeed.org/v1/streams/feed` — sub-second odds místo 30s polling.

### 🟡 Kelly Criterion Stake Sizing
Automatický výpočet optimální velikosti sázky na základě edge a bankrollu.

### 🟡 Multi-Chain Optimization
Porovnání fees: Polygon vs Base vs Gnosis — automatický výběr nejlevnějšího chainu.

---

## Závěr

```
PRIMÁRNÍ:  Azuro Protocol × Tampermonkey odds → LIVE cross-platform CS2 arb
LIVE:      33.77 USDT na Polygon, executor běží, alerty fungují
NEXT:      WebSocket live odds + Kelly criterion sizing
```
