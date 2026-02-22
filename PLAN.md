# RustMiskoLive — Implementační plán
# Naposledy aktualizováno: 2026-02-22
# Status: PHASE 1 LOGGING-ONLY NASAZENO

## Aktuálně nasazeno (PHASE 1)

- Binárka: `cargo run --bin live-observer`
- Režim: observe-only, bez exekuce orderů
- Log stream: JSONL eventy v `logs/YYYY-MM-DD.jsonl`
- Nové eventy:
      - `API_STATUS` — stav každého zdroje/sportu po pollu
      - `SYSTEM_HEARTBEAT` — souhrn cyklu (healthy sources, item count)
      - `ODDS_API_ARB` / `PINNACLE_LINE` — datové eventy (pokud dorazí)
- Runtime thresholdy (editovatelné přes `.env`):
      - `POLL_INTERVAL_SECS`
      - `MIN_ROI_PCT`

## Iterační pravidlo (POVINNÉ)

Každá změna prahů nebo logiky musí být zapsána sem + do `DECISIONS.md`:

1. before → after
2. důvod změny
3. očekávaný dopad
4. metrika ověření po 24h

Bez zápisu se změna nepovažuje za validní.

## Přehled architektury

```
ESPN live scores (free, neomezené)
        │
        ▼
  EventMonitor          ← detekuje GÓLOVÉ/BODOVÉ eventy (ne konec zápasu)
  (15s poll)
        │ InPlayEvent (gól, koš, set)
        ▼
  PriceMonitor          ← Betfair Exchange API + Smarkets API (WebSocket)
  (sub-1s update)       ← zjistí aktuální kurzy NA OBOU platformách
        │
        ▼
  ArbDetector           ← 3 typy edge (viz níže), ŽÁDNÉ AI v hot path
        │ signal
        ▼
  Resolver              ← risk check (min 2%, max $300, circuit breaker)
        │
        ▼
  OBSERVE LOG + NTFY    ← 48h observe, pak executor
```

---

## Tři typy edge — seřazeny dle priority

### TYP 1: In-play lag arb (PRIMARY — nejvyšší frekvence)
```
ESPN detekuje gól/koš → Betfair cena ještě nezareagovala → 15–60s okno
Příklad: Gól v 70. min → "Chelsea win" skočí z 1.8 na 1.3
         ale Betfair stále nabízí 1.75 → edge 2.7%
Frekvence: 3–15 příležitostí/den
Riziko: Nízké (výsledek je jistý fakt)
```

### TYP 2: Cross-exchange arb (SECONDARY)
```
Betfair nabízí Chelsea 2.05, Smarkets nabízí Chelsea 1.95
→ lay Chelsea na Smarkets + back Chelsea na Betfair = garantovaný profit
Frekvence: 1–5/den (závisí na počtu sledovaných trhů)
Riziko: Střední (musíš mít účet + likviditu na OBOU platformách)
Poznámka: Vyžaduje kapitál na obou platformách najednou
```

### TYP 3: Small league mispricing (BONUS)
```
Fortuna liga, Extraliga, nižší fotbalové ligy
Betfair/Smarkets vs. sharp books (Pinnacle via odds-api.io)
Menší boti → větší okno → edge 1–4%
Frekvence: 2–8/den
```

---

## Checkpointy — kdy co commitovat

### ✅ CHECKPOINT 0 — DONE (tento commit)
- [x] PLAN.md vytvořen
- [x] DECISIONS.md aktualizován (pivot od Polymarket ke Smarkets/Betfair)
- [x] RustMisko config.toml aktualizován (news markets)
- [x] Adresářová struktura RustMiskoLive existuje

### 🔲 CHECKPOINT 1 — Betfair + Smarkets price_monitor scaffold
Soubory: `crates/price_monitor/src/betfair.rs`, `crates/price_monitor/src/smarkets.rs`
Co dělá: Připojí se na Betfair Stream API + Smarkets WebSocket, loguje raw odds
Kritérium: `cargo build` projde, log obsahuje PRICE_UPDATE eventy
Commit: `"feat: price_monitor — Betfair Stream + Smarkets WebSocket"`

### 🔲 CHECKPOINT 2 — ESPN in-play event detection
Soubory: `crates/event_monitor/src/lib.rs` (nový, sport-based)
Co dělá: ESPN scoreboard poll každých 5s, detekuje SCORE_CHANGE eventy
Kritérium: Log obsahuje `SCORE_CHANGE { home_score: 1, away_score: 0, minute: 34 }`
Commit: `"feat: event_monitor — ESPN in-play score change detection"`

### 🔲 CHECKPOINT 3 — ArbDetector (Typ 1 + Typ 2)
Soubory: `crates/arb_detector/src/lib.rs`
Co dělá: Spojí score_change event s aktuální cenou → vypočítá edge
Kritérium: Log obsahuje ARB_OPPORTUNITY event s reálnými daty
Commit: `"feat: arb_detector — in-play lag + cross-exchange edge detection"`

### 🔲 CHECKPOINT 4 — 48h OBSERVE run
Co dělá: Celý pipeline běží, NTFY alertuje při edge, žádné ordery
Kritérium: Za 48h min. 10× ARB_OPPORTUNITY v logu
Data: Průměrný lag, průměrný edge%, nejlepší sport/liga
Commit: `"data: 48h observe results — X opportunities, Y avg edge"`
→ **ROZHODNUTÍ: zapnout executor nebo pivotovat**

### 🔲 CHECKPOINT 5 — Executor (pouze po zeleném CP4)
Soubory: `crates/executor/src/betfair.rs`, `crates/executor/src/smarkets.rs`
Co dělá: Zadává live ordery na Betfair/Smarkets
Start: max $50 notional, max 3 open pozice
Commit: `"feat: executor — live betting Betfair/Smarkets (Phase 3)"`

---

## AI v pipeline — ANO nebo NE?

**Rozhodnutí: ŽÁDNÉ AI v hot path (real-time rozhodování)**

Důvod:
- Latence AI API (OpenRouter) = 200–2000ms → zabije in-play okno (15–60s)
- Cost: 100 trades/den × API call = $5–20/den zbytečně
- In-play lag arb NEPOTŘEBUJE AI — edge je matematický fakt (cena - fair value)

**AI použití MIMO hot path (offline analytika):**
- Denní report: shrnutí P&L, nejlepší sporty/ligy
- Kalibrace keyword tabulky pro Polymarket news arb
- Detekce anomálií v historických datech (jednou za týden)
- Cost: $0.10–0.50/den

---

## Spektrum sportů a trhů

### Betfair Exchange — denní pokrytí
```
Sport              Trhy/den    In-play okno    Priorita
─────────────────────────────────────────────────────
Fotbal (global)    200–400     15–90s po gólu  ★★★★★
Basketball NBA     30–50       5–15s po koši   ★★★★☆
Tenis ATP/WTA      50–100      5–20s po setu   ★★★★☆
Hockey NHL/Ekl     20–40       10–30s po gólu  ★★★★☆
Baseball MLB       15–30       pomalejší       ★★★☆☆
Formule 1          5–15        jiný typ edge   ★★★☆☆
```

### Malé ligy (Typ 3 edge) — méně botů
```
Fortuna liga (CZ)     3–4 zápasy/kolo
Tipsliga (SK)         3–4 zápasy/kolo
Extraliga hokej (CZ)  4–6 zápasů/den v sezóně
Erste liga (CZ)       menší coverage
Nižší fotbal EU       stovky zápasů/den
```

---

## Náklady celkového systému

```
Betfair API:    ZDARMA (platíš jen commission 5% na výhry)
Smarkets API:   ZDARMA (platíš jen commission 2% na výhry)
ESPN API:       ZDARMA neomezené
Pinnacle:       ZDARMA read-only (pro cross-check)
odds-api.io:    ZDARMA 100 req/hod
OpenRouter AI:  $0.10–0.50/den (jen offline analytika)
Server:         Tvůj lokální počítač (žádné VPS náklady)
─────────────────────────────────────────────────────
CELKEM fixní:   $0/den
Variabilní:     Commission na výherní trady (2–5%)
```

---

## Kde teď jsme

**CHECKPOINT 0 dokončen.**
Čekám na:
1. Smarkets signup (ty děláš)
2. Betfair signup + AppKey (viz níže)
3. Pak začínám CHECKPOINT 1

## Jak získat Betfair AppKey

1. Registrace: betfair.com (CZ přístupné)
2. Developer Portal: developer.betfair.com → "My Account" → "API Keys"
3. Delay Key (free, bez depositu) → pro čtení trhů
4. Live Key (vyžaduje funded account) → pro placing betů
5. Do .env: `BETFAIR_APP_KEY=xxx` + `BETFAIR_SESSION_TOKEN=xxx`

## Jak získat Smarkets API key

1. smarkets.com/register → "Developer" account
2. docs.smarkets.com → Authentication → API token
3. Do .env: `SMARKETS_API_KEY=xxx`
