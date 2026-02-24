/// RustMiskoLive — Live Odds Observer
///
/// Co dělá:
///   1. Každých 15s polluje LIVE zápasy ze 4 zdrojů (LoL, Valorant, CS2, Dota2)
///   2. Detekuje přechod LIVE → FINISHED (state machine)
///   3. Okamžitě checkuje SX Bet orderbook pro oracle lag arbitráž
///   4. Telegram alert při edge >3%
///
/// Co NEDĚLÁ: žádné ordery (observe_only = true)
///
/// Spuštění:
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

    info!("=== RustMiskoLive Observer — LIVE SCORING ACTIVE ===");
    info!("Mode: OBSERVE ONLY (no trades)");
    info!("Strategy: Live match state machine → SX Bet oracle lag detection");
    info!("Logs: ./logs/");

    // Single instance lock
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

    let poll_interval_secs = env::var("ESPORTS_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15);

    info!("Live poll interval: {}s", poll_interval_secs);

    let monitor = EsportsMonitor::new("logs", poll_interval_secs);
    let arb = ArbDetector::new("logs", true);

    // Spustit STRATZ WebSocket na dotu 2
    monitor.start_stratz_ws().await;

    info!("⏳ WARMUP: Čekám 15s aby SX Bet cache background thread zmapoval trhy...");
    sleep(Duration::from_secs(15)).await;
    arb.debug_print_cache().await;
    info!("🚀 READY: Spouštím live scoring loop.");

    let mut fallback_counter: u32 = 0;

    loop {
        info!("--- Live poll cycle ---");

        // PRIMÁRNÍ: live match tracking → detekuje právě dokončené zápasy
        let live_finished = monitor.poll_live_all().await;
        for m in &live_finished {
            if let Err(e) = arb.evaluate_esports_match(&m.home, &m.away, &m.sport, &m.winner).await {
                warn!("SX Bet eval failed pro {}: {}", m.match_name, e);
            }
        }

        // FALLBACK: results scraping jednou za ~5 minut (audit)
        // Chytá zápasy co mohly proběhnout bez live detekce (restart bota atd.)
        fallback_counter += 1;
        if fallback_counter >= 20 {  // 20 × 15s = 5 minut
            fallback_counter = 0;
            info!("--- Fallback results audit ---");
            let fallback = monitor.poll_all().await;
            for m in fallback {
                if let Err(e) = arb.evaluate_esports_match(&m.home, &m.away, &m.sport, &m.winner).await {
                    warn!("Fallback SX Bet eval failed pro {}: {}", m.match_name, e);
                }
            }
        }

        let current_interval = if monitor.is_any_match_live() {
            3 // 🚀 Sniper mode!
        } else {
            poll_interval_secs // Běžný audit timing
        };

        sleep(Duration::from_secs(current_interval)).await;
    }
}
