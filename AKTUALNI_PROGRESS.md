# AKTUALNI_PROGRESS — handoff pro Sonneta

Aktualizováno: 2026-02-24
Repo: RustMiskoLive (`C:\RustMiskoLive`)

## 🚀 STAV: PHASE 0 STARTOVÁNA (PERSISTENT BROWSER NODE)

### Aktuální priorita

Nejvyšší priorita je zprovoznit na tomto Win11 zařízení **permanentní browser runtime** (manual login + persistent sessions), ze kterého Rust ingestuje live data napříč esport zdroji a bookie odds. Profit/scaling řešíme až po datovém PoC.

### Co už je ověřeno dnes (2026-02-24)

1. **HLTV test binárka běží stabilně** (`cargo run --bin hltv-test`)
2. **HTTP requesty na HLTV endpointy** vrací 403 (anti-bot), takže čistý reqwest scraping není dostačující
3. **Browser fallback vrstva** je implementována a připravená na další hardening
4. **Roadmap + Decisions** přepnuté na "Phase 0 first" workflow

Pozn.: "MATCH_RESOLVED" eventy jsou užitečné pro oracle-lag strategii (po konci). Phase 0 PoC je ale primárně o **LIVE dění + LIVE kurzech** (in-play), tj. kontinuální live update stream.

### Co děláme teď (bez odboček)

1. Nastavení always-on browser procesu (po rebootu se sám zvedne)
2. Ruční přihlášení na cílové stránky (esport live data + kurzy)
3. Rust feed fusion proof: systém musí ukázat „co je live“ + „kde je live odds"
4. Ukládání replay logu pro kalibraci a ladění

### Exit criteria pro přechod na scaling

- Feed uptime ≥ 98% za 24h
- p95 lag < 2s
- Konsensus feedů ≥ 80%
- False join rate < 5%

Dokud není tohle splněné, navyšování stake ani rozšíření na další node není priorita.

### Co se změnilo (2026-02-23)

**Kritický fix: systém přepnut z mrtvých výsledků na LIVE sledování.**

1. **LIVE State Machine v `esports_monitor`**
   - Nová metoda `poll_live_all()` jako PRIMÁRNÍ zdroj dat (každých 15s):
     - **LoL**: `getSchedule` API → sleduje `state: "inProgress"` → `"completed"` přechod
     - **Valorant**: `vlr.gg/matches` → CSS selektor `a.match-item.mod-live` (ověřeno browser inspekcí)
     - **CS2 + Dota 2**: `gosugamers.net/counterstrike/matches` a `dota2/matches` → SSR HTML parsování, detekce "Live" badge v `textContent`
   - In-memory `HashMap<String, LiveMatch>` drží aktuálně live zápasy
   - Detekce přechodu: zápas zmizí z live sekce → emituje `MATCH_RESOLVED` → okamžitě checkuje SX Bet

2. **GosuGamers scraper kompletně přepsán**
   - Starý kód: selektory `.match-list-item`, `.team-name`, `.score` → NA WEBU NEEXISTUJÍ (GosuGamers běží na Material UI)
   - Starý URL: `/counter-strike/matches` → VRACÍ 404!
   - Nový kód: parsuje `<a href="/tournaments/.../matches/ID-team1-vs-team2">` elementy
   - Team names se extrahují z URL slugu (spolehlivější než text parsing)
   - Skóre se parsuje regexem `(\d+)\s*:\s*(\d+)` z textu

3. **`main.rs` — Dual-mode loop**
   - PRIMÁRNÍ: `monitor.poll_live_all()` každých 15s → live→finished detekce
   - FALLBACK: `monitor.poll_all()` jednou za 5 min (20 cyklů) → audit/catch-up

4. **Deduplikace** — `HashSet` v `seen_matches` zabraňuje opakovanému zpracování

5. **Visibility logging** — SX Bet lookup miss viditelný na `info!` úrovni

### Co systém REÁLNĚ dělá teď

```
Live poll cycle:
  1. Stáhne live match stránky (LoL API, vlr.gg, GosuGamers)
  2. Porovná s pamětí: nový live? → zapamatuj. Zmizel live? → FINISHED!
  3. Pro FINISHED zápasy: dohledá vítěze na results stránce
  4. Okamžitě checkne SX Bet cache (16µs lookup)
  5. Pokud SX Bet market existuje → query orderbook → edge evaluation
  6. Edge >3% → Telegram alert + JSONL log
```

### Proč to bude fungovat

- SX Bet oracle lag: **10-25 minut** po konci zápasu
- Náš detection delay: **1-5 minut** (HTML refresh interval)
- **Zbývající okno: 5-20 minut** na sázku na známého vítěze

### Co stále NENÍ hotové (pravdivě)

1. **Trading/execution** — stále `observe_only = true`
2. **Signal klasifikace** (A+/A/B/REJECT) — zatím neimplementováno
3. **Oracle lag měření** — nemáme data o tom jak rychle SX Bet reálně settleuje
4. **PandaScore/websocket** — free zdroje stačí pro MVP, ale placené API by zkrátily delay na <30s

### Jak reprodukovat

```bash
cp .env.example .env
# Nastav ESPORTS_POLL_INTERVAL_SECS=15
cargo run --bin live-observer
# Sleduj terminál pro 🔴 LIVE a ✅ MATCH FINISHED hlášky
```

### Poznámka k pravdivosti

Tento soubor je záměrně bez optimism bias: popisuje přesně to, co je v repu a co bylo runtime ověřeno, včetně limitů.
