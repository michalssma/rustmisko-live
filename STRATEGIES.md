# Strategie

Aktualizováno: **2026-02-26**

## Aktivní produkční strategie

1. **Score-edge (primární)**
   - trigger: live score změna + edge nad limitem
   - auto-bet: **2 USD**
   - cíl: využít zpoždění adjustace kurzů

2. **HIGH odds-anomaly (sekundární, konzervativní)**
   - trigger: HIGH confidence, bounded discrepancy, safety guards
   - auto-bet: **1 USD**
   - cíl: chytat jen čisté, ne-extrémní anomálie

## Risk guardy

- min/max odds guard
- max bets per session
- dedup podle match/condition/base match key
- Telegram fail-safe (alert není „sent“, pokud odeslání selže)

## Co není aktivní strategie

- nápady bez implementace (Betfair/Polymarket/funding arbitráže) jsou backlog, ne live rozhodování.

## Source of truth

- runtime čísla: `AKTUALNI_PROGRESS.md`
- implementace logiky: `src/bin/alert_bot.rs`, `src/feed_hub.rs`, `executor/index.js`