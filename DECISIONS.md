# RustMiskoLive — Decision Log

Nový agent: přečti CONTEXT.md → pak tento soubor → pak kóduj.

---

## 2026-02-22 — Architektura: samostatný repo

**Rozhodnutí:** Nový standalone repo RustMiskoLive.
**Proč:** Oddělené CONTEXT.md + DECISIONS.md zabraňují zmatení agentů.

---

## 2026-02-22 — 48h observe first

**Rozhodnutí:** První 2 dny = observe only.
**Status:** ✅ SPLNĚNO — observe fáze proběhla, systém přešel na LIVE.

---

## 2026-02-23 — Azuro jako primární platforma

**Rozhodnutí:** Azuro Protocol nahrazuje SX Bet, Polymarket, Betfair.
**Proč:** Jediná crypto platforma s masivním CS2 pokrytím, NO KYC, wallet-only.
**Status:** ✅ INTEGROVÁNO A LIVE.

---

## 2026-02-24 — HLTV scraper v3 (auto-refresh)

**Rozhodnutí:** Přechod z v2 na v3 s auto-refresh mechanismem.
**Proč:** HLTV DOM šel stale po skončení zápasů — scraper posílal mrtvá data.
**Implementace:** 3 min auto-refresh + 90s stale detection + finished match detection (score ≥13).

---

## 2026-02-24 — Duplikátní alerty (arb_cross_book disabled)

**Rozhodnutí:** Vypnutí `arb_cross_book` alertů v alert_bot.
**Proč:** Stejný zápas generoval 2 alerty (arb_cross_book + odds_anomaly). odds_anomaly dává lepší kontext.
**Implementace:** Celý blok `arb_cross_book` alertů zakomentován v alert_bot.rs.

---

## 2026-02-24 — YES parser rozšíření

**Rozhodnutí:** Přidat více formátů YES reply.
**Implementace:** `3 YES $5`, `3 YES`, `YES $5`, `YES` (id=0 → latest alert).

---

## 2026-02-24 — Executor dry-run mode

**Rozhodnutí:** PRIVATE_KEY je optional. Bez něj executor běží v DRY-RUN.
**Proč:** Testování celého pipeline bez risikování peněz.
**Implementace:** DRY_RUN flag, fake betId `dry-{timestamp}`, simulované responses.

---

## 2026-02-24 — Přechod na LIVE

**Rozhodnutí:** Executor přepnut na LIVE režim s reálným private key.
**Wallet:** `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
**Balance:** 33.77 USDT na Polygon
**USDT Approval:** Unlimited pro Azuro Relayer (tx: `0x48cec4ba...`)
**RPC:** `https://1rpc.io/matic` (polygon-rpc.com disabled, llamarpc failed)

---

## 2026-02-24 — RPC endpoint selection

**Rozhodnutí:** Použít `https://1rpc.io/matic` jako primární Polygon RPC.
**Proč:**
- `polygon-rpc.com` — 401 "API key disabled"
- `polygon.llamarpc.com` — fetch failed
- `rpc.ankr.com/polygon` — 401 unauthorized
- `1rpc.io/matic` — ✅ funguje spolehlivě

---

## 2026-02-24 — Finanční model (non-custodial)

**Rozhodnutí:** Azuro je non-custodial — peníze zůstávají ve wallet do momentu betu.
**Rozdíl od Polymarket:** Polymarket má proxy wallet kde musíš depositovat předem.
**Azuro flow:** Wallet → approve Relayer → bet (EIP712 sign) → on-chain → win/lose → zpět do wallet.

---

## 2026-02-25 — NFT Data-Driven Stake Model

**Rozhodnutí:** Sport-specific stake caps založené na reálném ROI z 67 NFTs.
**Data:** esports +33.5%, cs2 +19.3%, football +13.3%, basketball -9.5%, tennis -35.6%.
**Implementace:** Tennis + basketball: max $1/bet (data-collection). CS2/esports/football: $3/bet.

---

## 2026-02-25 — Odds bucket caps

**Rozhodnutí:** Odds 1.5–3.0 = sweet spot. <1.5 a >=3.0 mají negativní ROI.
**Implementace:** `AUTO_BET_MIN_ODDS=1.15`, `AUTO_BET_MAX_ODDS=2.00`, CS2 map_winner exception `3.00`.

---

## 2026-02-26 — Fortuna scraper v3.0

**Rozhodnutí:** Kompletní rewrite Fortuna scraperu — auto-refresh, stale detection, DOM cap.
**Proč:** v2 padal po ~10 min, missing sports, no error recovery.

---

## 2026-02-27 — Opus peer review (12 items)

**Rozhodnutí:** Implementovat všech 12 feedback items z Opus code review.
**Klíčové:** Triple exposure fix, basketball guard, esports map-level guard, exposure caps.

---

## 2026-02-27 — 3-way AI debate (Opus/Gemini/GPT)

**Rozhodnutí:** Strategický audit od 3 AI modelů → 5 upgrade priorit.
**Výsledek:** Score-edge detection, cross-book comparison, exposure management → all implemented.

---

## 2026-02-28 — CRITICAL: Identické Azuro odds guard

**Rozhodnutí:** HARD BLOCK na bety kde Azuro odds team1 ≈ team2 (diff < 0.02).
**Proč:** 12 basketball betů za falešných 1.84 (Azuro oracle nerozlišuje týmy).
**Implementace:** `penalty += 6` (→ LOW confidence → skip) + anomaly path `!azuro_odds_identical`.

---

## 2026-02-28 — Anomaly path MIN_ODDS fix

**Rozhodnutí:** Přidat `azuro_odds >= AUTO_BET_MIN_ODDS` do anomaly auto-bet podmínek.
**Proč:** Team Aether prošel za 1.07 (pod 1.15 min) — score-edge path měl guard, anomaly ne.

---

## 2026-02-28 — Fortuna draw filter (v3.2)

**Rozhodnutí:** Post-process rawOdds: filtrovat remíza/draw/X/tie + smart team matching.
**Proč:** ~60% Fortuna odds byly SAME (draw odds posílané jako team odds).
**Výsledek:** Kvalita 92.5% (z ~40%).

---

## Pravidla

1. **Jazyk:** VŽDY česky.
2. **GIT:** Při riziku >20% nejdřív git save.
3. **Efektivita:** Nejjednodušší cesta k cíli.
4. **Pravdivost:** MD soubory reflektují realitu, ne přání.
