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

## 2026-02-22 — PHASE 1 logging-only deployment START

**Rozhodnutí:** Spouštíme okamžitě logging-only nasazení i bez plných API klíčů.

**Implementováno:**

- `API_STATUS` event per source/sport v každém poll cyklu
- `SYSTEM_HEARTBEAT` event per cyklus (health + item summary)
- `.env` parametry: `POLL_INTERVAL_SECS`, `MIN_ROI_PCT`

**Proč:**

- Potřebujeme audit trail 24/7 i při částečně nefunkčních zdrojích
- Agenti musí mít vždy up-to-date obraz stavu systému

**Pravidlo změn:**
Každý tuning thresholdů musí být zapsán do `PLAN.md` + `DECISIONS.md` (before/after + důvod + metrika).

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

## 2026-02-22 — Přechod z Polymarketu na SX Bet pro Esports

**Rozhodnutí:** Opouštíme platformu Polymarket a integrujeme Web3 platformu SX Bet na síti Polygon pro monitoring arbitráží.

**Proč:**

- Polymarket trpí nedostatkem trhů a nízkou likviditou u esportů.
- Sázková kancelář SX Bet (AMMs) nabízí více než 60 aktivních turnajů v LoL, CS2 a Valorantu.
- Systém vyrovnávání likvidity prostřednictvím Orakulů disponuje pomalejší odezvou (až 25 minut tzv. Oracle Lag), což představuje obrovský vysoce výnosný prostor bez ohledu na lidský/bot front-running.
- Lze komunikovat anonymně bez KYC a geoblocking restrikcí skrz otevřené REST data endpoints.

**Důsledek:** Kompletní asynchronní `RwLock` cache a Telegram notifikační infrastruktura připojena pro neustálý běh přes `live-observer`.

---

## 2026-02-23 — Multi-Bookie (Azuro) & Headless Bypasses (Phase 3)

**Rozhodnutí:**
Integrace The Graph na Azuro AMM (Polygon), zapojení Headless Chrome pro obcházení aktivní Cloudflare turnstile u příliš zabezpečených webů (GosuGamers) a integrace STRATZ API pro Dota 2. Zavedení reálných Gas oráklů (RPC nodes).

**Proč:**

- SX Bet nedovedl sám o sobě pojmout všechny CS2 a Dota ligy (likvidita rozptýlena). Azuro dává přístup k obřím esport poolům.
- GosuGamers přepnul na Under Attack mód (UAM) u Cloudflare. Běžný `reqwest-impersonate` zlyhal. Bylo nutné nasadit hardwarový sandbox s Chromium (via `headless_chrome`), který reálně zpracuje javascript challenge.
- Aby systém nevytekl na paměť (Memory Leak) přes headless browsery, Dota 2 šla na bezotiskové WebSocket API (Stratz) a Chromium proces se okamžitě ničí přes Drop implementace (Garbage Collection).
- Statické flat fees se vyhozeny a nahrazeny skutečnými RPC dotazy na síťové poplatky na Arbitrum (SX) a Polygon (Azuro).

**Důsledek:** Architektura verifikována a spuštěna do Phase 4 (Simulated Verification). Systém pálí asynchronní fan-out checky obou bookmakerů, modeluje skutečnou slippage na datech.

---

## 2026-02-24 — Priorita: Phase 0 Persistent Browser Node (Win11)

**Rozhodnutí:** Nejdřív dokončit PoC na tomto domácím Win11 zařízení (24/7), kde browser běží trvale, session loginy jsou ručně potvrzené a Rust ingestuje live data z více zdrojů.

**Proč:**

- Aktuální bottleneck není model, ale stabilní dostupnost live dat (challenge stránky, anti-bot)
- Rychlé škálování bez důkazu datové stability by zvyšovalo riziko a debug chaos
- Hardware + ruční onboarding účtů je teď nejrychlejší cesta k funkčnímu end-to-end PoC

**Důsledek:**

- Fáze "profit/scale" je odložena až po splnění Phase 0 KPI (uptime, lag, consensus, join quality)
- Dokumentace a daily status se řídí podle Phase 0 milestone checků

---

## 2026-02-24 — Stop/Go pravidlo před navyšováním stake

**Rozhodnutí:** Žádné navýšení kapitálu ani rozšíření na Android node bez splněných metrik kvality feedu.

**Stop/Go metriky:**

- Feed uptime ≥ 98% (24h)
- p95 lag < 2s
- Konsensus mezi feedy ≥ 80%
- False join rate < 5%

**Důsledek:**

- Micro-live testy mohou běžet, ale scaling je podmíněný datovým důkazem, ne pocitem.

---

## 2026-02-24 — Finanční cíl je směr, ne garance

**Rozhodnutí:** Cíl 2k USD/měsíc je pracovní target, ne garantovaný výsledek.

**Proč:**

- Reálný výsledek závisí na EV po fees/slippage, fill rate a tail risk událostech
- Přecenění jistoty by vedlo k předčasnému risku

**Důsledek:**

- Reporting bude uvádět měřené KPI a net výsledky po nákladech
- Strategická rozhodnutí se dělají na datech z replay/live logu
