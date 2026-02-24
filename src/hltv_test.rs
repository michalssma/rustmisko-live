//! Testovací binárka pro HLTV scraper
//! Spustit: cargo run --bin hltv-test

use anyhow::Result;
use hltv_scraper::{HltvScraper, HltvLiveMatch};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Setup logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    
    info!("🚀 HLTV Scraper Test - Target latency <15s");
    
    let mut scraper = HltvScraper::new();

    info!("🩺 Probing HLTV endpoints...");
    for probe_url in ["https://www.hltv.org/live", "https://www.hltv.org/results"] {
        match scraper.probe_endpoint(probe_url).await {
            Ok(probe) => {
                info!(
                    "Probe {} -> html_len={}, match_ids={}, challenge_page={}",
                    probe.url,
                    probe.html_len,
                    probe.match_id_count,
                    probe.looks_like_challenge_page
                );
            }
            Err(e) => warn!("Probe {} failed: {}", probe_url, e),
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    
    // 1) Test jednorázového načtení live zápasů
    info!("🔍 Fetching live matches from HLTV...");
    
    match scraper.fetch_live_matches().await {
        Ok(match_ids) => {
            if match_ids.is_empty() {
                info!("No live matches found on HLTV /live.");

                // 2) Fallback validace parseru na recent zápasech
                info!("🔁 Validating parser via /results fallback...");
                tokio::time::sleep(Duration::from_secs(3)).await;
                match scraper.fetch_recent_match_ids(5).await {
                    Ok(recent_ids) if !recent_ids.is_empty() => {
                        info!("Recent match candidates: {:?}", recent_ids);
                        let first_id = recent_ids[0];
                        info!("Fetching details for recent match {}...", first_id);
                        match scraper.fetch_match_details(first_id).await {
                            Ok(Some(match_data)) => {
                                info!("Recent match detail OK: {} vs {} ({}-{}) live:{}",
                                    match_data.team1,
                                    match_data.team2,
                                    match_data.score1,
                                    match_data.score2,
                                    match_data.is_live,
                                );
                            }
                            Ok(None) => warn!("Recent match {} details not found", first_id),
                            Err(e) => warn!("Recent match details fetch failed: {}", e),
                        }
                    }
                    Ok(_) => warn!("No recent match IDs found on /results fallback"),
                    Err(e) => warn!("/results fallback failed: {}", e),
                }
            } else {
                info!("Found {} live matches: {:?}", match_ids.len(), match_ids);
                
                // Test detailů prvního zápasu
                if let Some(&first_id) = match_ids.first() {
                    info!("Fetching details for match {}...", first_id);
                    match scraper.fetch_match_details(first_id).await {
                        Ok(Some(match_data)) => {
                            info!("Match details:");
                            info!("  Teams: {} vs {}", match_data.team1, match_data.team2);
                            info!("  Score: {}-{}", match_data.score1, match_data.score2);
                            info!("  Live: {}", match_data.is_live);
                            
                            // Test predikce
                            match match_data.predict() {
                                hltv_scraper::MatchPrediction::Team1Win(conf) => {
                                    info!("  Prediction: {} wins with {:.0}% confidence", match_data.team1, conf * 100.0);
                                }
                                hltv_scraper::MatchPrediction::Team2Win(conf) => {
                                    info!("  Prediction: {} wins with {:.0}% confidence", match_data.team2, conf * 100.0);
                                }
                                hltv_scraper::MatchPrediction::Uncertain => {
                                    info!("  Prediction: Uncertain");
                                }
                            }
                            
                            // Test conclusive check
                            if match_data.is_conclusive() {
                                info!("  ⚡ CONCLUSIVE - Ready for sniper mode!");
                            }
                        }
                        Ok(None) => {
                            warn!("Match {} details not found", first_id);
                        }
                        Err(e) => {
                            warn!("Failed to fetch match details: {}", e);
                        }
                    }
                }
            }
        }
        Err(e) => {
            warn!("Failed to fetch live matches: {}", e);
        }
    }
    
    // 3) Test kontinuálního monitoringu s callbackem (bounded)
    info!("\n🎯 Starting continuous monitoring for 25s (10s interval)...");
    
    let (tx, mut rx) = mpsc::channel::<HltvLiveMatch>(10);
    
    // Spusť monitoring v background task
    let monitor_handle = tokio::spawn(async move {
        let mut scraper = HltvScraper::new();
        
        if let Err(e) = scraper.monitor_live_matches(move |match_data| {
            if let Err(_) = tx.blocking_send(match_data) {
                // Channel closed, ignore
            }
        }).await {
            warn!("Monitor failed: {}", e);
        }
    });
    
    let monitor_deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let now = Instant::now();
        if now >= monitor_deadline {
            break;
        }

        let remaining = monitor_deadline.duration_since(now);
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(match_data)) => {
                info!("📡 HLTV UPDATE: {} vs {} ({}-{}) live: {}", 
                    match_data.team1, match_data.team2, 
                    match_data.score1, match_data.score2, 
                    match_data.is_live);

                if let Some((winner, confidence)) = match_data.predicted_winner() {
                    if confidence >= 0.9 {
                        info!("🔥 HIGH CONFIDENCE: {} wins with {:.0}% confidence", winner, confidence * 100.0);
                        info!("   → Should trigger sniper mode on SX Bet!");
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    
    // Cleanup
    monitor_handle.abort();
    info!("Test completed (bounded run).");
    
    Ok(())
}
