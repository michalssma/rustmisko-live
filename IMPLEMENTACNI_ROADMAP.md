# IMPLEMENTAČNÍ ROADMAP podle NAVRH.md

**Aktualizováno:** 2026-02-24  
**Status:** PHASE 0 - Persistent Browser PoC (Win11 zařízení jako primární data node)

## 🚀 **PHASE 0: PERSISTENT BROWSER NODE (PRIORITA TEĎ)**

**Cíl:** Na tomto 24/7 Win11 zařízení stabilně držet přihlášené browser sessions (manual login), nepřetržitě sbírat LIVE data z více zdrojů a přes Rust potvrdit, že umíme slučovat live matches + odds napříč trhy v reálném čase.

### **Milník 0.1: Always-on browser runtime** 🔄
- [ ] Vybrat primární browser profil (Edge/Chrome) pro dlouhodobé session cookies
- [ ] Nastavit auto-start browseru po rebootu + auto-open konkrétních tabů
- [ ] Vypnout sleep/hibernaci a agresivní power-saving
- [ ] Potvrdit 24h stabilitu bez ručního zásahu

### **Milník 0.2: Source onboarding (manual login first)** 🔄
- [ ] Přihlásit účty ručně na všech cílových zdrojích (esport data + bookie odds)
- [ ] Zmapovat které stránky dávají LIVE score, které LIVE kurzy
- [ ] Označit minimálně 2 nezávislé zdroje na sport jako "PRODUCTION FEED"
- [ ] U každého zdroje vyplnit failover prioritu (A/B/C)

### **Milník 0.3: Rust feed fusion PoC** 🔄
- [ ] Potvrdit příjem dat z browser feedu do Rust listeneru
- [ ] Spustit `feed-hub` WS ingest (`ws://<ip>:8080/feed`) pro Lenovo/Zebra JSON stream
- [ ] Gating pro odds (likvidita/spread/stale) + event logy (`LIVE_FUSION_READY`)
- [ ] Zavést normalizaci match identity (team aliases + deduplikace)
- [ ] Prokázat, že systém umí zobrazit: "co je live" + "kde je live odds"
- [ ] Uložit replay log pro pozdější tuning edge detekce

### **Milník 0.4: Proof-of-concept exit criteria** ⏳
- [ ] Uptime feedu ≥ 98% za 24h
- [ ] p95 feed lag < 2s
- [ ] Konsensus mezi feedy ≥ 80% na live zápasech
- [ ] False join rate (špatné match mapování) < 5%

**Poznámka:** Profitabilita a škálování (Android, vyšší stake) jsou až po splnění Phase 0 exit criteria.

## 📋 **PŘEHLED STAVU**

### ✅ **Již implementováno:**
1. **HLTV scraper crate** (`crates/hltv_scraper/`)
   - Fetch live matches z HLTV.org
   - Fetch match details s score
   - Prediction logic přímo v `HltvLiveMatch`
   - Rate limiting a user-agent rotace

2. **Prediction engine** (`crates/prediction_engine/`)
   - MatchState struct pro všechny esporty
   - Prediction enum s confidence scores
   - Heuristika pro CS2 a Valorant
   - Series prediction pro Bo3/Bo5

3. **Ultra-live monitor** (`src/ultra_live.rs`)
   - SniperSession management
   - Monitorovací loop s dynamic interval
   - Sniper mode (2s) vs normal mode (10s)
   - Expired sessions cleanup

4. **Dokumentace:**
   - `NAVRH.md` - kompletní strategie
   - `UK_VPS_SETUP.md` - guide pro UK VPS
   - Tento roadmap

### 🔄 **Právě implementujeme:**
1. Permanentní browser node na tomto Win11 zařízení
2. Multi-source live feed fusion v Rustu (nejdřív PoC, potom škálování)

### 🧪 **Aktuální lokální test (2026-02-24):**
- ✅ `cargo run --bin hltv-test` už **kompiluje a běží**
- ✅ Opraveny blokující build chyby:
  - `prediction_engine`: undefined `current_map_number`
  - `hltv_scraper`: `Instant` + `serde` derive konflikt
  - `src/hltv_test.rs`: closure lifetime (`move`)
- ✅ Implementována resilient vrstva `HTTP -> browser fallback` v `hltv_scraper`
- ✅ `fetch_live_matches()` přepnuto na `https://www.hltv.org/live`
- ✅ Přidán endpoint probe mód (`html_len`, `match_id_count`, `challenge_page`)
- ⚠️ Aktuální realita z testu:
  - `/live`: `html_len≈28k`, `match_ids=0`, `challenge_page=true`
  - `/results`: `html_len≈28k`, `match_ids=0`, `challenge_page=true`
  - závěr: browser fallback získává HTML, ale jde stále o challenge stránku, ne sportovní obsah

### ⏳ **Čeká na implementaci:**
1. **Phase 0:** Persistent Browser Node PoC (Win11)
2. **Fáze 1:** HLTV scraping prototype (dokončení)
3. **Fáze 2:** UK VPS setup + Betfair API
4. **Fáze 3:** Full prediction engine integrace
5. **Fáze 4:** Sniper mode execution na SX Bet

---

## 🎯 **FÁZE 1: HLTV Scraping Prototype (po Phase 0 PoC)**

### **Milník 1.1: Funkční HLTV fetcher** ✅
- [x] Vytvořeno `crates/hltv_scraper/`
- [x] Implementováno `fetch_live_matches()`
- [x] Implementováno `fetch_match_details()`
- [x] Rate limiting a user-agent rotace

### **Milník 1.2: Testovací binárka** ✅
- [x] Vytvořeno `src/hltv_test.rs`
- [x] Jednorázový fetch test
- [x] Kontinuální monitoring s callback

### **Milník 1.3: Benchmark vs GosuGamers** 🔄
- [ ] Odblokovat HLTV 403 (lokálně) / fallback source
- [ ] Spustit paralelně HLTV a GosuGamers scraping
- [ ] Měřit latenci:
  - Čas od konce zápasu → detekce
  - Success rate (kolik zápasů zachytíme)
  - HTTP error rate
- [ ] Výsledky zapsat do `benchmark_results.json`

**Testovací příkaz:**
```bash
cargo run --bin hltv-test -- --benchmark
```

### **Milník 1.4: Integrace do main loopu** ⏳
- [ ] Upravit `src/ultra_live.rs` pro použití HLTV jako primárního zdroje pro CS2
- [ ] Zachovat GosuGamers jako fallback
- [ ] Implementovat deduplikaci mezi zdroji

---

## 🎯 **FÁZE 2: UK VPS Setup (7 denní trial)**

### **Milník 2.1: Založení VPS** ⏳
- [ ] Zvolit Contabo vs jiný provider
- [ ] Založit účet s London datacentrem
- [ ] Získat SSH přístup
- [ ] Otestovat UK IP: `curl ifconfig.me`

### **Milník 2.2: Instalace prostředí** ⏳
- [ ] Nainstalovat Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- [ ] Nainstalovat Git, build-essential
- [ ] Nainstalovat PM2 pro process management

### **Milník 2.3: Clone a Build** ⏳
- [ ] Naklonovat repo na VPS
- [ ] `cargo build --release`
- [ ] Otestovat `./target/release/hltv-test`

### **Milník 2.4: Proxy Setup** ⏳
- [ ] Vybrat residential proxy provider (Bright Data/Smartproxy)
- [ ] Nakonfigurovat proxy rotaci v kódu
- [ ] Testovat connectivity k Betfair API

### **Milník 2.5: Betfair API test** ⏳
- [ ] Vytvořit `test_betfair_api.rs`
- [ ] Získat App Key z developer.betfair.com
- [ ] Otestovat login a základní requesty

**Odhad času:** 2-3 dny

---

## 🎯 **FÁZE 3: Prediction Engine Integrace**

### **Milník 3.1: Full prediction logic** 🔄
- [x] Základní engine implementován
- [ ] Rozšířit o:
  - [ ] Momentum tracking (posledních 5 score updates)
  - [ ] Economic round advantage pro Valorant
  - [ ] Map-specific predictions (Inferno vs Mirage etc.)

### **Milník 3.2: Sniper mode triggers** ⏳
- [ ] Implementovat `should_trigger_sniper()` logiku
- [ ] Thresholds:
  - Confidence ≥ 0.9 → Sniper mode (2s interval)
  - Confidence ≥ 0.85 → High alert
  - Confidence < 0.85 → Normal mode
  
### **Milník 3.3: Historical data collection** ⏳
- [ ] Ukládat match states do JSONL pro analýzu
- [ ] Kalibrovat confidence thresholds na reálných datech
- [ ] Vytvořit dashboard s accuracy stats

### **Milník 3.4: Multi-sport prediction** ⏳
- [ ] Valorant-specific heuristika
- [ ] LoL prediction (gold lead, dragon control)
- [ ] Dota 2 prediction (networth lead, barracks)

---

## 🎯 **FÁZE 4: Sniper Mode Execution**

### **Milník 4.1: SX Bet ultra-fast check** ⏳
- [ ] Zkrátit SX Bet cache refresh na 30s během sniper mode
- [ ] Implementovat `check_orderbook_aggressive()` metodu
- [ ] Priority queue pro high-confidence matches

### **Milník 4.2: Order preparation** ⏳
- [ ] Vytvořit `SniperOrder` struct:
  ```rust
  struct SniperOrder {
      match_id: u64,
      team_to_bet: String,
      confidence: f32,
      max_stake: f64, // ETH
      price_limit: f64,
      created_at: Instant,
      status: OrderStatus,
  }
  ```

### **Milník 4.3: Execution engine** ⏳
- [ ] Integrovat s existujícím `ArbDetector`
- [ ] Přidat `execute_sniper_order()` metodu
- [ ] Implementovat stop-loss/timeout logiku

### **Milník 4.4: Risk management** ⏳
- [ ] Position sizing based on confidence
- [ ] Max exposure per match/sport
- [ ] Circuit breakers při ztrátě

---

## 🎯 **FÁZE 5: Monitoring a Analytics**

### **Milník 5.1: Real-time dashboard** ⏳
- [ ] WebSocket server pro live updates
- [ ] React dashboard s:
  - Live matches grid
  - Confidence scores
  - Sniper mode status
  - Profit/loss tracking

### **Milník 5.2: Alerting system** ⏳
- [ ] Telegram bot vylepšení
- [ ] Webhook alerts pro vysoké confidence
- [ ] Email reports denní/světelné

### **Milník 5.3: Performance metrics** ⏳
- [ ] Latency tracking: detection → order
- [ ] Fill rate analysis
- [ ] Sharpe ratio calculation
- [ ] Drawdown monitoring

---

## 🔧 **TECHNICKÉ ÚKOLY**

### **Krátkodobé (tento týden):**
1. **Phase 0 dokončit**: persistent browser + source onboarding + feed fusion
2. **Challenge bypass hardening** (persistent browser session + cookies reuse + delší challenge wait)
3. **Dokončit HLTV benchmark** - měřit skutečnou latenci (po získání validních IDs)
4. **Fix/validace HLTV selektorů** na reálné stránce s obsahem

### **Střednědobé (2 týdny):**
1. **UK VPS setup** podle guide
2. **Betfair API integration**
3. **Proxy rotation system**
4. **Smarkets API research**

### **Dlouhodobé (1 měsíc):**
1. **Full prediction engine** s kalibrací
2. **Sniper mode execution**
3. **Risk management system**
4. **Dashboard a monitoring**

---

## 🚨 **RIZIKA A KONTINGENČNÍ PLÁNY**

### **Riziko: HLTV blokuje scraping**
- **Contingency:** Použít alternativní zdroje:
  1. **Liquipedia** API pro CS2
  2. **Estnn.com** pro rychlé score updates
  3. **Twitter feeds** pro instant výsledky

### **Riziko: Betfair API nepřístupné**
- **Contingency:** 
  - **Primary:** Smarkets API
  - **Secondary:** Matchbook Exchange  
  - **Fallback:** Pouze SX Bet (nižší likvidita)

### **Riziko: SX Bet oracle zrychlení**
- **Contingency:**
  - Monitorovat jejich GitHub
  - Přidat další Web3 sázkovky:
    - **PolyBet** na Polygonu
    - **MetaBets** na Arbitrum
    - **BetDEX** na Solana

---

## 📊 **METRIKY ÚSPĚCHU**

### **Fáze 1 Úspěch (týden 1):**
- HLTV scraping latency <15s (vs 60s GosuGamers)
- 95% success rate na live matches fetch
- 0 false positives v prediction engine

### **Fáze 2 Úspěch (týden 2):**
- UK VPS běží s 99% uptime
- Betfair API connectivity >95%
- Proxy rotation funguje bez banů

### **Fáze 3 Úspěch (týden 3):**
- Prediction accuracy >80% na testovacích datech
- Sniper mode activation při správných situacích
- Žádné false sniper triggers

### **Fáze 4 Úspěch (týden 4):**
- Fill rate >70% na sniper orders
- Avg latency detection→order <5s
- Positive ROI v testovacím režimu

---

## 👥 **RESPONSIBILITIES**

### **Na tobě (Sonneta):**
- [ ] Založit UK VPS trial (Contabo)
- [ ] Otestovat Betfair API connectivity
- [ ] Poskytnout feedback na prediction accuracy

### **Na mně (AI):**
- [x] Implementovat HLTV scraper
- [x] Vytvořit prediction engine
- [ ] Dokončit benchmark
- [ ] Pomoci s VPS setup issues

---

## 📞 **KOMUNIKACE A FEEDBACK**

### **Daily checkpoints:**
1. **Ráno:** Status update z overnight běhu
2. **Odpoledne:** Benchmark výsledky
3. **Večer:** Plán na další den

### **Feedback loop:**
- Reportovat falešné pozitivy v prediction
- Reportovat missed opportunities (zápasy jsme nezachytili)
- Navrhovat vylepšení heuristiky

---

**Následující krok:** Zprovoznit na tomto Win11 zařízení persistent browser runtime + ručně přihlásit zdroje; následně validovat Rust feed fusion na live zápasech a live kurzech.