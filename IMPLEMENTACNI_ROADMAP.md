# IMPLEMENTAČNÍ ROADMAP

Aktualizováno: **2026-03-01**

Tento dokument je roadmapa (plán), ne live stav. Aktuální provozní čísla jsou v `AKTUALNI_PROGRESS.md`.

## Phase A — Stabilita ✅ HOTOVO

- ✅ Dedup ochrany v auto-betu (match/condition/base-key)
- ✅ Spolehlivý claim flow (`/check-payout` + `/auto-claim` safety-net + Azuro relayer)
- ✅ Očištění noisy source dat (Fortuna draw filter, identické odds guard)
- ✅ 5-phase matching fix (NFKD, country translate, sport alias, token-subset)
- ✅ Zombie inflight TTL fix + stale pending_claims cleanup

## Phase B — Kvalita dat ✅ HOTOVO

- ✅ Source-level trust scoring (Fortuna kvalita 92.5%)
- ✅ Identické Azuro odds guard (penalty += 6 pro identical odds)
- ✅ Sport-specific score sanity limity
- ✅ Cross-validation HLTV vs Chance (mismatch → hard skip)
- ✅ WS State Gate (pre-flight condition Active check)

## Phase C — Exekuční kvalita ✅ VĚTŠINOU HOTOVO

- ✅ 6 safety layers implementovány
- ✅ Exposure caps (per-bet, per-condition, per-match, daily, per-sport, inflight)
- ✅ Loss streak pause (3 LOST → 300s)
- ✅ Min bankroll guard ($20)
- ✅ Created→follow-up polling na všech 3 bet paths
- ✅ Won→alreadyPaid fix (portfolio display)
- ✅ Startup message s oběma paths
- 🔲 Přesnější reason-tagging u každé sázky (`score_edge` vs `odds_anomaly`)
- 🔲 Automatický ranní report (P/L, win/loss, claim summary)

## Phase D — Rozšíření zdrojů 🔲 NEXT

- ✅ Fortuna scraper v3.2 (draw filter, smart matching, adaptive polling)
- 🔲 1xbit scraper (pokud data kvalita projde quality gate)
- 🔲 Další booky pouze pokud projdou quality gate

## Phase E — Škálování 🔲 BUDOUCNOST

- 🔲 Config file pro feature flags (teď hardcoded bool)
- 🔲 Bankroll growth strategy ($46 → $150+ small tier)
- 🔲 Multi-chain (Azuro na Gnosis/Chiliz)
- 🔲 Overtime/pre-match markets

## Exit kritéria pro „klidný noční režim"

- `executor /health` = `ok`
- běží `feed-hub`, `alert-bot`, `executor`
- bez kritických chyb v alert-bot logu
- pending claimy sledované (Created→follow-up polling)
- WS State Gate aktivní → pre-flight condition check
