use anyhow::Result;
use dotenv::dotenv;
use arb_detector::ArbDetector;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    // Initialize standard logger
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    tracing::info!("=== PROOF OF MAPPING: SX Bet Cache vs Scraping ===");
    
    // 1. Nastartujeme ArbDetector v Observe Only módu
    let arb = ArbDetector::new("logs", true);
    
    tracing::info!("Waiting 15 seconds for the background Tokio task to pull all 64 SX Bet leagues...");
    sleep(Duration::from_secs(15)).await;

    // TODO: Zde zkusime "uměle" předhodit pár týmů, o kterých víme, že dnes hrají
    // nebo zkusime dumpnout cast cache, at vidime, co presne SX Bet nabizi.
    
    // Abychom ukazali NAOSTRO, podivame se do VLR.gg / GosuGamers na nadcházející zápasy.
    // Pro ukázku zkusíme pár populárních jmen, co by mohla hrát.
    let test_teams = vec![
        ("Natus Vincere", "Team Liquid"),
        ("Fnatic", "Karmine Corp"),
        ("Paper Rex", "DRX"),
        ("Sentinels", "LOUD"),
        ("Bnk Fearx", "Dplus KIA"), // Tohle vime ze tam je
        ("Cloud9", "NRG"),
        ("G2 Esports", "Faze Clan"),
    ];

    tracing::info!("Testing {} potential live matches against SX Bet orderbooks...", test_teams.len());
    
    for (t1, t2) in test_teams {
        // Zkusime evaluate. Pokud to neni v SX Betu, hodi to "No cached market" a projde to hned.
        // Pokud to je, spoji se to s Orderbookem a vypise to mozny edge.
        let _ = arb.evaluate_esports_match(t1, t2, "test_sport", t1).await;
    }
    
    tracing::info!("Dumping all ACTIVE SX Bet markets found in cache:");
    arb.debug_print_cache().await;

    tracing::info!("Proof of mapping finished!");
    Ok(())
}
