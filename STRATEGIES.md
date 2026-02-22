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

**Co implementovat:**
```rust
// crates/arb_detector/src/odds_api.rs
GET https://api.the-odds-api.com/v4/sports/arbitrage-bets?apiKey=KEY
```

---

### 🟡 Polymarket Esports markets
**Status: NOVÝ NAPAD — viz ESPORTS_PIVOT.md v RustMisko**

CS2/LoL/Valorant matches na Polymarket:
- 50-100 markets/den (vs. 10-20 pro klasické sporty)
- Oracle lag 10-25 minut (šílenec okno)
- Méně botů než v klasickém sportu
- PandaScore free API (1000 req/měsíc)

**Toto je implementace v RustMisko** (ne RustMiskoLive),
protože Polymarket je přístupný z CZ.

---

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
