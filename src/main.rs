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
use price_monitor::PriceMonitor;
use std::env;
use tokio::time::{sleep, Duration};
use tracing::info;
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

    info!("=== RustMiskoLive Observer — 48h observe mode ===");
    info!("Mode: OBSERVE ONLY (no trades)");
    info!("Logging: Pinnacle lines + odds-api.io arb bets");
    info!("Logs: ./logs/");

    let pinnacle_key = env::var("PINNACLE_KEY").ok();   // volitelné
    let oddsapi_key  = env::var("ODDSAPI_KEY").ok();    // volitelné pro free tier
    let poll_interval_secs = env::var("POLL_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    let min_roi_pct = env::var("MIN_ROI_PCT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1.0);

    if pinnacle_key.is_none() {
        info!("PINNACLE_KEY not set — using unauthenticated access (may be rate limited)");
    }
    if oddsapi_key.is_none() {
        info!("ODDSAPI_KEY not set — odds-api poll will be skipped and logged as status");
    }

    info!("Poll interval: {}s", poll_interval_secs);
    info!("Paper signal min ROI: {:.2}%", min_roi_pct);

    let monitor = PriceMonitor::new(
        "logs",
        pinnacle_key,
        oddsapi_key,
        min_roi_pct,
        poll_interval_secs,
    );

    info!("Starting poll loop ({}s interval)...", poll_interval_secs);

    loop {
        info!("--- Poll cycle ---");
        monitor.poll_all().await;
        sleep(Duration::from_secs(poll_interval_secs)).await;
    }
}
