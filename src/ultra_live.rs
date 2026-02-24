//! Ultra-low latency live observer s HLTV scrapingem a prediction engine
//! Target: <15s detection latency (vs. 60-120s původně)
//!
//! Spustit: cargo run --bin ultra-live

use anyhow::Result;
use dotenv::dotenv;
use hltv_scraper::{HltvScraper, HltvLiveMatch};
use prediction_engine::{PredictionEngine, MatchState, Prediction, match_state_from_hltv};
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::{sleep, interval};
use tracing::{info, warn, error, debug};
use tracing_subscriber::{EnvFilter, fmt};

/// Sniper mode pro jednotlivé zápasy
struct SniperSession {
    match_id: u64,
    team1: String,
    team2: String,
    last_prediction: Option<Prediction>,
    started_at: Instant,
    checks_count: u32,
}

impl SniperSession {
    fn new(match_id: u64, team1: String, team2: String) -> Self {
        Self {
            match_id,
            team1,
            team2,
            last_prediction: None,
            started_at: Instant::now(),
            checks_count: 0,
        }
    }
    
    /// Aktualizuj predikci
    fn update_prediction(&mut self, prediction: Prediction) -> bool {
        let prev_conf = self.last_prediction.as_ref()
            .and_then(|p| p.confidence())
            .unwrap_or(0.0);
        let new_conf = prediction.confidence().unwrap_or(0.0);
        
        self.last_prediction = Some(prediction.clone());
        self.checks_count += 1;
        
        // Log změny confidence
        if new_conf >= 0.9 && prev_conf < 0.9 {
            info!("🔥 SNIPER ACTIVATED for {} vs {}: confidence ↑ {:.0}%", 
                self.team1, self.team2, new_conf * 100.0);
            return true;
        }
        
        false
    }
    
    /// Je session příliš stará? (timeout 30 minut)
    fn is_expired(&self) -> bool {
        self.started_at.elapsed() > Duration::from_secs(1800)
    }
}

/// Hlavní ultra-low latency monitor
struct UltraLiveMonitor {
    hltv_scraper: Arc<Mutex<HltvScraper>>,
    prediction_engine: Arc<Mutex<PredictionEngine>>,
    sniper_sessions: Arc<Mutex<HashMap<u64, SniperSession>>>,
    /// Cache pro detekci ukončených zápasů
    previous_live_ids: Arc<Mutex<HashSet<u64>>>,
}

impl UltraLiveMonitor {
    fn new() -> Self {
        Self {
            hltv_scraper: Arc::new(Mutex::new(HltvScraper::new())),
            prediction_engine: Arc::new(Mutex::new(PredictionEngine::new())),
            sniper_sessions: Arc::new(Mutex::new(HashMap::new())),
            previous_live_ids: Arc::new(Mutex::new(HashSet::new())),
        }
    }
    
    /// Hlavní monitoring loop
    async fn run(&self) -> Result<()> {
        info!("🚀 ULTRA-LIVE MONITOR starting - Target latency <15s");
        info!("Sources: HLTV.org (CS2), VLR.gg (Valorant)");
        info!("Strategy: Prediction engine + Sniper mode");
        
        let mut normal_interval = interval(Duration::from_secs(10));
        let mut sniper_interval = interval(Duration::from_secs(2));
        
        loop {
            // Zkontroluj jestli máme aktivní sniper sessions
            let has_active_snipers = {
                let sessions = self.sniper_sessions.lock().await;
                !sessions.is_empty()
            };
            
            let sleep_duration = if has_active_snipers {
                sniper_interval.tick().await;
                Duration::from_secs(2)
            } else {
                normal_interval.tick().await;
                Duration::from_secs(10)
            };
            
            // Spusť monitoring cyklus
            if let Err(e) = self.monitor_cycle().await {
                error!("Monitor cycle failed: {}", e);
            }
            
            // Vyčisti staré sniper sessions
            self.cleanup_expired_sessions().await;
        }
    }
    
    /// Jeden monitoring cyklus
    async fn monitor_cycle(&self) -> Result<()> {
        let mut scraper = self.hltv_scraper.lock().await;
        let mut sessions = self.sniper_sessions.lock().await;
        let mut previous_ids = self.previous_live_ids.lock().await;
        
        // 1. Získej aktuální live zápasy z HLTV
        let current_live_ids = match scraper.fetch_live_matches().await {
            Ok(ids) => ids,
            Err(e) => {
                warn!("HLTV fetch failed: {}", e);
                return Ok(());
            }
        };
        
        // 2. Pro každý live zápas získej detaily a predikuj
        for &match_id in &current_live_ids {
            if let Ok(Some(match_data)) = scraper.fetch_match_details(match_id).await {
                // Vytvoř match state pro predikci
                let state = match_state_from_hltv(
                    "cs2",
                    &match_data.team1,
                    &match_data.team2,
                    match_data.score1,
                    match_data.score2,
                    1, // map_number (prozatím 1)
                    3, // total_maps (prozatím Bo3)
                    match_data.is_live,
                );
                
                // Proveď predikci
                let prediction_engine = self.prediction_engine.lock().await;
                let prediction = prediction_engine.predict(&state);
                
                // Log detaily
                debug!("HLTV: {} vs {} ({}-{}) - Prediction: {:?}", 
                    match_data.team1, match_data.team2, 
                    match_data.score1, match_data.score2, 
                    prediction);
                
                // Zkontroluj zda potřebujeme sniper mode
                if let Some(conf) = prediction.confidence() {
                    if conf >= 0.85 {
                        // Spusť nebo aktualizuj sniper session
                        let session = sessions.entry(match_id)
                            .or_insert_with(|| SniperSession::new(
                                match_id,
                                match_data.team1.clone(),
                                match_data.team2.clone(),
                            ));
                        
                        if session.update_prediction(prediction.clone()) {
                            // Sniper byl aktivován - zvýšená frekvence kontrol
                            info!("🎯 SNIPER MODE ENGAGED for {} vs {} (conf: {:.0}%)",
                                match_data.team1, match_data.team2, conf * 100.0);
                            
                            // TODO: Trigger SX Bet orderbook check každé 2s
                            self.trigger_sx_bet_sniper(&match_data, conf).await;
                        }
                    }
                }
                
                // Emituj událost pokud je to nový live zápas
                if !previous_ids.contains(&match_id) {
                    info!("🔴 NEW LIVE: {} vs {} ({}-{})",
                        match_data.team1, match_data.team2,
                        match_data.score1, match_data.score2);
                }
            }
        }
        
        // 3. Detekuj ukončené zápasy
        let finished_ids: Vec<u64> = previous_ids.iter()
            .filter(|&id| !current_live_ids.contains(id))
            .cloned()
            .collect();
        
        for match_id in finished_ids {
            if let Some(session) = sessions.remove(&match_id) {
                info!("✅ MATCH FINISHED (sniper session ended): {} vs {} ({} checks)",
                    session.team1, session.team2, session.checks_count);
            }
            
            previous_ids.remove(&match_id);
            
            // TODO: Final check na SX Bet pro oracle lag arb
            info!("   → Should check SX Bet for oracle lag opportunity");
        }
        
        // 4. Aktualizuj previous IDs
        *previous_ids = current_live_ids.into_iter().collect();
        
        Ok(())
    }
    
    /// Trigger pro SX Bet sniper mode
    async fn trigger_sx_bet_sniper(&self, match_data: &HltvLiveMatch, confidence: f32) {
        // TODO: Implementovat SX Bet orderbook check
        // Prozatím jen log
        info!("🎯 SX BET SNIPER: Checking orderbook for {} vs {} (conf: {:.0}%)",
            match_data.team1, match_data.team2, confidence * 100.0);
        
        // Simulace: pokud confidence > 95%, začni intenzivně kontrolovat
        if confidence > 0.95 {
            info!("   ⚡ ULTRA-HIGH CONFIDENCE - Starting aggressive orderbook polling");
        }
    }
    
    /// Vyčisti expirované sniper sessions
    async fn cleanup_expired_sessions(&self) {
        let mut sessions = self.sniper_sessions.lock().await;
        let expired_ids: Vec<u64> = sessions.iter()
            .filter(|(_, session)| session.is_expired())
            .map(|(&id, _)| id)
            .collect();
        
        for id in expired_ids {
            if let Some(session) = sessions.remove(&id) {
                warn!("🕒 Sniper session expired: {} vs {} ({} checks, {}s old)",
                    session.team1, session.team2,
                    session.checks_count,
                    session.started_at.elapsed().as_secs());
            }
        }
    }
}

/// Hlavní funkce
#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    
    // Setup logging
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info"))
        )
        .init();
    
    info!("=== ULTRA-LIVE MONITOR ===");
    info!("Strategy: HLTV scraping + Prediction Engine + Sniper Mode");
    info!("Target latency: <15s (vs. 60-120s original)");
    info!("Sniper mode: 2s interval při high confidence");
    
    let monitor = UltraLiveMonitor::new();
    monitor.run().await?;
    
    Ok(())
}

/// Testovací funkce pro jednorázový fetch
async fn test_hltv_fetch() -> Result<()> {
    info!("🧪 Testing HLTV scraper...");
    
    let mut scraper = HltvScraper::new();
    
    match scraper.fetch_live_matches().await {
        Ok(ids) => {
            info!("Found {} live matches: {:?}", ids.len(), ids);
            
            if let Some(&first_id) = ids.first() {
                match scraper.fetch_match_details(first_id).await {
                    Ok(Some(match_data)) => {
                        info!("Match {} details:", first_id);
                        info!("  Teams: {} vs {}", match_data.team1, match_data.team2);
                        info!("  Score: {}-{}", match_data.score1, match_data.score2);
                        info!("  Live: {}", match_data.is_live);
                        
                        // Test predikce
                        match match_data.predict() {
                            hltv_scraper::MatchPrediction::Team1Win(conf) => {
                                info!("  Prediction: {} wins with {:.0}% confidence", 
                                    match_data.team1, conf * 100.0);
                            }
                            hltv_scraper::MatchPrediction::Team2Win(conf) => {
                                info!("  Prediction: {} wins with {:.0}% confidence", 
                                    match_data.team2, conf * 100.0);
                            }
                            hltv_scraper::MatchPrediction::Uncertain => {
                                info!("  Prediction: Uncertain");
                            }
                        }
                    }
                    Ok(None) => info!("Match {} not found", first_id),
                    Err(e) => warn!("Failed to fetch match details: {}", e),
                }
            }
        }
        Err(e) => warn!("Failed to fetch live matches: {}", e),
    }
    
    Ok(())
}