# Implementační plán

Aktualizováno: **2026-02-26**

## Cíl

Mít konzervativní, auditovatelný a stabilní live-betting pipeline s jasným ranním reportem.

## Kroky

1. **Data ingest stabilita**
   - držet čistý Tipsport feed (v2.3)
   - potlačit noisy externí zdroje

2. **Decisioning stabilita**
   - score-edge auto-bet: 2 USD
   - HIGH odds-anomaly auto-bet: 1 USD
   - dedup ochrany na více úrovních

3. **Settlement stabilita**
   - pravidelný payout check
   - auto-claim + safety-net auto-claim

4. **Reporting**
   - ráno jedním skriptem vyjet přehled:
     - co se koupilo (sport, tým, kurz, stake)
     - proč se koupilo (reason / heuristika)
     - stav (pending/claimable/claimed)
     - orientační P/L

## Poznámka

Tento soubor je plán. Real-time stav je v `AKTUALNI_PROGRESS.md`.