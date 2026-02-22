# RustMiskoLive — Projednané strategie

# Naposledy aktualizováno: 2026-02-22

# Stav: Čekáme na vyřešení geo/API blokád

---

## ZAMÍTNUTO / BLOKOVÁNO

### ❌ Betfair Exchange (in-play lag arb)

**Blokováno: CZ geoblocking**

Betfair.com i developer.betfair.com hlásí "Czech Republic unavailable".
Stream API (sub-second odds) je technicky ideální pro in-play lag arb.

**Co čeká:** Betfair je dostupný přes VPN (UK server).
Pokud se dostaneme na API: implementovat `crates/price_monitor/src/betfair.rs`.

**API docs:** https://developer.betfair.com/en/betfair-exchange-api/

---

### ❌ Smarkets (cross-exchange arb)

**Blokováno: CZ 404 / country restriction**

smarkets.com/register vrací 404 z CZ.
Výhoda: 2% commission (vs. Betfair 5%) → ideální pro cross-exchange arb s Betfairem.

**Co čeká:** Smarkets má UK sídlo — VPN UK nebo EU právní entity může pomoci.

---

### ❌ Pinnacle API (sharp line benchmark)

**Blokováno: Vyžaduje auth (401)**

Pinnacle nabízí free read-only API podle dokumentace, ale endpoint vrací 401
bez Basic auth credentials. Přístup vyžaduje funded Pinnacle account.

**Použití:** Sharp line benchmark pro Type 3 arb (small league mispricing).
**Alternativa:** odds-api.io má `/arbitrage-bets` endpoint (100 req/h free).

---

### ❌ OddsPortal scraping

**ZAMÍTNUTO: Fragile + ToS problém**

Návrh byl scraping OddsPortal pro historická odds data.
Rozhodnutí: NIKDY nescraping — nestabilní, ToS violation, možný ban.

---

### ❌ Tipsport.cz API

**Zamítnuto: Interní API, bez dokumentace**

Tipsport nemá veřejné API. Interní API endpoints jsou obfuskované a mění se.
Risk: ban účtu při detekci automatizace.

---

## MOŽNÉ CESTY (čeká na průzkum)

### 🟡 VPN + Betfair / Smarkets

**Status: Neotestováno**

UK VPN by měl odemknout Betfair i Smarkets.
Risk: ToS Betfairu zakazuje VPN přístup (Section 6.3).
Nutné právní posouzení nebo UK entity.

---

### 🟡 odds-api.io (Type 3 edge — small league)

**Status: API key k dispozici, neotestováno v produkci**

```
ODDSAPI_KEY=edf29a96be1a0f82a5f2507494e05f88d4d1508912fd54d2878c187767247b13
```

Endpoint `/arbitrage-bets` vrací hotové arb příležitosti.
Omezení: 100 req/h na free tier, nezahrnuje in-play data.

---

### ✅ SX.bet (Esports in-play / Oracle Lag) -> THE WINNING STRATEGY

**Status: NASAZENO A PRODUKČNÍ (Background `live-observer`)**

Pivot od Polymarketu (který neměl dostatek Volume v esportech) k Web3 sázkovce SX.bet na síti Polygon.

- Žádné KYC, zero geo-blocking.
- **Obří Oracle Lag:** 10-25 minut (sázkovka čeká na potvrzovací nody pro vyplacení sázek, my reagujeme v milisekundách na reálný výsledek z VLR.gg/GosuGamers).
- **Background Sync Cache:** `ArbDetector` cachuje všech ~64 aktivních esports lig v intervalu 1 minuty do `RwLock`.
- Match-up Resolution trvá 16µs (cache hit) a celkový ping na SX zjišťující hranu (Edge) bere pod 330ms.

Tento přístup využívá hlouposti opožděných market-makerů na SX Betu a dává botovi obrovský funkční náskok s notifikacemi rovnou na Telegram.

### 🟡 Matchbook Exchange

**Status: Neotestováno**

UK/EU betting exchange, možná CZ přístupný.
Commission ~2%, méně botů než Betfair.

---

## Závěr: Aktuální priorita

```
PRIMÁRNÍ:  RustMisko (Polymarket) — news lag arb (geopolitika) + esports arb
SEKUNDÁRNÍ: RustMiskoLive — čeká na přístup k Betfair/Smarkets nebo Matchbook
```

RustMiskoLive je připraven architektonicky (PLAN.md checkpointy 1-5),
ale nemůžeme spustit bez přístupu k exchange.

Jakmile bude přístup k exchange, spustit CHECKPOINT 1 (price_monitor).
