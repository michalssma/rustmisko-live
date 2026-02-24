# NAVRH — Optimalizace latence a přístupu k likvidním burzám

Aktualizováno: 2026-02-23
Status: NÁVRH pro implementaci

## CÍL: Snížit detection lag z 1-2 min na <15s a získat přístup k Betfair/Smarkets

## 🎯 1. RAPIDNÍ ESPORT DATA: Scraping místo placených API

### HLTV.org (CS2) — Gold Standard pro low-latency

**Struktura URL:**
```
https://www.hltv.org/matches/<match_id>/<team1>-vs-<team2>
Příklad: https://www.hltv.org/matches/2365125/natus-vincere-vs-faze
```

**DOM elementy pro live skóre:**
```html
<!-- Skóre týmů -->
<div class="team1-gradient">
    <div class="teamName">Natus Vincere</div>
    <div class="score">13</div>
</div>
<div class="team2-gradient">
    <div class="teamName">FaZe</div>
    <div class="score">8</div>
</div>

<!-- Stav zápasu -->
<div class="countdown">LIVE</div>  <!-- nebo "Match over" -->
```

**Implementace v `crates/esports_monitor/src/lib.rs`:**
```rust
// Nová metoda pro HLTV live tracking
pub async fn poll_hltv_live() -> Vec<LiveMatch> {
    // 1. Nejprve získej aktuální live matches z /matches
    // 2. Pro každý match scrapni detailní stránku
    // 3. Extrahuj skóre a stav
    // 4. Pokud skóre >= 13 (CS2) nebo 13+ rozdíl, označ jako "likely finished"
}
```

### Trackergg.com (Valorant) — Real-time scoreboard

**Struktura:**
```
https://tracker.gg/valorant/match/<match_id>
```

**Klíčové selektory:**
```css
/* Skóre týmů */
div.scoreboard__team--red [data-stat="score"]
div.scoreboard__team--blue [data-stat="score"]

/* Stav zápasu */
div.match-header__status:contains("COMPLETE")
```

**Výhoda:** Trackergg updatuje každý round v reálném čase (~3-5s delay).

### LoL Esports (leagueoflegends.com) — Oficiální API

**Endpoint pro live:**
```
GET https://esports-api.lolesports.com/persisted/gw/getSchedule?hl=en-US&leagueId=<id>
```

**Headery (stejné jako v `getSchedule`):**
```
x-api-key: 0TvQnueqKa5mxJntVWt0w4LpLfEkrV1Ta8rQBb9Z
```

**Výhoda:** Oficiální API, 0 scraping overhead, update každých 10s.

### Liquipedia (Dota 2, StarCraft II) — Community wiki

**API endpoint:**
```
https://liquipedia.net/<game>/api.php?action=parse&page=Tournament&prop=text&format=json
```

**Výhoda:** Machine-readable data, často rychlejší než GosuGamers.

---

## 🎯 2. UK VPS + PROXY SETUP pro Betfair/Smarkets

### Krok za krokem:

1. **Založ VPS u Contabo (UK London):**
   - 7denní trial: https://contabo.com/en/vps/
   - Vyber London datacenter
   - Minimální konfigurace: 2 vCPU, 4GB RAM (£4.99/měs)

2. **Nastav Rust prostředí na VPS:**
```bash
# Na VPS
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo apt-get update
sudo apt-get install -y build-essential
```

3. **Klonuj RustMiskoLive:**
```bash
git clone <repo-url>
cd RustMiskoLive
cargo build --release
```

4. **Konfigurace proxy pro rotaci IP:**
```rust
// crates/price_monitor/src/betfair.rs
use reqwest::{Client, Proxy};

struct BetfairClient {
    client: Client,
    proxy_list: Vec<String>,
    current_proxy_idx: usize,
}

impl BetfairClient {
    fn rotate_proxy(&mut self) {
        self.current_proxy_idx = (self.current_proxy_idx + 1) % self.proxy_list.len();
        // Recreate client with new proxy
    }
}
```

5. **Nákup residential proxy:**
   - Luminati (Bright Data): ~$15/měs za 5GB
   - Smartproxy: ~$12/měs
   - **Důležité:** Vyber UK residential IP pro Betfair

---

## 🎯 3. PREDICTION ENGINE: Heuristika pro early detection

### Implementace v `crates/prediction_engine/src/lib.rs`:

```rust
#[derive(Debug, Clone)]
pub struct MatchState {
    pub sport: String,           // "cs2", "valorant", "lol", "dota2"
    pub score_team1: u8,
    pub score_team2: u8,
    pub map_number: u8,          // 1, 2, 3 (pro Bo3)
    pub total_maps: u8,          // 3 pro Bo3, 5 pro Bo5
    pub is_live: bool,
    pub last_update: DateTime<Utc>,
}

#[derive(Debug, PartialEq)]
pub enum Prediction {
    Team1Win(f32),  // confidence 0.0-1.0
    Team2Win(f32),
    Uncertain,
}

impl MatchState {
    pub fn predict(&self) -> Prediction {
        match self.sport.as_str() {
            "cs2" => self.predict_cs2(),
            "valorant" => self.predict_valorant(),
            "lol" => self.predict_lol(),
            "dota2" => self.predict_dota2(),
            _ => Prediction::Uncertain,
        }
    }
    
    fn predict_cs2(&self) -> Prediction {
        // CS2: vyhrává se na 13 vítězných roundů
        if self.score_team1 >= 13 && self.score_team1 - self.score_team2 >= 2 {
            Prediction::Team1Win(1.0)
        } else if self.score_team2 >= 13 && self.score_team2 - self.score_team1 >= 2 {
            Prediction::Team2Win(1.0)
        } else if self.score_team1 == 12 && self.score_team2 <= 10 {
            // 12:10 → velmi vysoká šance na výhru
            Prediction::Team1Win(0.95)
        } else if self.score_team2 == 12 && self.score_team1 <= 10 {
            Prediction::Team2Win(0.95)
        } else if self.score_team1 >= 11 && self.score_team1 - self.score_team2 >= 5 {
            // Např. 11:6 → ~85% šance
            Prediction::Team1Win(0.85)
        } else {
            Prediction::Uncertain
        }
    }
    
    fn predict_valorant(&self) -> Prediction {
        // Valorant: vyhrává se na 13
        if self.score_team1 >= 13 && self.score_team1 - self.score_team2 >= 2 {
            Prediction::Team1Win(1.0)
        } else if self.score_team2 >= 13 && self.score_team2 - self.score_team1 >= 2 {
            Prediction::Team2Win(1.0)
        } else if self.score_team1 == 12 && self.score_team2 <= 9 {
            // 12:9 → prakticky jistota
            Prediction::Team1Win(0.98)
        } else {
            Prediction::Uncertain
        }
    }
}
```

### Integrace do main loopu:

```rust
// V main.rs
let prediction_engine = PredictionEngine::new();

// V každém cyklu
for match in &live_matches {
    let state = MatchState {
        sport: match.sport.clone(),
        score_team1: match.score1,
        score_team2: match.score2,
        map_number: match.map_number,
        total_maps: match.total_maps,
        is_live: true,
        last_update: Utc::now(),
    };
    
    match prediction_engine.predict(&state) {
        Prediction::Team1Win(confidence) if confidence >= 0.9 => {
            // Sniper mode: zkrátit interval na 2s
            info!("🔥 PREDICTION: {} wins with {:.0}% confidence", match.home, confidence*100.0);
            // Začít častěji kontrolovat SX Bet orderbook
            trigger_sniper_mode(&match).await;
        }
        _ => {}
    }
}
```

---

## 🎯 4. SNIPER MODE: Ultra-low latency execution

### Když predikce >90%:

1. **Zkrátit poll interval na 2s** pro daný zápas
2. **Připravit limit order** na SX Bet:
   - Cena: current_best_bid + 0.001 ETH (pro lepší pozici v orderbooku)
   - Velikost: 0.01-0.05 ETH (testovací)
3. **Monitorovat HLTV/Trackergg každou sekundu**
4. **Spustit order okamžitě** při detekci "Match over"

```rust
async fn trigger_sniper_mode(match: &LiveMatch) {
    // Založ dedicated tok pro tento zápas
    tokio::spawn(async move {
        let mut sniper_interval = tokio::time::interval(Duration::from_secs(2));
        
        loop {
            sniper_interval.tick().await;
            
            // Ultra-fast check na finální výsledek
            if let Ok(final_score) = fetch_ultra_fast_score(&match.id).await {
                if final_score.is_conclusive() {
                    // EXECUTE ORDER
                    execute_sx_bet_order(&match, final_score).await;
                    break;
                }
            }
        }
    });
}
```

---

## 📋 IMPLEMENTAČNÍ ROADMAP

### Fáze 1 (Tento týden): HLTV scraping prototype
1. Vytvoř `crates/hltv_scraper/src/lib.rs`
2. Implementuj `fetch_live_scores()`
3. Benchmark vs. GosuGamers (měř latency)

### Fáze 2 (7 den trial): UK VPS setup
1. Založ Contabo VPS
2. Otestuj Betfair API z UK IP
3. Implementuj proxy rotaci

### Fáze 3: Prediction engine
1. Vytvoř `crates/prediction_engine`
2. Integruj do main loopu
3. Kalibruj heuristiku na historických datech

### Fáze 4: Sniper mode
1. Implementuj multi-threaded sniper
2. Test s malým kapitálem (0.01 ETH)
3. Monitoruj fill rate a slippage

---

## ⚠️ RIZIKA A MITIGACE

1. **HLTV rate limiting:**
   - Rotace user-agent
   - Respect `robots.txt`
   - Backup: trackergg pro Valorant

2. **Betfair ban přes VPS:**
   - Používat residential proxy místo datacenter IP
   - Limit requestů na 10/min
   - Monitorovat HTTP 429 (Too Many Requests)

3. **SX Bet oracle zrychlení:**
   - Diversifikace: přidat další Web3 sázkovky (PolyBet, MetaBets)
   - Sledovat jejich GitHub pro změny v oracle contracts

---

## 🔬 METRIKY PRO ÚSPĚCH

- **Latence detekce:** <15s (aktuálně 60-120s)
- **Fill rate orders:** >70% (aktuálně 0% - observe only)
- **ROI měsíční:** >20% po poplatcích
- **Uptime:** >95%

---

**Další krok:** Začni s implementací HLTV scraperu. Můžeme iterativně testovat každou komponentu.
