# RustMiskoLive — Implementační plán

# Naposledy aktualizováno: 2026-02-24

# Status: PHASE 4 - SIMULATED VERIFICATION (LIVE EXPERIMENT)

---

## Diagnóza stavu (2026-02-24)

### Co funguje (Vše naimplementováno):

- Přechod na striktní webhook/live state-machine (NE scrapování starých /results).
- Asynchronní SX Bet Orderbook Sweeping (vypočítána reálná slippage za $100).
- Asynchronní Azuro The Graph (Polygon AMM) GraphQL parsing s 1.5% likviditní penalizací.
- Plovoucí live RPC Network Fees pro Arbitrum i Polygon odečítající se z Net Edge!
- Riot Games Rate Limiter (TokenBucket `<0.8 calls/sec`) a adaptivní Sniper mód plošně.
- Headless Chrome pro GosuGamers (CS2 bot-bypass oblafnutí a Auto-Garbage Collection).
- Dota 2 běží čistě přes WebSockets (STRATZ API) pro absolutní eliminaci spotřeby paměti.

### Co je potřeba:

Sledovat v produkčním `observe_only = true` režimu, zachytit první ostré spread edge a validovat, jestli 1% Net Marže uvízne v logu na živém e-sportovém matchi než bot narazí na jakýkoliv skrytý bug.

---

## Architektura Live Scoring

### Koncept: State Machine per Match

```
NEZNÁMÝ → LIVE (detekován na live stránce) → JUST_FINISHED (zmizel z live / state=completed) → EVALUATED (SX Bet check proveden)
```

Klíčový moment je přechod `LIVE → JUST_FINISHED`. V tu vteřinu voláme `arb.evaluate_esports_match()`.

### Nový data flow

```
┌────────────────────────┐
│   LIVE MATCH SOURCES   │
│                        │
│  LoL: getSchedule API  │──── state: "inProgress" → "completed"
│  (JSON, 15s poll)      │
│                        │
│  Valorant: vlr.gg      │──── /matches stránka, live section
│  (HTML scrape, 30s)    │
│                        │
│  CS2: HLTV/GosuGamers  │──── /matches stránka, live section
│  (HTML scrape, 30s)    │
│                        │
│  Dota2: GosuGamers     │──── /matches stránka, live section
│  (HTML scrape, 30s)    │
└──────────┬─────────────┘
           │
           ▼
┌────────────────────────┐
│  EsportsMonitor        │
│  live_matches: HashMap │─── pamatuje si LIVE zápasy
│                        │
│  Detekuje přechod:     │
│  LIVE → FINISHED       │
│  = NOVÝ výsledek!      │
└──────────┬─────────────┘
           │ Vec<MatchResolvedEvent>
           ▼
┌────────────────────────┐
│  ArbDetector           │
│  SX Bet cache lookup   │──── market_hash → orderbook → edge calc
│  Telegram alert        │
└────────────────────────┘
```

---

## Datové zdroje — detaily

### 1. LoL — `getSchedule` API ⭐ PRIORITA (nejsnazší)

- **URL**: `https://esports-api.lolesports.com/persisted/gw/getSchedule?hl=en-US`
- **Header**: `x-api-key: 0TvQnueqKa5mxJntVWt0w4LpLfEkrV1Ta8rQBb9Z`
- **State field**: `events[].state` = `"unstarted"` | `"inProgress"` | `"completed"`
- **Team names**: `events[].match.teams[0].name`, `events[].match.teams[1].name`
- **Winner**: `events[].match.teams[N].result.outcome` = `"win"`
- **Strategie**: Poll každých 15s. Trackuj `inProgress` zápasy. Jakmile zmizí z inProgress nebo přejdou na `completed`, emituj resolved event.

### 2. Valorant — vlr.gg `/matches` ⭐

- **URL**: `https://www.vlr.gg/matches` (NE /matches/results!)
- **Live indikátor**: `a.match-item` s live score (ne countdown). Pravděpodobně class `.mod-live` na match itemu.
- **Strategie**: Scrapuj /matches, identifikuj live zápasy (mají score místo countdown). Trackuj je. Jakmile zmizí ze stránky nebo se přesunou na results → resolved.

### 3. CS2 — GosuGamers / HLTV alternativy s Bypass ochranou

- **Cloudflare blokátor**: Obyčejný Scraping nefunguje.
- **Strategie**: Spawnutí Micro-Browseru přes `headless_chrome`. Sandboxing procesů načte stránku a počká na rendering React DOMu, zkopíruje kód a browser ihned zabije proces k zajištění minimalizace RAM memory leaků.

### 4. Dota 2 — STRATZ API WebSockets ⭐

- **URL**: `wss://api.stratz.com/graphql`
- **Strategie**: Zero-memory stream namísto periodického HTML scrapování GosuGamers. Odpozoruje GraphQL eventy o konci zápasu. Pro backend i sídelní servery takřka nulové zatížení.

---

## Implementační kroky

### Krok 1: Přidat `LiveMatchState` tracking do `EsportsMonitor`

Nový struct `LiveMatchState` + `HashMap<String, LiveMatchState>` v monitoru.
State enum: `Live { first_seen, teams, sport }` → `JustFinished { winner }` → `Evaluated`

### Krok 2: Implementovat `poll_live_lol()`

Nejsnazší — čistý JSON API. Volat `getSchedule`, filtrovat `inProgress` a `completed` eventy. Porovnat s předchozím stavem.

### Krok 3: Implementovat `poll_live_valorant()`

Scrapnout `vlr.gg/matches` (ne /results). Parsovat live zápasy. Detekovat transition.

### Krok 4: Implementovat `poll_live_cs2()` a `poll_live_dota2()`

GosuGamers `/matches` stránka pro oba.

### Krok 5: Nový `poll_live_all()` v monitoru

Agreguje všechny live polly. Vrací jen NOVĚ dokončené zápasy.

### Krok 6: Upravit `main.rs`

Primární loop volá `poll_live_all()`. Stávající `poll_all()` (results scraping) běží jen jako audit/fallback jednou za 5 minut.

### Krok 7: Cleanup `seen_matches`

Periodicky čistit (max 500 entries, FIFO) aby nerostla paměť.

---

## Kde je kód

- `crates/esports_monitor/src/lib.rs` — scraping + live state tracking
- `crates/arb_detector/src/lib.rs` — SX Bet cache + edge detection
- `crates/logger/src/lib.rs` — event types
- `src/main.rs` — main loop

## Stará architektura (pro referenci)

### TYP 1: SX Bet Oracle Lag (PRIMARY)

```
Scraper detekuje konec zápasu → SX Bet contract stále přijímá sázky (oracle lag 10-25 min)
→ Edge = 1.0 - best_available_prob (protože výsledek je 100% jistý)
```

### TYP 2-3: Cross-exchange arb, Small league mispricing

Zatím neimplementováno. `price_monitor` crate je dead code.
