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
pub struct OddsApiArbEvent {
    pub ts:          String,
    pub event:       &'static str,    // "ODDS_API_ARB"
    pub sport:       String,
    pub home:        String,
    pub away:        String,
    pub roi_pct:     f64,
    pub outcome_a:   String,
    pub outcome_a_odds: f64,
    pub bookmaker_a: String,
    pub outcome_b:   String,
    pub outcome_b_odds: f64,
    pub bookmaker_b: String,
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
