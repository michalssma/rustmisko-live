/// RustMiskoLive — Live Odds Observer (48h observe only)
///
/// Co dělá:
///   1. Každých 60s stahuje Pinnacle lines (sharp benchmark)
///   2. Každých 60s dotazuje odds-api.io /arbitrage-bets
///   3. Loguje vše do logs/YYYY-MM-DD.jsonl
///   4. NTFY alert při edge >3%
///
/// Co NEDĚLÁ: žádné ordery (observe_only = true)
///
/// Před spuštěním:
///   cp .env.example .env
///   cargo run --bin live-observer

use anyhow::Result;
use dotenv::dotenv;
use esports_monitor::EsportsMonitor;
use arb_detector::ArbDetector;
use std::env;
use std::fs::File;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info"))
        )
        .init();

    info!("=== RustMiskoLive Observer — Esports Phase ===");
    info!("Mode: OBSERVE ONLY (no trades)");
    info!("Logging: Liquipedia/HTML resolved matches");
    info!("Logs: ./logs/");

    // 1. Single instance lock (Process Safety)
    let lock_file_path = env::temp_dir().join("rustmiskolive_esports.lock");
    let lock_file = match File::create(&lock_file_path) {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to create lock file at {:?}: {}", lock_file_path, e);
            return Ok(());
        }
    };

    let mut lock = fd_lock::RwLock::new(lock_file);
    let _write_guard = match lock.try_write() {
        Ok(guard) => {
            info!("Acquired single-instance lock.");
            guard
        }
        Err(_) => {
            warn!("Another instance of live-observer is already running! Exiting.");
            return Ok(());
        }
    };

    // 2. Načtení env
    let poll_interval_secs = env::var("ESPORTS_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15);

    info!("Poll interval: {}s", poll_interval_secs);

    let monitor = EsportsMonitor::new("logs", poll_interval_secs);
    let arb = ArbDetector::new("logs", true); // Observe only mode pro logovani.

    info!("Starting poll loop ({}s interval)...", poll_interval_secs);

    info!("⏳ SYSTEM WARMUP: Waiting 15 seconds to let the background thread map all SX Bet markets...");
    sleep(Duration::from_secs(15)).await;
    arb.debug_print_cache().await;
    info!("🚀 WARMUP COMPLETE: Starting to cross-reference scraped matches against the cache.");

    loop {
        info!("--- Poll cycle ---");
        let matches = monitor.poll_all().await;
        for m in matches {
            if let Err(e) = arb.evaluate_esports_match(&m.home, &m.away, &m.sport, &m.winner).await {
                warn!("Glimpse edge checking failed for {}: {}", m.match_name, e);
            }
        }

        sleep(Duration::from_secs(poll_interval_secs)).await;
    }
}
