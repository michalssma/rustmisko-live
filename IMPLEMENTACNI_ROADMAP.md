# IMPLEMENTAČNÍ ROADMAP

Aktualizováno: **2026-02-26**

Tento dokument je roadmapa (plán), ne live stav. Aktuální provozní čísla jsou v `AKTUALNI_PROGRESS.md`.

## Phase A — Stabilita (aktuální priorita)

- Dotáhnout dedup ochrany v auto-betu (match/condition/base-key)
- Udržet spolehlivý claim flow (`/check-payout` + `/auto-claim` safety-net)
- Průběžně čistit noisy source data mimo Tipsport

## Phase B — Kvalita dat

- Přidat source-level trust scoring (tipsport > ostatní)
- Izolovat nebo penalizovat zdroje s garbage team names
- Rozšířit sport-specific score sanity limity podle reálného provozu

## Phase C — Exekuční kvalita

- Přesnější reason-tagging u každé sázky (`score_edge` vs `odds_anomaly`)
- Ranní report: P/L, win/loss, claim summary, otevřené pending pozice
- Lepší audit trail pro morning review

## Phase D — Rozšíření zdrojů

- Fortuna scraper (pokud zachová kvalitu dat)
- Další booky pouze pokud projdou quality gate

## Exit kritéria pro „klidný noční režim"

- `executor /health` = `ok`
- běží `feed-hub`, `alert-bot`, `executor`
- bez kritických chyb v alert-bot logu
- pending claimy sledované a bez ztráty token mappingu