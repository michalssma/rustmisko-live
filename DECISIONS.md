# RustMiskoLive — Decision Log

Nový agent: přečti CONTEXT.md → pak tento soubor → pak kóduj.

---

## 2026-02-22 — Architektura: samostatný repo

**Rozhodnutí:** Nový standalone repo, ne branch ani crate uvnitř RustMisko.

**Proč:** Dva roboti poběží věčně paralelně. Oddělené CONTEXT.md + DECISIONS.md zabraňují zmatení agentů.

---

## 2026-02-22 — Datové zdroje

**Rozhodnutí:** Priorita zdrojů:
1. Pinnacle API (free read-only) — sharp line benchmark
2. odds-api.io (100 req/h free) — /arbitrage-bets endpoint
3. Polymarket CLOB (zdarma) — cílový trh pro execution

**Scraping: NIKDY** — nestabilní, ToS problém.

---

## 2026-02-22 — 48h observe first

**Rozhodnutí:** První 2 dny = observe only. Žádné ordery.

**Kritéria pro přechod:**
- Pinnacle poll funguje (logy obsahují PINNACLE_LINE eventy)
- odds-api.io vrátí >0 arb příležitostí za 48h
- Edge po odečtení Polymarket fees (2%) stále >1%

---

## 2026-02-22 — Primární strategie: In-play lag arb na Betfair + Smarkets

**Rozhodnutí:** In-play lag arb je primární edge typ.

**Mechanismus:**
ESPN (free, 5s poll) detekuje score change → Betfair/Smarkets cena stále stará → 15–60s okno → edge

**Tři typy edge:**
1. In-play lag (primární) — ESPN gól → exchange lag
2. Cross-exchange arb (sekundární) — Betfair vs. Smarkets mismatch
3. Small league mispricing (bonus) — sharp books vs. exchange

**AI: ŽÁDNÉ v hot path.** Cost/latence zabíjí edge. Pouze offline denní report.

**Checkpointy:** viz PLAN.md

---

## 2026-02-22 — Betfair jako primární exchange, Smarkets jako sekundární

**Rozhodnutí:** Betfair = primární (větší likvidita, Stream API), Smarkets = sekundární (nižší commission 2%).

**Proč:**
- Betfair má Stream API (WebSocket, sub-second) — ideální pro in-play
- Betfair má větší likviditu na malých ligách
- Smarkets levnější commission (2% vs 5%) → pro cross-exchange arb

**Klíče potřebné:**
- `BETFAIR_APP_KEY` + `BETFAIR_SESSION_TOKEN` (developer.betfair.com)
- `SMARKETS_API_KEY` (docs.smarkets.com)

---
