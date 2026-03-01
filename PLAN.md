# Implementační plán

Aktualizováno: **2026-03-01**

## Cíl

Mít konzervativní, auditovatelný a stabilní live-betting pipeline s jasným ranním reportem.

## Co je HOTOVO ✅

1. **Data ingest stabilita**
   - Tipsport feed v3.0 (detailed_score, live odds)
   - Fortuna scraper v3.2 (draw filter, 92.5% kvalita)
   - HLTV v3.1, Chance v1.1, FlashScore
   - 5-phase matching fix (NFKD, country translate, token-subset)

2. **Decisioning stabilita**
   - Path A: Score-edge auto-bet ($3/$1 sport-dependent)
   - Path B: Odds anomaly auto-bet ($2, 2+ sources)
   - 6 safety layers (filters → dedup → exposure → data quality → settlement → streak)
   - WS State Gate (pre-flight condition Active check)

3. **Settlement stabilita**
   - Auto-claim safety-net (60s loop)
   - Azuro relayer handles 99%+ claims automatically
   - Created→follow-up polling na všech 3 bet paths

4. **Observabilita**
   - FUSION_MISS logging, NORM_TRACE sampling
   - Permanent ledger (data/ledger.jsonl)
   - Telegram alerting for all events

## Co je NEXT 🔲

1. **Rozšiřování scraper coverage**
   - 1xbit scraper (všechny sporty)
   - Další booky pro lepší cross-validation

2. **Per-sport exposure tuning**
   - Config file pro feature flags (teď hardcoded)
   - Bankroll growth: $46→$150 (small tier)

3. **Reporting**
   - Automatický ranní report s P/L breakdown
   - Reason tagging (score_edge vs odds_anomaly) → 100% audit trail

## Poznámka

Tento soubor je plán. Real-time stav je v `AKTUALNI_PROGRESS.md`.
