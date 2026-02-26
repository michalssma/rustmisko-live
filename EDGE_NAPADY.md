# EDGE NÁPADY — Prioritní expanze

Aktualizováno: **2026-02-26**

> Tento dokument je strategický backlog s vysokou prioritou.  
> Neobsahuje garantované P/L sliby; slouží jako exekuční mapa „co má největší edge efekt“.

---

## Priority mapa

| #   | Edge                                       | Očekávaný dopad                           | Náročnost                    | Priorita |
| --- | ------------------------------------------ | ----------------------------------------- | ---------------------------- | -------- |
| 1   | **Valorant + LoL + Dota2 map_winner**      | Same CS2 logic, 3x coverage               | Nízká (scraper + match keys) | **P0**   |
| 2   | **1xbit Tampermonkey scraper**             | Nový datový zdroj ALL sports              | Střední                      | **P1**   |
| 3   | **Fortuna/Tipsport cross-book divergence** | Více kvalitních signálů denně             | Nízká–střední                | **P1**   |
| 4   | **Tennis set_winner edge model**           | Přesný set model (game leads)             | Střední                      | **P1**   |
| 5   | **Source trust scoring**                   | Méně fake signálů                         | Nízká                        | **P1**   |
| 6   | **Bet reason tagging (ground truth)**      | Lepší ranní audit a tuning                | Nízká                        | **P2**   |
| 7   | **Betfair / exchange feed**                | Potenciálně velmi silný pricing benchmark | Vysoká                       | **P3**   |

---

## EDGE #1 — Fortuna + další bookmaker (nejrychlejší multiplikátor)

### Proč je to důležité

- Jeden feed = omezený počet divergence.
- Dva a více feedů = výrazně víc validních porovnání proti Azuro.

### Co implementovat

- `userscripts/fortuna_odds_scraper.user.js` ve stejném WS formátu jako Tipsport.
- V `feed_hub` přidat source-level metriku kvality (garbage ratio, stale ratio).
- V `alert_bot` mít možnost filtrovat nebo penalizovat konkrétní zdroj.

### Hot path (minimum viable)

1. odds scraper Fortuna (match_winner market)
2. stejné normalizační čištění názvů jako Tipsport
3. quality gate: pokud source dělá garbage, nevstupuje do auto-betu

---

## EDGE #2 — Score-edge modely pro football/hockey/basketball

### Proč je to důležité

- Dnes je nejsilnější coverage v CS2; mimo něj se nechává edge ležet.

### Co implementovat

- `src/bin/alert_bot.rs`:
  - football: konzervativní win-prob model podle score diff
  - hockey: nižší jistota, přísnější threshold
  - basketball: score diff + total points proxy (fáze zápasu)

### Bezpečnost

- Zachovat hard score sanity limity podle sportu.
- Auto-bet jen HIGH confidence + guardy jako dnes.

---

## EDGE #3 — Source trust scoring (must-have proti garbage)

### Cíl

- Každému zdroji dát dynamické trust skóre a tím řídit, zda jde do auto-betu.

### Návrh metrik

- `garbage_name_rate`
- `score_sanity_reject_rate`
- `stale_rate`
- `match_success_rate` (kolik signálů skončí validním settlement flow)

### Praktická pravidla

- Pokud trust pod limitem → jen alert (bez auto-betu).
- Tipsport může být „trusted default“, ostatní „probation mode“.

---

## EDGE #4 — Ground-truth reason tagging do historie sázek

### Proč

- Ranní report teď reason částečně odhaduje heuristikou.
- Potřebujeme 100% audit: proč byla sázka otevřena.

### Co změnit

- Při zápisu do `data/bet_history.txt` přidat explicitní field:
  - `reason=score_edge` nebo `reason=odds_anomaly_high`
  - ideálně i snapshot edge/confidence v momentu vstupu

---

## EDGE #5 — Exchange benchmark (Betfair-like)

### Realita

- Potenciálně velmi silný zdroj „fair odds“ benchmarku.
- Vyšší implementační i provozní složitost (DOM/přístup/region).

### Kdy to řešit

- Až po dokončení P1 a P2.

---

## Přijímací kritéria pro každý nový edge

1. **Data kvalita**: bez garbage jmen a score artefaktů.
2. **Stabilita**: parser drží dlouhý běh bez degradace.
3. **Bez regrese**: Tipsport v2.3 nesmí utrpět.
4. **Risk guardy**: auto-bet jen při stejných nebo přísnějších podmínkách.
5. **Rollback**: možnost edge okamžitě vypnout feature flagem.

---

## Execution pořadí (doporučené)

1. Fortuna scraper + quality gate
2. Ground-truth reason tagging
3. Football/Hockey/Basket score modely
4. Source trust scoring
5. Exchange benchmark

Tohle je „profit-first“ pořadí s nejlepším poměrem dopad / risk / čas.
