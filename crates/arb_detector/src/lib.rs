/// RustMiskoLive — Arb Detector
/// Porovnává Pinnacle fair value vs Polymarket cenu
/// Fáze 1: OBSERVE only — loguje, neobchoduje

use anyhow::{Context, Result};
use logger::{EventLogger, ArbOpportunityEvent, now_iso};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

pub struct ArbDetector {
    logger:       EventLogger,
    observe_only: bool,
    min_edge_pct: f64,
    client:       Client,
    telegram_bot_token: String,
    telegram_chat_id: String,
    // Mapa "home_vs_away" -> (marketHash, sportXeventId)
    active_markets: Arc<RwLock<HashMap<String, (String, String)>>>,
}

impl ArbDetector {
    pub fn new(log_dir: impl Into<std::path::PathBuf>, observe_only: bool) -> Self {
        let detector = Self {
            logger:       EventLogger::new(log_dir),
            observe_only,
            min_edge_pct: 0.03, // 3% minimum edge
            client:       Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_else(|_| Client::new()),
            telegram_bot_token: std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| "8125729036:AAH_rDK4i-xmWlN2OttWLYxN1Wq_vI4Nvv8".to_string()),
            telegram_chat_id: std::env::var("TELEGRAM_CHAT_ID").unwrap_or_else(|_| "6458129071".to_string()),
            active_markets: Arc::new(RwLock::new(HashMap::new())),
        };

        // Spustime background sync pro SX Bet markety
        detector.spawn_sx_market_sync();

        detector
    }

    /// Pomocná funkce na normalizaci názvů týmů (jen malá alfanumerika) pro lepší cache hits.
    fn normalize_team_name(name: &str) -> String {
        name.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect()
    }

    /// Background task pro udržování superrychle cache aktivních trhů na SX Bet
    fn spawn_sx_market_sync(&self) {
        let client = self.client.clone();
        let cache = Arc::clone(&self.active_markets);

        tokio::spawn(async move {
            loop {
                let mut new_cache = HashMap::new();
                
                // 1. Získej všechny aktivní esport ligy ze SX Bet (sportId = 9)
                let mut active_esport_leagues = Vec::new();
                if let Ok(l_resp) = client.get("https://api.sx.bet/leagues").send().await {
                    if let Ok(l_data) = l_resp.json::<serde_json::Value>().await {
                        if let Some(leagues) = l_data.pointer("/data").and_then(|d| d.as_array()) {
                            for l in leagues {
                                let is_active = l.pointer("/active").and_then(|a| a.as_bool()).unwrap_or(false);
                                let is_esports = l.pointer("/sportId").and_then(|s| s.as_u64()).unwrap_or(0) == 9;
                                if is_active && is_esports {
                                    if let Some(l_id) = l.pointer("/leagueId").and_then(|id| id.as_u64()) {
                                        active_esport_leagues.push(l_id);
                                    }
                                }
                            }
                        }
                    }
                }

                info!("Background sync: Found {} active SX Bet e-sports leagues.", active_esport_leagues.len());

                // 2. Pro každou ligu získej aktivní markety
                for league_id in active_esport_leagues.iter() {
                    let url = format!("https://api.sx.bet/markets/active?leagueId={}", league_id);
                    if let Ok(resp) = client.get(&url).send().await {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            if let Some(markets) = data.pointer("/data/markets").and_then(|m| m.as_array()) {
                                for m in markets {
                                    // Chceme jen MoneyLine sázky = type: 52
                                    let type_id = m.pointer("/type").and_then(|t| t.as_u64()).unwrap_or(0);
                                    if type_id == 52 {
                                        let t1_raw = m.pointer("/teamOneName").and_then(|s| s.as_str()).unwrap_or("");
                                        let t2_raw = m.pointer("/teamTwoName").and_then(|s| s.as_str()).unwrap_or("");
                                        
                                        let t1 = Self::normalize_team_name(t1_raw);
                                        let t2 = Self::normalize_team_name(t2_raw);
                                        
                                        let hash = m.pointer("/marketHash").and_then(|s| s.as_str()).unwrap_or("").to_string();
                                        let event_id = m.pointer("/sportXeventId").and_then(|s| s.as_str()).unwrap_or("").to_string();
                                        
                                        if !t1.is_empty() && !t2.is_empty() && !hash.is_empty() {
                                            new_cache.insert(format!("{}_vs_{}", t1, t2), (hash.clone(), event_id.clone()));
                                            new_cache.insert(format!("{}_vs_{}", t2, t1), (hash, event_id)); // pro oba smery
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    // Bezprostřední propis do cache
                    {
                        let mut lock = cache.write().await;
                        for (k, v) in new_cache.drain() {
                            lock.insert(k, v);
                        }
                    }

                    // Zvolni, abychom nezaspamovali SX Bet API
                    sleep(Duration::from_millis(200)).await;
                }

                let total_items = cache.read().await.len() / 2;
                info!("Background sync completed: Cached {} mapped SX Bet moneyline matches.", total_items);
                
                // Osvěžíme za minutu
                sleep(Duration::from_secs(60)).await; 
            }
        });
    }

    /// Porovnej Pinnacle implied prob vs Polymarket price
    /// pinnacle_prob: 0.0–1.0 (fair value bez vigu)
    /// polymarket_price: 0.0–1.0 (YES cena na CLOB)
    pub fn evaluate_pinnacle_vs_polymarket(
        &self,
        home:             &str,
        away:             &str,
        sport:            &str,
        pinnacle_prob:    f64,  // fair value
        polymarket_price: f64,  // aktuální tržní cena
        condition_id:     &str,
    ) {
        // Edge = fair value - market price
        // Pokud Polymarket podhodnotí (cena < fair value) → edge na BUY
        let edge = pinnacle_prob - polymarket_price;

        if edge < self.min_edge_pct {
            return; // pod threshold → ticho
        }

        let action = if self.observe_only { "OBSERVE" } else { "BUY" };

        let ev = ArbOpportunityEvent {
            ts:               now_iso(),
            event:            "ARB_OPPORTUNITY",
            source:           "pinnacle_vs_polymarket".to_string(),
            home:             home.to_string(),
            away:             away.to_string(),
            sport:            sport.to_string(),
            edge_pct:         edge,
            pinnacle_prob,
            polymarket_price,
            action:           action.to_string(),
        };

        info!(
            edge = format!("{:.1}%", edge * 100.0),
            pinnacle_prob = format!("{:.2}", pinnacle_prob),
            polymarket   = format!("{:.2}", polymarket_price),
            "{} vs {} — edge found (Condition: {})",
            home, away, condition_id
        );

        let _ = self.logger.log(&ev);

        // Telegram Notification
        let bot_token = self.telegram_bot_token.clone();
        let chat_id = self.telegram_chat_id.clone();
        let client = self.client.clone();
        let h = home.to_string();
        let a = away.to_string();
        
        if !bot_token.is_empty() && !chat_id.is_empty() {
            let decimal_odds = 1.0 / polymarket_price;
            let msg = format!(
                "🚨 EDGE {:.1}% se našla pro zápas {} vs {}!\n\nVýhra by byla {:.2}x.\nFair Prob: {:.2} vs SX Prob: {:.2}", 
                edge * 100.0, h, a, decimal_odds, pinnacle_prob, polymarket_price
            );
            
            tokio::spawn(async move {
                let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
                let payload = serde_json::json!({
                    "chat_id": chat_id,
                    "text": msg,
                });
                if let Err(e) = client.post(&url).json(&payload).send().await {
                    warn!("Failed to send Telegram notification: {}", e);
                }
            });
        }
    }

    /// Bleskově najde SX Bet market přes cache a sáhne po likviditě do Orderbooku
    pub async fn evaluate_esports_match(&self, home: &str, away: &str, sport: &str, winner: &str) -> Result<()> {
        let t1 = Self::normalize_team_name(home);
        let t2 = Self::normalize_team_name(away);
        let key = format!("{}_vs_{}", t1, t2);

        let overall_start = std::time::Instant::now();
        
        let (market_hash, event_id) = {
            let cache = self.active_markets.read().await;
            
            // Prohledame i substringove (pri castecne normalizaci) pokud exaktni match selze
            let exact_match = cache.get(&key).cloned();
            
            if exact_match.is_none() {
                // Pokusime se najit substring match v klicich (drazsi operace, ale match_resolved se nestava tak casto)
                let partial_match = cache.keys().find(|k| k.contains(&t1) && k.contains(&t2));
                if let Some(p_key) = partial_match {
                     cache.get(p_key).cloned()
                } else {
                    None
                }
            } else {
                exact_match
            }
        }.unwrap_or((String::new(), String::new()));
        
        if market_hash.is_empty() {
            tracing::debug!("No cached SX Bet market found for {} vs {} (key: {})", home, away, key);
            return Ok(());
        }

        let cache_elapsed = overall_start.elapsed().as_micros();
        info!("⚡ FAST LOOKUP: {} vs {} mapped to SX Event {} in {}µs", home, away, event_id, cache_elapsed);

        // Nyní jdeme okamžitě rovnou na orderbook (/orders?marketHash=X) přečíst nejlepší kurzy
        let orders_url = format!("https://api.sx.bet/orders?marketHash={}", market_hash);
        
        let req_start = std::time::Instant::now();
        let orders_resp = self.client.get(&orders_url)
            .send().await.context("SX Bet orders API failed")?;
            
        let pm_orders: serde_json::Value = orders_resp.json().await.context("SX Bet JSON parse failed")?;
        
        // Cti data orders a najdi bet pro vítěze
        let mut best_guaranteed_prob = 1.0; 
        
        if let Some(orders_arr) = pm_orders.pointer("/data").and_then(|d| d.as_array()) {
            for order in orders_arr {
                let status = order.pointer("/orderStatus").and_then(|s| s.as_str()).unwrap_or("");
                if status != "ACTIVE" { continue; }

                // Determine whose bet this is - MakerOutcomeOne
                let is_t1 = order.pointer("/isMakerBettingOutcomeOne").and_then(|b| b.as_bool()).unwrap_or(false);
                let order_winner = if is_t1 { Self::normalize_team_name(home) } else { Self::normalize_team_name(away) };

                // My chceme vzít BUY objednávku na YES pro 'winner'. 
                // Zjednodusime - SX Bet nabizi kurzy makeru, taker sází proti nim
                if order_winner.contains(&Self::normalize_team_name(winner)) {
                    let prob_str = order.pointer("/percentageOdds").and_then(|s| s.as_str()).unwrap_or("0");
                    if let Ok(prob_u128) = prob_str.parse::<u128>() {
                        // Převod z 10^18 formátu do float: např 95000000000000000000 -> 95.0 -> 0.95
                        let dec_prob = (prob_u128 as f64) / 100_000_000_000_000_000_000.0;
                        if dec_prob < best_guaranteed_prob && dec_prob > 0.01 {
                            best_guaranteed_prob = dec_prob;
                        }
                    }
                }
            }
        }

        let req_elapsed = req_start.elapsed().as_millis();
        let total_elapsed = overall_start.elapsed().as_millis();
        info!("⚡ SX API Ping: {}ms | Total Arb Eval: {}ms | Best Edge Prob: {:.2}", req_elapsed, total_elapsed, best_guaranteed_prob);

        if best_guaranteed_prob < 1.0 {
            // Evaluace: Pinnacle je teď vlastně "skutečný vývoj reality" = 100% tzn 1.0 
            // My jsme našli trh na SX Betu s kurzem odpovídajícím best_guaranteed_prob.
            self.evaluate_pinnacle_vs_polymarket(home, away, sport, 1.0, best_guaranteed_prob, &market_hash);
        } else {
            info!("No profitable volume left on SX Bet for {}", winner);
        }

        Ok(())
    }

    /// Debugovaci pomucka pro vypsani obsahu cache
    pub async fn debug_print_cache(&self) {
        let cache = self.active_markets.read().await;
        info!("--- CURRENT SX BET CACHE DUMP ({} items) ---", cache.len());
        for (key, val) in cache.iter().take(15) { // ukaž prvnich 15 pro prehled
            info!("MAPPED: {} -> SX Event ID: {}", key, val.1);
        }
    }
}
