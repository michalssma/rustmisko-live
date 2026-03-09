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

## OSTATNÍ SPORTY LIVE

Rozšíření má jít jen přes sporty, které už dnes skutečně vidíme v live feedu a/nebo odds vrstvě, a jen pokud mají score-state, který umíme přetavit do informační výhody podobně jako dnes `cs2`.

### Priorita rollout pořadí

1. **Dota-2**
   - V live feedu i odds vrstvě už je přítomná.
   - Model už má kill-score logiku a patří do stejné family jako `cs2` / `valorant`.
   - Cíl: otevřít jen `match_or_map` path s přísným edge gate a bez generic fallbacku.

2. **Valorant / LoL**
   - Live feed coverage existuje, score model je blízký map-based esportům.
   - Cíl: převzít map-score princip z `cs2`, ale až po replay validaci na reálných live samplech.

3. **Basketball**
   - Odds coverage je silná a v historických anomaly datech je jediný zelený ostrůvek.
   - Live score model už v kódu existuje, ale feature flag je vypnutý.
   - Cíl: otevřít nejdřív score-edge micro-stake režim, ne anomaly-first.

4. **Volleyball**
   - Odds feed existuje, live score je set-based a strukturálně čistší než football.
   - Cíl: připravit jednoduchý set-lead model, ale nejdřív jen dry-run + replay.

5. **Ice-hockey**
   - Odds coverage existuje, ale bez odladěného score modelu je riziko podobné footballu.
   - Cíl: držet zatím alert-only / paper-only, dokud nebude minute-state a game-state model.

6. **Baseball**
   - Coverage je zatím malá.
   - Cíl: neotvírat pro auto-bet dřív, než bude dost live sample a jasný inning-state model.

### Co má zůstat omezené

1. **Generic `esports` fallback**
   - Držet přísnější než konkrétní sporty (`cs2`, `dota-2`, `valorant`, `lol`).
   - Nepoužívat jako hlavní growth vector pro throughput.

2. **`odds_anomaly` path**
   - Po recent auditu je jako celek slabší než `edge` path.
   - Pro nové sporty nepouštět anomaly autobet jako první; nejdřív score-edge / state-driven model.

### Gate pro každý nový live sport

1. Musí existovat stabilní live score/state parsing bez garbage spike.
2. Musí existovat replay na historických JSONL vzorcích daného sportu.
3. Nejdřív `dry-run`, potom micro-stake, potom standard stake.
4. Rollout jen pokud daný sport drží kladné nebo aspoň ne-negativní recent ROI v prvním malém vzorku.
5. Pokud sport skončí pod nulou nebo začne generovat `ConditionNotRunning` / garbage-score šum, vrátit ho zpět do alert-only režimu.

## DOTA-2 A VALORANT - DETAILNÍ LIVE ROLLOUT

Tady už nejde o obecné "přidáme další esport". `dota-2` a `valorant` mají jít jako samostatné vertikály se svým feed gate, market gate, modelem, dry-runem a risk budgetem. Hlavní bottleneck dnes není chybějící model v kódu, ale to, že v live feedu často dorazí event s odds, ale bez použitelného `score` / `detailed_score`.

### Tvrdý verdikt

1. **Neotvírat generic `esports` kvůli Dota/Valorant throughputu.**
2. **Neotvírat `odds_anomaly` jako první produkční path.**
3. **Nejdřív vyřešit data readiness a až potom pustit score-edge autobet.**
4. **První rollout dělat jen přes `match_winner` a pouze tam, kde live state potvrzuje informační výhodu.**

### Systémové blokery, které musí zmizet

1. **Live score nullability**
   - U části `dota-2` / `valorant` live eventů dnes vidíme validní odds, ale `score=null` a `detailed_score=null`.
   - Takový event nesmí být "měkce" interpretován. Musí skončit jako `not_actionable_missing_score_state`.

2. **Market identity a market preference**
   - Dota i Valorant musí vždy nést explicitní `market_key` (`match_winner`, případně později `map1_winner`, `map2_winner`).
   - Nesmíme míchat `match_winner` edge s mapovým stavem bez jasného přepočtu.

3. **Feed completeness metriku nemáme dost přísnou**
   - Potřebujeme odděleně sledovat, kolik live eventů pro `dota-2` a `valorant` má:
     - odds only,
     - score only,
     - odds + score,
     - odds + detailed_score.
   - Bez toho nepoznáme, jestli je problém v ingestu, normalizaci, nebo u booka.

### Fáze 0 - Feed readiness audit

1. Přidat sport-specifickou metriku readiness do observability:
   - `live_events_total{sport}`
   - `live_events_with_score{sport}`
   - `live_events_with_detailed_score{sport}`
   - `live_events_with_azuro_match_market{sport}`
   - `live_events_actionable_ratio{sport}`

2. Nastavit minimální gate pro vstup do dry-runu:
   - `dota-2`: aspoň 60 % live eventů s použitelným score/state.
   - `valorant`: aspoň 70 % live eventů s použitelným score/state.
   - Pokud ratio nedrží 24 hodin, sport zůstává pouze `alert-only`.

3. Logovat explicitní reject reasony:
   - `DOTA_SCORE_MISSING`
   - `VALORANT_SCORE_MISSING`
   - `SPORT_MARKET_NOT_PREFERRED`
   - `SPORT_STATE_NOT_ACTIONABLE`

### Fáze 1 - Dota-2 dry-run

`dota-2` má jít první, protože už má v kódu vlastní větev a kill-score/game-state je pro live edge přirozenější než u generic esportů.

1. **Povolené trhy**
   - Start jen s `match_winner`.
   - `map*_winner` nechat zatím jen v observability, dokud neověříme kvalitu map-state parsingu.

2. **Actionable stavy**
   - Musí existovat validní game/map stav, ne jen bookmaker odds.
   - Pokud event neobsahuje použitelný state, skip bez výjimky.

3. **Edge gate**
   - Začít přísněji než u `cs2`:
     - min edge `>= 30 %`
     - odds corridor konzervativně `1.45 - 2.10`
   - Otevírat až po replay validaci na reálných historických samplech.

4. **Dry-run cíle**
   - nasbírat 50+ kandidátů,
   - ověřit rozdělení reject reasonů,
   - ověřit, že dominantní reject není datová díra, ale skutečně edge filtr.

5. **Go/No-Go pro micro-stake**
   - `placed-ready candidates >= 20`
   - `missing_score_reject_ratio < 35 %`
   - replay + live paper P/L nesmí být výrazně negativní.

### Fáze 2 - Dota-2 micro-stake

1. Spustit jen přes feature flag.
2. Fixní malý stake (`$0.50-$1.00`) bez dynamického navyšování.
3. Denní cap oddělený od `cs2` budgetu.
4. Povinné KPI po prvních 20 settlech:
   - ROI >= 0
   - reject po `PLACED` (`ConditionNotRunning`, revert) < 10 %
   - žádný pattern falešných betů bez reálného score edge.

### Fáze 3 - Valorant dry-run

`valorant` musí být druhý, ale ne automaticky kopie `cs2`. Je map-based, ale ve feedu často trpí chybějícím state.

1. **Povolené trhy**
   - start jen `match_winner`
   - map winners až po potvrzení, že `detailed_score` konzistentně odlišuje map state

2. **Actionable stavy**
   - musí být zřejmé, zda jde o map score / match score a nesmí docházet k záměně
   - pokud to feed nerozlišuje, event jde do `alert-only`

3. **Edge gate**
   - min edge `>= 30 %`
   - konzervativní odds corridor `1.50 - 2.05`
   - bez generic `esports` fallbacku

4. **Dry-run cíle**
   - 50+ kandidátů
   - rozdělit kandidáty na:
     - score-ready
     - odds-only
     - market-ambiguous
   - pokud `market-ambiguous` tvoří významnou část sample, rollout stopnout a nejdřív opravit parsing

### Fáze 4 - Valorant micro-stake

1. Spustit až po Dota dry-runu nebo paralelně jen pokud feed kvalita drží.
2. Stake stejně konzervativní jako Dota.
3. Samostatný KPI sheet, nemíchat hned s generic `esports` výsledky.

### Feature flags a rollout přepínače

1. `FF_DOTA2_EDGE_DRY_RUN`
2. `FF_DOTA2_EDGE_LIVE`
3. `FF_VALORANT_EDGE_DRY_RUN`
4. `FF_VALORANT_EDGE_LIVE`
5. `FF_DOTA2_MAP_MARKETS`
6. `FF_VALORANT_MAP_MARKETS`

Každý flag musí mít rollback bez redeploye a jasný ledger/alert reason tag.

### Co přesně doplnit do kódu, než se to pustí

1. **Sport-specific readiness counters** v `feed-hub` nebo v heartbeat vrstvě.
2. **Explicit sport reject reasony** v `alert_bot` ledgeru.
3. **Strict null-score guard** pro `dota-2` a `valorant`.
4. **Oddělené KPI breakouty** podle `sport + market_key + path`.
5. **Replay fixture set** pro oba sporty z reálných live JSONL vzorků.

### Risk management pro oba sporty

1. Nezvyšovat stake jen proto, že jde o nový sport s menší frekvencí.
2. Neškálovat podle generic `esports` P/L.
3. Pokud se ukáže, že live score coverage kolísá podle booka nebo času dne, zavést whitelist jen na stabilní subsegment.
4. Pokud se v prvních 20 settlech objeví vzor "bet bez skutečného state edge", rollout okamžitě vrátit na dry-run.

### Definice úspěchu

1. `dota-2` a `valorant` musí generovat validní candidates z live state, ne z pouhé cenové odchylky.
2. Reject reason dominance se má přesunout z `missing score/state` na normální edge filtry.
3. První live sample musí být auditovatelný po eventech: `candidate -> placed -> accepted -> settled` s jasným `market_key`.
4. Teprve potom má smysl uvažovat o uvolnění corridoru nebo map markets.

## Poznámka

Tento soubor je plán. Real-time stav je v `AKTUALNI_PROGRESS.md`.
