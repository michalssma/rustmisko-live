# RustMiskoLive — Context
# Naposledy aktualizováno: 2026-02-22
# Nový agent: přečti tento soubor → DECISIONS.md → PLAN.md → pak kóduj

## Co je tento projekt

**Primární profit systém.** In-play lag arbitrage na Betfair Exchange + Smarkets.

Sesterský projekt k RustMisko (Polymarket news arb).
Sdílí pouze wallet infrastrukturu — vše ostatní je oddělené.

## Strategie

```
ESPN detekuje gól/score change (5s poll, zdarma)
        ↓
Betfair Stream API / Smarkets WebSocket — aktuální kurzy
        ↓
ArbDetector — 3 typy edge:
  1. In-play lag: ESPN ví o gólu, exchange ještě nezareagoval (15–60s)
  2. Cross-exchange: Betfair vs. Smarkets price mismatch
  3. Small league: sharp books (Pinnacle) vs. exchange
        ↓
Resolver — risk check (min 2%, max $300, circuit breaker)
        ↓
OBSERVE 48h → pak Executor (live bets)
```

## Aktuální stav — CHECKPOINT 0 ✅

- [x] PLAN.md s checkpointy
- [x] DECISIONS.md
- [x] Adresářová struktura
- [x] Cargo.toml workspace
- [ ] **NEXT: CHECKPOINT 1** — Betfair Stream + Smarkets WebSocket price_monitor

## Co čeká na tebe (člověka)

1. **Smarkets signup** → API key → do `.env` jako `SMARKETS_API_KEY=xxx`
2. **Betfair signup** → developer.betfair.com → AppKey → do `.env` jako `BETFAIR_APP_KEY=xxx`
3. Pak říct agentovi: "pust se do checkpointu 1"

## Klíče v .env (NIKDY necommitovat)

```
BETFAIR_APP_KEY=
BETFAIR_USERNAME=
BETFAIR_PASSWORD=
SMARKETS_API_KEY=
ODDSAPI_KEY=edf29a96be1a0f82a5f2507494e05f88d4d1508912fd54d2878c187767247b13
```

## Soubory

```
RustMiskoLive/
├── PLAN.md          ← checkpointy, architektura, edge typy — ČTĚTE PRVNÍ
├── DECISIONS.md     ← všechna rozhodnutí s důvody
├── CONTEXT.md       ← tento soubor
├── src/main.rs      ← orchestrátor
├── crates/
│   ├── price_monitor/   ← Betfair Stream + Smarkets WebSocket
│   ├── arb_detector/    ← edge kalkulace (Typ 1, 2, 3)
│   └── logger/          ← JSONL + NTFY alerts
├── logs/            ← YYYY-MM-DD.jsonl
└── .env             ← secrets (v .gitignore)
```

## AI náklady

ŽÁDNÉ AI v real-time path. Pouze offline denní report (~$0.10–0.50/den).
Viz PLAN.md sekce "AI v pipeline".

## Vztah k RustMisko

| | RustMisko | RustMiskoLive |
|---|---|---|
| Platforma | Polymarket | Betfair + Smarkets |
| Edge typ | News lag | In-play lag + cross-exchange |
| Frekvence | 0–3/týden | 5–20/den |
| Priorita | Sekundární | **Primární** |
