# RustMiskoLive — Context

# Naposledy aktualizováno: 2026-02-22

# Nový agent: přečti tento soubor → DECISIONS.md → PLAN.md → pak kóduj

## Co je tento projekt

**Primární profit systém.** In-play lag arbitrage na Betfair Exchange + Smarkets.

Sesterský projekt k RustMisko (Polymarket news arb).
Sdílí pouze wallet infrastrukturu — vše ostatní je oddělené.

## Strategie

## Strategie

```
Unofficial LoL API / VLR.gg / GosuGamers (Scraping & Free Data)
        ↓
Esports Monitor — detekuje právě ukončené CS2/LoL/Valorant zápasy
        ↓
ArbDetector — detekuje lag na Web3 SX Bet esports trzích (10–25 minut). Uloží si 64 aktivních lig do 16µs RwLock Cache.
        ↓
Resolver — risk check
        ↓
OBSERVE 48h → pak Telegram Alert → Executor (live bets)
```

## Aktuální stav — FÁZE 1 (Production Ready) ✅

- [x] Observer plně napojen na VLR.gg (HTML scraping), GosuGamers a neoficiální LoL API.
- [x] Obří pivot z Polymarketu (který neměl likviditu) na SX.bet.
- [x] Process Safety — implementace `fd-lock` brání běhu dvou instancí.
- [x] Telegram Notifikace po nalezení validního edge.
- [x] JSONL eventy pro Esports (`MATCH_RESOLVED`).

## Klíče v .env (NIKDY necommitovat)

```
ESPORTS_POLL_INTERVAL_SECS=15
TELEGRAM_BOT_TOKEN=8125729036:...
TELEGRAM_CHAT_ID=...
```

## Soubory

```
RustMiskoLive/
├── PLAN.md          ← checkpointy, architektura, edge typy — ČTĚTE PRVNÍ
├── DECISIONS.md     ← všechna rozhodnutí s důvody
├── CONTEXT.md       ← tento soubor
├── src/main.rs      ← orchestrátor
├── crates/
│   ├── esports_monitor/   ← Esports (CS2/LoL/Valo)
│   ├── arb_detector/    ← edge kalkulace
│   └── logger/          ← JSONL + NTFY alerts
├── logs/            ← YYYY-MM-DD.jsonl
└── .env             ← secrets (v .gitignore)
```

## AI náklady

ŽÁDNÉ AI v real-time path. Pouze offline denní report (~$0.10–0.50/den).
Viz PLAN.md sekce "AI v pipeline".

## Vztah k RustMisko

|           | RustMisko  | RustMiskoLive              |
| --------- | ---------- | -------------------------- |
| Platforma | Polymarket | SX.bet (Polygon)           |
| Edge typ  | News lag   | Esports in-play Oracle Lag |
| Frekvence | 0–3/týden  | 5–20/den                   |
| Priorita  | Sekundární | **Primární**               |
