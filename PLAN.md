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

1. **Runbook hardening (fix → test → gate)**
   - Každý fix má povinné pořadí: **implementace → replay test → KPI kontrola → teprve potom rollout**.
   - Bez splněných gate metrik se nepokračuje do další fáze.

2. **Fix #1 — Market alignment v odds anomaly**
   - Upravit anomaly detekci tak, aby porovnání probíhalo jen mezi stejnými markety (`Azuro market == market_key/payload.market`).
   - Zabránit mixu `map*_winner` vs `match_winner` (hard skip při nekompatibilním marketu).
   - Cíl: odstranit phantom anomálie a snížit falešné alerty.

3. **Povinný replay test po Fix #1**
   - Spustit deterministický replay na 5 historických JSONL souborech.
   - Příkaz: `cargo test -- --test-threads=1`
   - Test musí běžet s clock injection (fixovaný čas/replay timeline), aby výsledky byly reprodukovatelné.
   - Ověřit shodu: počty alertů, A/B/C klasifikace, reject důvody.

4. **KPI Gate #1 (po alignmentu)**
   - `anomaly_precision > 90%` (true positives / total alerts), měřit před/po z JSONL grepem.
   - `only-1-source SKIP < 25%`
   - `placed_rate >= 35%` z validních HIGH signálů
   - Pokud gate neprojde: rollback změny a iterace pouze na matching/market mapování.

5. **Fix #2 — Observability metriky ve feed-hub**
   - Rozšířit `/state` o:
     - `freshness_by_bookmaker` (p50/p95/max age)
     - `source_count_per_market` (`match_key + market`)
     - `fused_ready_per_market`
   - Přidat periodický JSONL heartbeat event se stejnými metrikami pro audit.

6. **Povinný replay test po Fix #2**
   - Opět `cargo test -- --test-threads=1` na stejných 5 JSONL.
   - Ověřit determinismus + konzistenci metrik (žádné časové drift artefakty).

7. **KPI Gate #2 (po observability)**
   - Každý reject typu `only source`/`stale` musí mít jednoznačné metrické vysvětlení.
   - `ConditionNotRunning < 10%` z attemptů
   - `FOLLOW-UP REJECTED (execution reverted) < 5%` z `PLACED`

8. **Scraper-by-scraper validační průchod**
   - Pořadí: `Fortuna → Tipsport → Chance` (izolovaně).
   - Pro každý scraper: ingest, freshness, market alignment, fusion, anomaly kvalita.
   - Minimum: 20 validních live vzorků na scraper bez systematického mismatch.

9. **Staged rollout autobetu**
   - Fáze A: dry-run (signal-only).
   - Fáze B: micro-stake ($1, denní cap).
   - Fáze C: standard stake ($3) jen pro whitelist markety/sporty.
   - Přechod mezi fázemi jen při 24h stabilitě KPI.

10. **Hard-stop + rollback pravidla**
    - Okamžitý stop při KPI breach (`reverted`, `ConditionNotRunning`, denní loss limit).
    - Rollback musí být konfigurační (bez dlouhého redeploy).

11. **Až potom rozšiřování coverage**
    - 1xbit scraper a další booky až po splnění všech gate metrik výše.

12. **Reporting (finální vrstva)**
    - Automatický ranní report s P/L breakdown.
    - Dotažení reason taggingu (`score_edge` vs `odds_anomaly`) na 100% audit trail.

## Poznámka

Tento soubor je plán. Real-time stav je v `AKTUALNI_PROGRESS.md`.
