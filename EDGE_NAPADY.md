# EDGE NÁPADY — Prioritní expanze

Aktualizováno: **2026-03-01**

> Tento dokument je strategický backlog s vysokou prioritou.
> Neobsahuje garantované P/L sliby; slouží jako exekuční mapa „co má největší edge efekt".

---

## Priority mapa

| #   | Edge                                       | Očekávaný dopad                           | Náročnost                    | Priorita | Stav |
| --- | ------------------------------------------ | ----------------------------------------- | ---------------------------- | -------- | ---- |
| 1   | **Valorant + LoL + Dota2 map_winner**      | Same CS2 logic, 3x coverage               | Nízká (scraper + match keys) | **P0**   | 🔲 |
| 2   | **1xbit Tampermonkey scraper**             | Nový datový zdroj ALL sports              | Střední                      | **P1**   | 🔲 |
| 3   | **Fortuna/Tipsport cross-book divergence** | Více kvalitních signálů denně             | Nízká–střední                | **P1**   | ✅ DONE |
| 4   | **Tennis set_winner edge model**           | Přesný set model (game leads)             | Střední                      | **P1**   | 🔲 |
| 5   | **Source trust scoring**                   | Méně fake signálů                         | Nízká                        | **P1**   | ✅ PARTIAL |
| 6   | **Bet reason tagging (ground truth)**      | Lepší ranní audit a tuning                | Nízká                        | **P2**   | 🔲 |
| 7   | **Betfair / exchange feed**                | Potenciálně velmi silný pricing benchmark | Vysoká                       | **P3**   | 🔲 |

---

## ✅ HOTOVÉ EDGY

### EDGE #3 — Fortuna cross-book (DONE)
- Fortuna scraper v3.2: draw filter, adaptive polling, smart team matching
- Kvalita: 92.5% (z ~40%)
- Cross-book overlap s Azuro funguje → Path B odds anomaly auto-bet aktivní

### EDGE #5 — Source trust scoring (PARTIAL)
- Identické Azuro odds guard (penalty += 6)
- Cross-validation HLTV vs Chance (mismatch → hard skip)
- WS State Gate → condition Active check
- **Chybí:** dynamické trust skóre per source (garbage_name_rate, stale_rate)

---

## 🔲 OTEVŘENÉ EDGY

### EDGE #1 — Valorant + LoL + Dota2 map_winner (P0)
- Scraper potřebuje: Tipsport/Chance/HLTV mají tyto sporty v nabídce
- alert_bot `get_sport_config()` už podporuje `valorant`, `dota-2`, `league-of-legends`
- Chybí: scraper pro specifické turnaje + map score parsing

### EDGE #2 — 1xbit scraper (P1)
- Nový datový zdroj ALL sports → zvýší cross-validation coverage
- Tampermonkey scraper ve WS formátu jako Tipsport

### EDGE #4 — Tennis set_winner edge model (P1)
- Přesný set-level model (game leads, podání)
- Tennis min edge snížen na 12% (z 15%), ale ROI stále záporný

### EDGE #6 — Reason tagging (P2)
- Přidat `reason=score_edge` / `reason=odds_anomaly` do ledger
- Snapshot edge/confidence v momentu vstupu pro 100% audit trail

### EDGE #7 — Exchange benchmark (P3)
- Betfair-like fair odds benchmark
- Vyšší implementační složitost, řešit až po P0/P1

---

## Přijímací kritéria pro každý nový edge

1. **Data kvalita**: bez garbage jmen a score artefaktů.
2. **Stabilita**: parser drží dlouhý běh bez degradace.
3. **Bez regrese**: stávající scrapers nesmí utrpět.
4. **Risk guardy**: auto-bet jen při stejných nebo přísnějších podmínkách.
5. **Rollback**: možnost edge okamžitě vypnout feature flagem.

---

## Execution pořadí (doporučené)

1. ~~Fortuna scraper + quality gate~~ ✅ DONE
2. Ground-truth reason tagging
3. Valorant/LoL/Dota2 map_winner
4. 1xbit scraper
5. Tennis set_winner model
6. Exchange benchmark

Tohle je „profit-first" pořadí s nejlepším poměrem dopad / risk / čas.
