# AKTUALNI_PROGRESS — handoff pro Gemini

Aktualizováno: 2026-02-22
Repo: RustMiskoLive (`C:\RustMiskoLive`)

## Co bylo skutečně dokončeno

1. **PHASE 1 logging-only je nasazená v kódu**
   - `live-observer` běží v observe režimu (bez order execution).
   - Přidány nové JSONL eventy:
     - `API_STATUS` (stav zdroje/sportu)
     - `SYSTEM_HEARTBEAT` (souhrn poll cyklu)
   - Runtime tunables přes `.env`:
     - `POLL_INTERVAL_SECS`
     - `MIN_ROI_PCT`

2. **Code changes (realně aplikováno)**
   - `crates/logger/src/lib.rs`
     - nové event struktury `ApiStatusEvent`, `SystemHeartbeatEvent`
   - `crates/price_monitor/src/lib.rs`
     - poll vrací health summary
     - per-source API status logging
     - heartbeat po každém cyklu
     - `MIN_ROI_PCT` filtr pro odds-api signály
   - `src/main.rs`
     - načítá `POLL_INTERVAL_SECS`, `MIN_ROI_PCT`
   - `.env.example`
     - přidány nové konfig položky
   - `crates/arb_detector/src/lib.rs`
     - cleanup unused variable warning

3. **Dokumentace byla synchronizována**
   - `PLAN.md` — status změněn na PHASE 1 logging-only nasazeno
   - `DECISIONS.md` — rozhodnutí o startu logging-only deploymentu
   - `CONTEXT.md` — aktualizovaný aktuální stav + next steps

4. **Runtime ověření proběhlo**
   - vznikl log soubor `logs/2026-02-22.jsonl`
   - log obsahuje validní nové eventy (`API_STATUS`, `SYSTEM_HEARTBEAT`)

## Co teď nefunguje / není hotové (pravdivě)

1. **Pinnacle endpoint vrací 401 bez auth**
   - aktuálně vidíme `INVALID_AUTHORIZATION_HEADER`
   - bez validního `PINNACLE_KEY` nejsou data z Pinnacle

2. **odds-api polling je skipnutý bez API klíče**
   - pokud není `ODDSAPI_KEY`, zapisuje se `skipped_no_api_key`
   - tudíž paper signaly z odds-api nejsou zatím reálně generované

3. **Trading/execution není implementován**
   - stále čistě logging-only
   - A+/A/B klasifikace signálů zatím není v kódu

4. **Čas od času byl lock na `live-observer.exe` při rebuildu**
   - potřeba hlídat běžící proces před novým `cargo run`

## Co má Gemini opravit / dodělat jako další krok

### Priorita 1 — stabilní datové naplnění
- Přidat robustnější fallback feed (když Pinnacle 401), aby heartbeat nebyl trvale 0 healthy.
- Ověřit správný endpoint/kontrakt pro odds-api a sjednotit parser pro reálné response varianty.

### Priorita 2 — paper signal intelligence
- Přidat klasifikaci `A_PLUS | A | B | REJECT` přímo do logu podle:
  - confidence,
  - liquidity,
  - spread,
  - stale timing,
  - source quorum.
- Přidat denní agregaci kvality signálů (precision proxy, conversion to resolved outcomes).

### Priorita 3 — process safety
- Přidat guard proti současnému běhu více instancí observeru.
- Přidat explicitní `STARTUP_EVENT` a `SHUTDOWN_EVENT` do JSONL.

## Jak reprodukovat současný stav

1. `cp .env.example .env` (nebo ručně vyplnit)
2. minimálně nastavit:
   - `POLL_INTERVAL_SECS=60`
   - `MIN_ROI_PCT=1.0`
   - ideálně `ODDSAPI_KEY=...`
3. spustit:
   - `cargo run --bin live-observer`
4. kontrola:
   - `logs/YYYY-MM-DD.jsonl` obsahuje heartbeat/status eventy

## Poznámka k pravdivosti

Tento soubor je záměrně bez optimism bias: popisuje přesně to, co je v repu a co bylo runtime ověřeno, včetně limitů.
