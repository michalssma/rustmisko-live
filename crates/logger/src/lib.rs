/// RustMiskoLive — Logger
/// JSONL event stream, NTFY alerts

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct EventLogger {
    log_dir: PathBuf,
}

impl EventLogger {
    pub fn new(log_dir: impl Into<PathBuf>) -> Self {
        let dir = log_dir.into();
        fs::create_dir_all(&dir).ok();
        Self { log_dir: dir }
    }

    pub fn log<T: Serialize>(&self, event: &T) -> Result<()> {
        let date  = Utc::now().format("%Y-%m-%d").to_string();
        let path  = self.log_dir.join(format!("{date}.jsonl"));
        let line  = serde_json::to_string(event)?;
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{line}")?;
        Ok(())
    }
}

pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

// ── Event typy ────────────────────────────────────────────────────────────────

#[derive(Serialize, Debug)]
pub struct PinnacleLineEvent {
    pub ts:           String,
    pub event:        &'static str,   // "PINNACLE_LINE"
    pub sport:        String,
    pub home:         String,
    pub away:         String,
    pub home_odds:    f64,            // decimal
    pub away_odds:    f64,
    pub draw_odds:    Option<f64>,
    pub pinnacle_prob_home: f64,      // implied prob bez vigu
    pub pinnacle_prob_away: f64,
}

#[derive(Serialize, Debug)]
pub struct PolymarketPriceEvent {
    pub ts:           String,
    pub event:        &'static str,   // "POLYMARKET_PRICE"
    pub condition_id: String,
    pub question:     String,
    pub yes_price:    f64,
    pub no_price:     f64,
    pub liquidity:    f64,
}

#[derive(Serialize, Debug)]
pub struct ArbOpportunityEvent {
    pub ts:              String,
    pub event:           &'static str,   // "ARB_OPPORTUNITY"
    pub source:          String,         // "pinnacle_vs_polymarket" | "arbitrage_bets_api"
    pub home:            String,
    pub away:            String,
    pub sport:           String,
    pub edge_pct:        f64,
    pub pinnacle_prob:   f64,
    pub polymarket_price: f64,
    pub action:          String,         // "OBSERVE" (48h), pak "BUY"
}

#[derive(Serialize, Debug)]
pub struct MatchResolvedEvent {
    pub ts:          String,
    pub event:       &'static str,    // "MATCH_RESOLVED"
    pub sport:       String,
    pub match_name:  String,
    pub home:        String,
    pub away:        String,
    pub winner:      String,
    pub ended_at:    String,
}

#[derive(Serialize, Debug)]
pub struct ApiStatusEvent {
    pub ts:          String,
    pub event:       &'static str,    // "API_STATUS"
    pub source:      String,           // "pinnacle" | "odds_api"
    pub scope:       String,           // sport/category
    pub ok:          bool,
    pub status_code: Option<u16>,
    pub message:     String,
    pub items_logged: usize,
}

#[derive(Serialize, Debug)]
pub struct SystemHeartbeatEvent {
    pub ts:                 String,
    pub event:              &'static str, // "SYSTEM_HEARTBEAT"
    pub phase:              String,
    pub poll_interval_secs: u64,
    
    // Obsolete classic metrics
    pub pinnacle_items:     usize,
    pub oddsapi_items:      usize,
    pub total_items:        usize,
    
    // New Esports metrics
    pub overall_items:      usize,
    pub healthy_sources:    usize,
    pub total_sources:      usize,
}

/// Pošli čitelný push alert
pub async fn send_ntfy_alert(msg: &str, title: &str) {
    let client = reqwest::Client::new();
    match client
        .post("https://ntfy.sh/rustmisko")
        .header("Title", title)
        .header("Priority", "high")
        .header("Tags", "money_with_wings")
        .body(msg.to_string())
        .send()
        .await
    {
        Ok(_)  => tracing::info!("NTFY sent: {}", title),
        Err(e) => tracing::warn!("NTFY failed: {}", e),
    }
}
