# RustMiskoLive — Implementační plán

# Naposledy aktualizováno: 2026-02-23

# Status: LIVE SCORING IMPLEMENTACE (kritická priorita)

---

## Diagnóza stavu (2026-02-23)

### Co funguje:
- SX Bet background cache sync — 12 mapovaných moneyline matchů každých 60s ✅
- Deduplikace scrapovaných výsledků (Gemini commit `1e471d7`) ✅
- Info logging pro "No cached SX Bet market" ✅
- Telegram alerting pipeline (kód hotový, nikdy nefire-oval) ✅

### Co NEFUNGUJE (root cause):
- **Scrapujeme `/results` stránky** = zápasy dokončené před HODINY
- SX Bet market na tyto staré zápasy už neexistuje → lookup vždy selže
- Za 2 dny provozu: **0 ARB_OPPORTUNITY**, **0 Telegram notifikací**
- Systém je de facto NOP loop

### Co je potřeba:
Přepnout ze scrapování STARÝCH výsledků na sledování LIVE zápasů a detekci momentu dokončení → checknutí SX Bet orderbooku v 10-25min oracle lag window.

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

### 3. CS2 — HLTV.org alternativně GosuGamers
- **HLTV**: `https://www.hltv.org/matches` — má live section nahoře, ale 403 anti-bot
- **GosuGamers fallback**: `https://www.gosugamers.net/counter-strike/matches` — live matches na hlavní stránce (ne /results)
- **Strategie**: Scrapuj matches stránku (ne results), detekuj live → finished transition.

### 4. Dota 2 — GosuGamers
- **URL**: `https://www.gosugamers.net/dota2/matches`
- Stejná strategie jako CS2.

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
