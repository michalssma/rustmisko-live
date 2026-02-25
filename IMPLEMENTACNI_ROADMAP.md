# IMPLEMENTAČNÍ ROADMAP

**Aktualizováno:** 2026-02-25  
**Verze:** v4.4.0 (f932f2b)  
**Status:** PHASE 1  Škálování a více zdrojů

---

##  PHASE 0: KOMPLETNÍ (Done)

> Win11 zařízení jako 24/7 live sázecí node

| Milník | Status | Detail |
|--------|--------|--------|
| 0.1 Browser runtime |  DONE | Chrome + Tampermonkey, 3 persistent tabs |
| 0.2 Zdroje onboard |  DONE | FlashScore v3 (7 sportů) + Tipsport v2.1 |
| 0.3 Feed fusion PoC |  DONE | feed_hub: WSSQLiteHTTP /state |
| 0.4 Live execution |  DONE | executor na Polygon, LIVE USDT sázky |
| 0.5 Claim pipeline |  DONE | auto-claim každých 60s, betId fix v4.4.0 |
| 0.6 Sport matching |  DONE | esportscs2 fallback fix v4.4.0 |

**Výsledky Phase 0:**
- Balance: $27.80  $38.74 USDT (první reálné výhry)
- 5 CS2 sázek: 2× Won, 2× Lost, 1× Canceled
- Peněženka: `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`

---

##  PHASE 1: AKTUÁLNÍ  Škálování zdrojů (PRIORITY)

> Přidat více datových zdrojů  více opportunities  vyšší obrat

### 1.1 Fortuna.cz scraper [URGENT  HIGH ROI]
**Proč:** Fortuna má jiné kurzy než Tipsport  ARB příležitosti
- [ ] Tampermonkey userscript pro `fortuna.cz/live`
- [ ] Parsovat LiveOdds JSON (stejná struktura jako Tipsport scraper)
- [ ] Přidat bookmaker="fortuna" do feed_hub
- [ ] Odhadovaný čas: 2-4h
- [ ] Potenciál: +3-5 ARB alertů/den

### 1.2 Football/Hockey score model [MEDIUM]
**Proč:** Football má obrovský Azuro objem, score momentum funguje globálně
- [ ] V alert_bot: rozšířit `find_score_edges()` pro football
  - Gól v 80'+: silný momentum signal
  - Domácí tým vede 1:0, čas >70min  lay second goal opp
- [ ] Hockey: trzí gól, powerplay timing
- [ ] Basketball: quarter-by-quarter momentum
- [ ] Odhadovaný čas: 3-5h

### 1.3 SofaScore jako backup live zdroj [LOW effort]
**Proč:** Záloha pro FlashScore výpadky
- [ ] Tampermonkey pro `sofascore.com/live`
- [ ] Stejný WS protokol jako FlashScore scraper
- [ ] Odhadovaný čas: 1-2h

---

##  PHASE 2: POKROČILÉ EDGES (viz EDGE_NAPADY.md)

> Implementovat postupně  od nejnižšího effort/risk k nejvyššímu

### 2.1 Betfair Exchange scraper [HIGH ROI]
- [ ] Účet na Betfair (nutná registrace/verifikace)
- [ ] Userscript pro `betfair.com/exchange/live`
- [ ] Srovnání Betfair lay-odds vs Azuro back-odds  garantovaný profit
- [ ] Odhadovaný čas: 5-8h + 1-2 dny verifikace účtu

### 2.2 Kelly criterion stake sizing [MEDIUM]
- [ ] V alert_bot: místo fixed $2, vypočítat Kelly fraction
- [ ] `stake = (edge * bankroll) / odds`  agresivní Kelly
- [ ] Half-Kelly pro konzervativní přístup
- [ ] Odhadovaný čas: 2h

### 2.3 Polymarket vs Azuro cross-market [HIGH EFFORT]
- [ ] Polymarket API: `clob-endpoint.polymarket.com`
- [ ] Mapping politických eventů  odpovídající Azuro conditions
- [ ] Odhadovaný čas: 8-12h

### 2.4 Twitter/X sentiment alerts [LOW effort  monitoring]
- [ ] Python script: Twitter API v2 stream filter (cs2, esports, hltv)
- [ ] Klíčová slova: "roster change", "bootcamp", "major qualifier"
- [ ] Telegram alert pouze (ne auto-bet)
- [ ] Odhadovaný čas: 2-3h

### 2.5 ESPN/SofaScore statistiky [LOW effort]
- [ ] Fetch historických head-to-head dat
- [ ] Přidat do confidence scoring v alert_bot
- [ ] Odhadovaný čas: 3h

---

##  PHASE 3: INFRASTRUKTURA (Dlouhodobé)

### 3.1 UK VPS (viz UK_VPS_SETUP.md)
- [ ] DigitalOcean Londýn VPS (Ubuntu 22.04)
- [ ] Feed relay: VPS  Win11 (nízká latence)
- [ ] Lepší VPN bypass pro bookmaker geoblocking

### 3.2 Azuro WebSocket (sub-second odds)
- [ ] `wss://streams.onchainfeed.org` místo 30s GraphQL polling
- [ ] alert_bot poller každou 1s místo 10s
- [ ] Latency výhoda: zpracování odds reakcí dříve než market se adjustuje

### 3.3 Multi-chain optimalizace
- [ ] Porovnat Polygon vs Base vs Gnosis na fees
- [ ] Smart routing: low-fee chain pro malé sázky

### 3.4 Dashboard web UI
- [ ] Simple React SPA zobrazující live /state
- [ ] Graf balance over time
- [ ] Active bets tabulka

---

##  KPI a Cíle

| Metrika | Aktuálně | Phase 1 cíl | Phase 2 cíl |
|---------|----------|-------------|-------------|
| Balance | $38.74 | $60 | $100+ |
| Fused pairs | ~19-50 | ~80-150 | ~200+ |
| Sázky/den | ~2-5 | ~5-10 | ~10-20 |
| ROI/měsíc | ~+15% | ~+20% | ~+30% |
| Zdroje | 2 (FS+TS) | 4 (+ Fortuna + Sofascore) | 6+ |

---

##  Reference dokumenty

- [EDGE_NAPADY.md](EDGE_NAPADY.md)  7 strategií s matematikou a prioritami
- [AKTUALNI_PROGRESS.md](AKTUALNI_PROGRESS.md)  live stav + architektura
- [NAVRH.md](NAVRH.md)  původní návrh systému
- [DECISIONS.md](DECISIONS.md)  technické rozhodnutí a jejich důvody
- [STRATEGIES.md](STRATEGIES.md)  sázecí strategie

---

##  Jak spustit produkci (rychlý cheatsheet)

```powershell
# Zkontrolovat stav
Invoke-RestMethod http://127.0.0.1:8081/state | ConvertTo-Json -Depth 3

# Spustit feed-hub (pokud nejede)
Start-Process -FilePath ".\target\debug\feed-hub.exe" `
  -ArgumentList "" -WindowStyle Normal `
  -Environment @{RUST_LOG="info"; FEED_DB_PATH="data/feed.db"}

# Spustit alert_bot (pokud nejede)
Start-Process -FilePath ".\target\debug\alert_bot.exe" `
  -WindowStyle Normal `
  -Environment @{TELEGRAM_BOT_TOKEN="7611316975:AAG_..."; ...}

# Spustit executor
cd executor; node index.js

# Build po změnách
cargo build --bin feed-hub --bin alert_bot
```

**Chrome tabs nutné otevřít manuálně:**
- `flashscore.com/esports/cs-go/` (Tampermonkey zapnuto)
- `flashscore.com/tennis/` + `/football/` + `/basketball/`
- `tipsport.cz/live` (Tampermonkey zapnuto)
