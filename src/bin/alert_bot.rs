//! Telegram Alert Bot pro CS2 odds anomálie
//!
//! Standalone binary — polluje feed-hub /opportunities endpoint,
//! detekuje odds discrepancy mezi Azuro a trhem, posílá Telegram alerty.
//! Miša odpoví YES $X / NO a bot umístí sázku přes Azuro executor sidecar.
//! Auto-cashout monitoruje aktivní sázky a cashoutuje při profitu.
//!
//! Spuštění:
//!   $env:TELEGRAM_BOT_TOKEN="7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk"
//!   $env:TELEGRAM_CHAT_ID="6458129071"
//!   $env:FEED_HUB_URL="http://127.0.0.1:8081"
//!   $env:EXECUTOR_URL="http://127.0.0.1:3030"  # Node.js sidecar
//!   cargo run --bin alert_bot

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, HashMap};
use std::time::Duration;
use tracing::{info, warn, error, debug};
use tracing_subscriber::{EnvFilter, fmt};
use std::path::Path;

// ====================================================================
// Config
// ====================================================================

const POLL_INTERVAL_SECS: u64 = 2;  // 2s — near-instant detection of Tipsport score changes!
/// Minimum edge % to trigger alert (all tiers)
const MIN_EDGE_PCT: f64 = 5.0;
/// Don't re-alert same match+score+side within this window
const ALERT_COOLDOWN_SECS: i64 = 45; // faster re-fire for live execution windows
/// Auto-cashout check interval
const CASHOUT_CHECK_SECS: u64 = 30;
/// Minimum profit % to auto-cashout
const CASHOUT_MIN_PROFIT_PCT: f64 = 3.0;
/// Minimum score-edge % to trigger alert
const MIN_SCORE_EDGE_PCT: f64 = 8.0;
/// Score edge cooldown per match (seconds)
const SCORE_EDGE_COOLDOWN_SECS: i64 = 30; // 30s — react fast to new score changes!
/// === AUTO-BET CONFIG ===
const AUTO_BET_ENABLED: bool = true;
/// Base stake per auto-bet in USD
const AUTO_BET_STAKE_USD: f64 = 3.0;
/// Reduced stake for data-collection sports (tennis, basketball) — capped at $1 for small sample gathering
const AUTO_BET_STAKE_LOW_USD: f64 = 1.0;
/// Minimum Azuro odds to auto-bet (skip heavy favorites)
const AUTO_BET_MIN_ODDS: f64 = 1.15;
/// Maximum odds for auto-bet (skip extreme underdogs)
const AUTO_BET_MAX_ODDS: f64 = 2.00;
/// CS2 map_winner exception: allow higher odds (score-based edge is more reliable on maps)
const AUTO_BET_MAX_ODDS_CS2_MAP: f64 = 3.00;
/// Manual/Reaction default stake in USD
const MANUAL_BET_DEFAULT_USD: f64 = 3.0;
/// Manual/Reaction max odds cap (risk guard)
const MANUAL_BET_MAX_ODDS: f64 = 2.00;
/// Manual/Reaction alert must be fresh (prevents betting stale/reset markets)
const MANUAL_ALERT_MAX_AGE_SECS: i64 = 25;
/// Block betting on generic esports keys — DISABLED: Azuro condition_id check
/// already ensures we only bet on real Azuro markets. esports:: keys from
/// Tipsport resolve to cs2:: via alt-key lookup → safe to bet.
const BLOCK_GENERIC_ESPORTS_BETS: bool = false;
/// Retry settings — exponential backoff for live market condition pauses
const AUTO_BET_RETRY_MAX: usize = 3;
/// Exponential backoff delays per retry attempt (ms) — total worst-case = 600ms
const AUTO_BET_RETRY_DELAYS_MS: [u64; 3] = [50, 150, 400];
/// Signal TTL — reject bet if decision is older than this (seconds)
const SIGNAL_TTL_SECS: u64 = 3;
/// Slippage guard factors (minOdds = displayed_odds * factor)
const MIN_ODDS_FACTOR_DEFAULT: f64 = 0.97;
const MIN_ODDS_FACTOR_TENNIS: f64 = 0.97;
/// Prefer auto-bet only when anomaly is confirmed by at least N market sources
const AUTO_BET_MIN_MARKET_SOURCES: usize = 2;
/// Ignore stale odds snapshots older than this threshold
const MAX_ODDS_AGE_SECS: i64 = 20;
/// === RISK MANAGEMENT ===
/// Daily settled-loss limit HARD ceiling — min(this, tier_daily_cap) is effective limit
const DAILY_LOSS_LIMIT_USD: f64 = 30.0;
/// When daily loss cap is hit, resend reminder to Telegram every N seconds
const DAILY_LOSS_REMINDER_SECS: i64 = 900;
/// === AUTO-CLAIM CONFIG ===
const CLAIM_CHECK_SECS: u64 = 60;
/// Portfolio status report interval (seconds) — every 30 min
const PORTFOLIO_REPORT_SECS: u64 = 1800;
/// === WATCHDOG ===
/// Seconds without feed-hub data before entering SAFE MODE
const WATCHDOG_TIMEOUT_SECS: u64 = 120;
/// === CASHOUT — DISABLED (no EV/fair_value calc yet, margin leak risk) ===
const FF_CASHOUT_ENABLED: bool = false;

// ====================================================================
// FEATURE FLAGS — enable upgrades incrementally (Gemini recommendation)
// Order: detailed_score parser → cross-validate → exposure caps → re-bet
// ====================================================================
/// Parse Chance detailed_score for CS2 round-level data
const FF_CHANCE_ROUND_PARSER: bool = true;
/// Cross-validate HLTV vs Chance scores (mismatch → skip + resync freeze)
const FF_CROSS_VALIDATION: bool = true;
/// Dynamic exposure caps (per-bet, per-condition, per-match, inflight)
const FF_EXPOSURE_CAPS: bool = true;
/// Allow re-bet on same condition when edge grows (tier upgrade / edge jump)
const FF_REBET_ENABLED: bool = true;
/// Cross-map momentum bonus (+3% for dominant previous map)
const FF_CROSS_MAP_MOMENTUM: bool = true;
/// Inflight exposure cap (max % of bankroll locked in pending bets)
const FF_INFLIGHT_CAP: bool = true;
/// Per-sport exposure caps (prevent single-model failure from draining bank)
const FF_PER_SPORT_CAP: bool = true;
/// Resync freeze: on cross-validation mismatch, block match 60s, require 2 agreements
const FF_RESYNC_FREEZE: bool = true;

/// Sport-specific auto-bet configuration (v2 — with sport models)
/// Returns: (auto_bet_allowed, min_edge_pct, stake_multiplier, preferred_market)
/// preferred_market: "map_winner" | "match_winner"
fn get_sport_config(sport: &str) -> (bool, f64, f64, &'static str) {
    match sport {
        // Esports: prefer map_winner, but allow match_winner fallback when map market is missing.
        "cs2" | "valorant" | "dota-2" | "league-of-legends" | "lol" | "esports"
            => (true, 12.0, 1.0, "match_or_map"),
        // Tennis: match_winner — our tennis_model uses set+game state
        // Safety: auto-bet only allowed when set_diff >= 1 (checked in sport guard)
        "tennis"
            => (true, 15.0, 1.0, "match_winner"),
        // Basketball: match_winner — point spread model
        "basketball"
            => (true, 12.0, 1.0, "match_winner"),
        // Football: NOW ENABLED with strict guards
        // Our football_model uses minute + goal difference
        // Safety: auto-bet only when goal_diff >= 2 (checked in sport guard)
        "football"
            => (true, 18.0, 1.0, "match_winner"),
        // New sports: alerts enabled, conservative edge thresholds
        "volleyball" | "ice-hockey" | "baseball" | "cricket" | "boxing"
            => (true, 15.0, 1.0, "match_winner"),
        // Unknown sport: alerts only
        _
            => (false, 0.0, 0.0, "none"),
    }
}

fn min_odds_factor_for_match(match_key: &str) -> f64 {
    let sport = match_key.split("::").next().unwrap_or("");
    if sport == "tennis" {
        MIN_ODDS_FACTOR_TENNIS
    } else {
        MIN_ODDS_FACTOR_DEFAULT
    }
}

/// Additional sport-specific safety guard for auto-bet.
/// Returns true if the specific match situation is safe enough for auto-bet.
/// This is checked IN ADDITION to edge/odds thresholds.
fn sport_auto_bet_guard(sport: &str, opp: &Opportunity) -> bool {
    match sport {
        "tennis" => {
            // Tennis: allow all bets (removed set_diff >= 1 guard per user request)
            true
        }
        "football" => {
            // Football: allow all bets (removed goal_diff >= 2 guard per user request)
            true
        }
        // Esports + basketball: no extra guard needed
        _ => true,
    }
}

/// Prematch odds anomaly auto-bet: ENABLED for LIVE as well
const AUTO_BET_ODDS_ANOMALY_ENABLED: bool = true;
const AUTO_BET_ODDS_ANOMALY_STAKE_USD: f64 = 2.0;

// ====================================================================
// EXPOSURE CAPS — Dynamic bankroll-based risk management (GPT/Gemini consensus)
// ====================================================================

/// Per-bet cap as fraction of bankroll (by tier)
/// Per-condition cap (sum of all re-bets on one condition_id)
/// Per-match cap (sum of all markets in one match)
/// Inflight cap (max total pending wagers as % of bankroll)
/// Tiers: micro (<150), small (150-500), medium (500-1500), large (1500+)
fn get_exposure_caps(bankroll: f64) -> (f64, f64, f64, f64, f64) {
    // Returns: (per_bet_frac, per_condition_frac, per_match_frac, daily_loss_frac, inflight_frac)
    if bankroll < 150.0 {
        (0.05, 0.10, 0.15, 0.30, 0.45)  // micro: 5% bet, 10% cond, 15% match, 30% daily, 45% inflight
    } else if bankroll < 500.0 {
        (0.03, 0.08, 0.12, 0.20, 0.40)  // small
    } else if bankroll < 1500.0 {
        (0.02, 0.06, 0.10, 0.15, 0.35)  // medium
    } else {
        (0.015, 0.05, 0.08, 0.10, 0.30) // large
    }
}

/// Per-sport exposure caps (fraction of bankroll) — prevents single model failure from draining bank
/// Returns max total wagered per sport per day as fraction of bankroll
fn get_sport_exposure_cap(sport: &str, bankroll: f64) -> f64 {
    let frac = match sport {
        "cs2" | "esports" | "valorant" | "dota-2" | "league-of-legends" | "lol" => 0.40,
        "football" => 0.25,
        "tennis" | "basketball" => 0.10,
        _ => 0.10, // conservative for new sports
    };
    bankroll * frac
}

/// Calculate dynamic base stake: 0.9 * per_bet_cap (clean, stable sizing)
fn dynamic_base_stake(bankroll: f64, sport: &str) -> f64 {
    let (per_bet_frac, _, _, _, _) = get_exposure_caps(bankroll);
    let base = bankroll * per_bet_frac * 0.9;
    // Data-collection sports: cap at $1
    if sport == "tennis" || sport == "basketball" {
        base.min(AUTO_BET_STAKE_LOW_USD)
    } else {
        base
    }
}

/// Stake Trimmer: min(calculated_stake, per_bet, cond_left, match_left, daily_left, inflight_left, sport_left)
/// cross_val_multiplier: 1.25 if cross-validated, 1.0 neutral — applied to STAKE, not edge threshold
/// Returns the final safe stake, or 0.0 if bet should be skipped
/// When FF_EXPOSURE_CAPS is off, returns calculated_stake unchanged (simple min with daily cap).
fn trim_stake(
    calculated_stake: f64,
    bankroll: f64,
    condition_exposure: f64,  // already wagered on this condition (incl. inflight)
    match_exposure: f64,      // already wagered on this match (incl. inflight)
    daily_net_loss: f64,      // current daily net loss
    inflight_total: f64,      // total USD in all pending bets
    sport_exposure: f64,      // already wagered on this sport today
    sport: &str,              // sport key for per-sport cap
    cross_val_multiplier: f64, // 1.0 or 1.25 — boosted stake for cross-validated bets
    sod_bankroll: f64,        // start-of-day bankroll for daily loss limit (prevents shrinking box)
) -> f64 {
    // Effective daily limit: min(hard_limit, tier-based cap)
    // Uses SOD bankroll so the limit doesn't shrink as you lose bets during the day
    let effective_daily_limit = if !FF_EXPOSURE_CAPS {
        DAILY_LOSS_LIMIT_USD
    } else {
        let (_, _, _, daily_loss_frac, _) = get_exposure_caps(sod_bankroll);
        DAILY_LOSS_LIMIT_USD.min(sod_bankroll * daily_loss_frac)
    };

    if !FF_EXPOSURE_CAPS {
        let base = calculated_stake * cross_val_multiplier;
        return base.min((effective_daily_limit - daily_net_loss).max(0.0));
    }

    let (per_bet_frac, per_cond_frac, per_match_frac, _, inflight_frac) = get_exposure_caps(bankroll);
    let per_bet_cap = bankroll * per_bet_frac;
    let per_cond_cap = bankroll * per_cond_frac;
    let per_match_cap = bankroll * per_match_frac;

    let cond_room = (per_cond_cap - condition_exposure).max(0.0);
    let match_room = (per_match_cap - match_exposure).max(0.0);
    let daily_room = (effective_daily_limit - daily_net_loss).max(0.0);

    // Inflight cap: prevent too much capital locked in pending bets
    let inflight_room = if FF_INFLIGHT_CAP {
        (bankroll * inflight_frac - inflight_total).max(0.0)
    } else {
        f64::MAX
    };

    // Per-sport cap: prevent single model failure from draining bank
    let sport_room = if FF_PER_SPORT_CAP {
        let sport_cap = get_sport_exposure_cap(sport, bankroll);
        (sport_cap - sport_exposure).max(0.0)
    } else {
        f64::MAX
    };

    // Apply cross-validation multiplier to stake (NOT to edge threshold)
    let boosted_stake = calculated_stake * cross_val_multiplier;

    let final_stake = boosted_stake
        .min(per_bet_cap)
        .min(cond_room)
        .min(match_room)
        .min(daily_room)
        .min(inflight_room)
        .min(sport_room);

    // OBSERVABILITY: log trim_stake evaluation for every bet attempt
    if final_stake < boosted_stake * 0.99 || final_stake < 0.50 {
        tracing::debug!(
            "📊 TRIM_STAKE: raw={:.2} boosted={:.2} final={:.2} | caps: bet={:.2} cond={:.2} match={:.2} daily={:.2} inflight={:.2} sport={:.2} | sod_br={:.2} cur_br={:.2} eff_daily_lim={:.2}",
            calculated_stake, boosted_stake, final_stake,
            per_bet_cap, cond_room, match_room, daily_room, inflight_room, sport_room,
            sod_bankroll, bankroll, effective_daily_limit
        );
    }

    if final_stake < 0.50 { 0.0 } else { final_stake }
}

/// Cross-validation result for HLTV vs Chance score comparison.
/// Returns (skip: bool, stake_multiplier: f64)
///   skip=true → HARD SKIP (mismatch = invalid input)
///   stake_multiplier: 1.25 (agree) or 1.0 (single source or non-CS2)
/// IMPORTANT: multiplier is for STAKE/PRIORITY only, NOT for edge threshold!
fn cross_validation_check(
    hltv_score: Option<(i32, i32)>,
    chance_score: Option<(i32, i32)>,
) -> (bool, f64) {
    match (hltv_score, chance_score) {
        (Some(h), Some(c)) => {
            if h.0 == c.0 && h.1 == c.1 {
                (false, 1.25)  // Both agree → higher stake/priority
            } else {
                (true, 0.0)    // Mismatch → HARD SKIP (stale data risk)
            }
        }
        _ => (false, 1.0),  // Only one source → neutral
    }
}

// ====================================================================
// RESYNC FREEZE — after cross-validation mismatch, block match for 60s
// and require 2 consecutive agreements before re-enabling
// ====================================================================

#[derive(Debug, Clone)]
struct ResyncState {
    /// When the mismatch was first detected
    frozen_at: chrono::DateTime<Utc>,
    /// Number of consecutive agreements since freeze
    consecutive_agreements: u32,
}

impl ResyncState {
    fn new() -> Self {
        Self {
            frozen_at: Utc::now(),
            consecutive_agreements: 0,
        }
    }

    /// Check if match is still frozen (needs 60s + 2 consecutive agreements)
    fn is_frozen(&self) -> bool {
        let elapsed = (Utc::now() - self.frozen_at).num_seconds();
        elapsed < 60 || self.consecutive_agreements < 2
    }

    /// Record an agreement; returns true if resync complete (unfrozen)
    fn record_agreement(&mut self) -> bool {
        self.consecutive_agreements += 1;
        let elapsed = (Utc::now() - self.frozen_at).num_seconds();
        elapsed >= 60 && self.consecutive_agreements >= 2
    }

    /// Reset on new mismatch
    fn record_mismatch(&mut self) {
        self.frozen_at = Utc::now();
        self.consecutive_agreements = 0;
    }
}

// ====================================================================
// RE-BET TRACKING — allow multiple bets on same condition as edge grows
// ====================================================================

#[derive(Debug, Clone)]
struct ReBetState {
    /// Number of bets placed on this condition
    bet_count: u32,
    /// Highest confidence tier reached
    highest_tier: String,
    /// Last edge percentage when we bet
    last_edge_pct: f64,
    /// Last bet timestamp
    last_bet_at: chrono::DateTime<Utc>,
    /// Total USD wagered on this condition
    total_wagered: f64,
}

impl ReBetState {
    fn new(tier: &str, edge_pct: f64, stake: f64) -> Self {
        Self {
            bet_count: 1,
            highest_tier: tier.to_string(),
            last_edge_pct: edge_pct,
            last_bet_at: Utc::now(),
            total_wagered: stake,
        }
    }
}

/// Check if re-bet is allowed on this condition
/// Returns true if: tier improved OR edge jumped ≥8%, cooldown ≥30s, count < 3,
/// AND new edge_raw (after slippage) > last edge (not just "paper" edge)
fn rebet_allowed(state: &ReBetState, new_tier: &str, new_edge_raw: f64, cond_cap_left: f64, match_cap_left: f64) -> bool {
    let tier_value = |t: &str| -> u8 {
        match t {
            "ULTRA" => 4,
            "HIGH" => 3,
            "MEDIUM" => 2,
            "LOW" => 1,
            _ => 0,
        }
    };
    let elapsed = (Utc::now() - state.last_bet_at).num_seconds();
    let tier_improved = tier_value(new_tier) > tier_value(&state.highest_tier);
    let edge_jumped = new_edge_raw - state.last_edge_pct >= 8.0;
    // Re-bet must have higher raw edge than last time (no "paper" inflation)
    let edge_actually_higher = new_edge_raw > state.last_edge_pct;
    // Re-bet must not exceed remaining condition/match caps
    let caps_ok = cond_cap_left >= 0.50 && match_cap_left >= 0.50;

    state.bet_count < 3
        && elapsed >= 30
        && edge_actually_higher
        && (tier_improved || edge_jumped)
        && caps_ok
}

// ====================================================================
// Types matching feed-hub /opportunities JSON
// ====================================================================

#[derive(Debug, Clone, Deserialize)]
struct OpportunitiesResponse {
    ts: String,
    total_live: usize,
    total_odds: usize,
    fused_matches: usize,
    opportunities: Vec<Opportunity>,
}

#[derive(Debug, Clone, Deserialize)]
struct Opportunity {
    match_key: String,
    opp_type: String,
    team1: String,
    team2: String,
    score: String,
    detailed_score: Option<String>,
    value_side: u8,
    signal: String,
    confidence: f64,
    odds: f64,
    implied_prob_pct: f64,
    estimated_fair_pct: f64,
    edge_pct: f64,
    bookmaker: String,
    odds_age_secs: i64,
    live_age_secs: i64,
}

// Feed-hub /state types (for cross-bookmaker comparison)
#[derive(Debug, Clone, Deserialize)]
struct StateResponse {
    ts: String,
    connections: usize,
    live_items: usize,
    odds_items: usize,
    fused_ready: usize,
    odds: Vec<StateOddsItem>,
    #[serde(default)]
    live: Vec<LiveItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct LiveItem {
    match_key: String,
    #[allow(dead_code)]
    source: String,
    payload: LivePayload,
}

#[derive(Debug, Clone, Deserialize)]
struct LivePayload {
    team1: String,
    team2: String,
    score1: i32,
    score2: i32,
    status: String,
    #[serde(default)]
    detailed_score: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct StateOddsItem {
    match_key: String,
    source: String,
    seen_at: String,
    payload: OddsPayload,
}

#[derive(Debug, Clone, Deserialize)]
struct OddsPayload {
    sport: Option<String>,
    bookmaker: String,
    market: Option<String>,
    team1: String,
    team2: String,
    odds_team1: f64,
    odds_team2: f64,
    liquidity_usd: Option<f64>,
    spread_pct: Option<f64>,
    url: Option<String>,
    // Azuro execution data
    game_id: Option<String>,
    condition_id: Option<String>,
    outcome1_id: Option<String>,
    outcome2_id: Option<String>,
    chain: Option<String>,
}

/// Map winner odds from Azuro (map1_winner, map2_winner, map3_winner)
#[derive(Debug, Clone)]
struct MapWinnerOdds {
    market: String,
    team1: String,
    team2: String,
    odds_team1: f64,
    odds_team2: f64,
    seen_at: String,
    condition_id: Option<String>,
    outcome1_id: Option<String>,
    outcome2_id: Option<String>,
    bookmaker: String,
    chain: Option<String>,
    url: Option<String>,
}

// Telegram getUpdates response
#[derive(Debug, Deserialize)]
struct TgUpdatesResponse {
    ok: bool,
    result: Vec<TgUpdate>,
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
    message_reaction: Option<TgMessageReaction>,
}

#[derive(Debug, Deserialize)]
struct TgMessageReaction {
    chat: TgChat,
    message_id: i64,
    date: i64,
    new_reaction: Vec<TgReactionType>,
}

#[derive(Debug, Deserialize)]
struct TgReactionType {
    #[serde(rename = "type")]
    reaction_type: String,
    emoji: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    message_id: i64,
    chat: TgChat,
    text: Option<String>,
    reply_to_message: Option<Box<TgMessage>>,
    date: i64,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
}

// Tracked alert (cooldown)
struct SentAlert {
    match_key: String,
    sent_at: chrono::DateTime<Utc>,
}

// ====================================================================
// Telegram helpers
// ====================================================================

async fn tg_send_message(client: &reqwest::Client, token: &str, chat_id: i64, text: &str) -> Result<i64> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("Telegram sendMessage failed: {} — {}", status, body);
        anyhow::bail!("Telegram sendMessage failed: {} — {}", status, body);
    }
    let resp_json: serde_json::Value = resp.json().await?;
    let msg_id = resp_json["result"]["message_id"].as_i64().unwrap_or(0);
    Ok(msg_id)
}

async fn tg_get_updates(client: &reqwest::Client, token: &str, offset: i64) -> Result<TgUpdatesResponse> {
    let url = format!(
        "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout=5&allowed_updates=[\"message\",\"message_reaction\"]",
        token, offset
    );
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("getUpdates HTTP {}: {}", status, body);
    }
    let parsed: TgUpdatesResponse = serde_json::from_str(&body)
        .with_context(|| format!("Failed to parse getUpdates: {}", &body[..body.len().min(200)]))?;
    Ok(parsed)
}

async fn tg_get_me(client: &reqwest::Client, token: &str) -> Result<i64> {
    let url = format!("https://api.telegram.org/bot{}/getMe", token);
    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;
    let bot_id = resp["result"]["id"].as_i64().unwrap_or(0);
    Ok(bot_id)
}

// ====================================================================
// Score Edge — HLTV score vs stale Azuro odds
// ====================================================================

/// Track previous scores per match for score-change detection
struct ScoreTracker {
    /// match_key → (score1, score2, timestamp) — last known scores
    prev_scores: HashMap<String, (i32, i32, chrono::DateTime<Utc>)>,
    /// match_key → timestamp when we last alerted score edge
    edge_cooldown: HashMap<String, chrono::DateTime<Utc>>,
}

impl ScoreTracker {
    fn new() -> Self {
        Self {
            prev_scores: HashMap::new(),
            edge_cooldown: HashMap::new(),
        }
    }

    /// Clean entries older than 30 min (match ended)
    fn cleanup(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::seconds(1800);
        self.prev_scores.retain(|_, (_, _, ts)| *ts > cutoff);
        self.edge_cooldown.retain(|_, ts| *ts > cutoff);
    }
}

/// Score edge alert — Azuro odds haven't adjusted to live score
struct ScoreEdge {
    match_key: String,
    team1: String,
    team2: String,
    /// Current live score
    score1: i32,
    score2: i32,
    /// Previous score (before change)
    prev_score1: i32,
    prev_score2: i32,
    /// Which team is winning based on score: 1 or 2
    leading_side: u8,
    /// Current Azuro odds (possibly stale)
    azuro_w1: f64,
    azuro_w2: f64,
    azuro_bookmaker: String,
    /// Implied probability from Azuro for the leading team
    azuro_implied_pct: f64,
    /// Score-implied probability for the leading team
    score_implied_pct: f64,
    /// Edge: score_implied - azuro_implied
    edge_pct: f64,
    /// "HIGH" / "MEDIUM"
    confidence: &'static str,
    /// Azuro execution data
    game_id: Option<String>,
    condition_id: Option<String>,
    outcome1_id: Option<String>,
    outcome2_id: Option<String>,
    outcome_id: Option<String>,
    chain: Option<String>,
    azuro_url: Option<String>,
    /// CS2 map confidence tier for dynamic odds cap ("ULTRA"/"HIGH"/"MEDIUM"/"LOW"/None)
    cs2_map_confidence: Option<&'static str>,
    /// Cross-validation stake multiplier (1.0 = normal, 1.25 = boosted)
    cv_stake_mult: f64,
}

// ====================================================================
// CS2 ROUND SCORE PARSER — extract current round from Chance detailed_score
// ====================================================================

/// Parse CS2 round score from Chance.cz detailed_score string.
/// Examples:
///   "Lepší ze 3 | 3.mapa - 13:6, 9:13, 7:12" → Some((7, 12)) — current map round score
///   "Lepší ze 3 | 2.mapa - 13:6, 4:8"         → Some((4, 8))
///   "Lepší ze 3 | 1.mapa - 5:3"                → Some((5, 3))
/// Returns the LAST score in the comma-separated list (= current map being played).
fn parse_cs2_round_score(detailed: &str) -> Option<(i32, i32)> {
    // Pattern: contains "mapa" and has scores like "X:Y"
    if !detailed.to_lowercase().contains("mapa") {
        return None;
    }

    // Find all X:Y patterns in the string
    let re_scores: Vec<(i32, i32)> = detailed
        .split(|c: char| c == ',' || c == '|' || c == '-')
        .filter_map(|segment| {
            let trimmed = segment.trim();
            // Match pure "X:Y" where X and Y are 0-30 (CS2 round range)
            let parts: Vec<&str> = trimmed.split(':').collect();
            if parts.len() == 2 {
                if let (Ok(a), Ok(b)) = (parts[0].trim().parse::<i32>(), parts[1].trim().parse::<i32>()) {
                    if a >= 0 && a <= 30 && b >= 0 && b <= 30 {
                        return Some((a, b));
                    }
                }
            }
            None
        })
        .collect();

    // Last score = current map being played
    re_scores.last().copied()
}

/// Parse the current map number from detailed_score.
/// "Lepší ze 3 | 3.mapa - 13:6, 9:13, 7:12" → Some(3)
fn parse_cs2_current_map(detailed: &str) -> Option<u8> {
    // Look for "N.mapa" pattern
    for segment in detailed.split('|') {
        let trimmed = segment.trim().to_lowercase();
        if trimmed.contains("mapa") {
            // Extract digit before ".mapa"
            for ch in trimmed.chars() {
                if ch.is_ascii_digit() {
                    return Some(ch as u8 - b'0');
                }
            }
        }
    }
    None
}

/// Parse all completed map scores from detailed_score.
/// "Lepší ze 3 | 3.mapa - 13:6, 9:13, 7:12" → [(13,6), (9,13)] (completed maps only, not current)
fn parse_cs2_completed_maps(detailed: &str) -> Vec<(i32, i32)> {
    let all_scores: Vec<(i32, i32)> = detailed
        .split(|c: char| c == ',' || c == '|' || c == '-')
        .filter_map(|segment| {
            let trimmed = segment.trim();
            let parts: Vec<&str> = trimmed.split(':').collect();
            if parts.len() == 2 {
                if let (Ok(a), Ok(b)) = (parts[0].trim().parse::<i32>(), parts[1].trim().parse::<i32>()) {
                    if a >= 0 && a <= 30 && b >= 0 && b <= 30 {
                        return Some((a, b));
                    }
                }
            }
            None
        })
        .collect();

    // All scores except the last one (which is the current map in progress)
    if all_scores.len() > 1 {
        all_scores[..all_scores.len() - 1].to_vec()
    } else {
        Vec::new()
    }
}

/// Cross-map momentum: check if team1 dominated previous map(s).
/// Returns bonus probability (e.g. +0.03 = 3%) if dominant, 0.0 otherwise.
/// Rule (Gemini consensus): only apply if map1 winner won by ≥5 rounds diff.
fn cross_map_momentum_bonus(completed_maps: &[(i32, i32)], leading_side: u8) -> f64 {
    if completed_maps.is_empty() { return 0.0; }

    // Check the LAST completed map
    let (s1, s2) = completed_maps[completed_maps.len() - 1];
    let diff = (s1 - s2).abs();

    // Only apply if dominant win (5+ round diff) and winner matches leading side
    if diff >= 5 {
        let map_winner_side: u8 = if s1 > s2 { 1 } else { 2 };
        if map_winner_side == leading_side {
            return 0.03; // +3% momentum bonus
        }
    }

    0.0
}

/// CS2 score → estimated win probability for the LEADING team
/// Detects whether scores are round-level (0-13) or map-level (0-2)
/// and returns expected match win probability.
///
/// Round scores (max > 3): within a single map
///   - Leading by 3+ rounds → team controlling the map
///   - Leading by 6+ → map almost decided
///   - Leading by 8+ → map virtually won
/// Strip ::mapN_winner suffix from a match key to get the base match key.
/// E.g. "cs2::team_a_vs_team_b::map1_winner" → "cs2::team_a_vs_team_b"
/// Used for dedup: only ONE map-winner bet per base match.
fn strip_map_winner_suffix(key: &str) -> String {
    // Pattern: key ends with ::map<digit>_winner
    if let Some(pos) = key.rfind("::map") {
        if key[pos..].contains("_winner") {
            return key[..pos].to_string();
        }
    }
    key.to_string()
}

fn normalize_team_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
}

/// Check if a single team name loosely matches another (substring or equality after normalization)
fn team_name_matches_single(live_name: &str, azuro_name: &str) -> bool {
    let a = normalize_team_name(live_name);
    let b = normalize_team_name(azuro_name);
    if a.is_empty() || b.is_empty() { return false; }
    if a == b || a.contains(&b) || b.contains(&a) { return true; }
    // Word-set match: handles first/last name reversal (tennis, individual sports)
    // "Masarova Rebeka" vs "Rebeka Masarova" → both normalize to same set of words
    let la = live_name.to_lowercase();
    let lb = azuro_name.to_lowercase();
    let mut wa: Vec<&str> = la.split_whitespace().collect();
    let mut wb: Vec<&str> = lb.split_whitespace().collect();
    if wa.len() >= 2 && wa.len() == wb.len() {
        wa.sort();
        wb.sort();
        if wa == wb { return true; }
    }
    false
}

/// Given the leading team name from live data, determine which Azuro side (1 or 2) matches.
/// Returns Some(1) or Some(2) if unambiguously matched, None if ambiguous or no match.
/// NEVER falls back to positional — if we can't identify the team, we BLOCK the bet.
fn resolve_azuro_side(leading_team: &str, azuro_team1: &str, azuro_team2: &str, _positional_side: u8) -> Option<u8> {
    let m1 = team_name_matches_single(leading_team, azuro_team1);
    let m2 = team_name_matches_single(leading_team, azuro_team2);
    if m1 && !m2 { return Some(1); }
    if m2 && !m1 { return Some(2); }
    // Also try matching the OTHER live team against Azuro teams for cross-validation
    // (caller should use resolve_azuro_side_pair for that)
    // Both match or neither: AMBIGUOUS → return None (block bet)
    None
}

/// Full pair resolution: try leading team first, then losing team for cross-validation.
/// Returns Some(azuro_side_of_leading_team) or None if ambiguous.
fn resolve_azuro_side_pair(
    live_team1: &str, live_team2: &str, leading_side: u8,
    azuro_team1: &str, azuro_team2: &str,
) -> Option<u8> {
    let leading_team = if leading_side == 1 { live_team1 } else { live_team2 };
    let losing_team = if leading_side == 1 { live_team2 } else { live_team1 };
    // Try leading team first
    if let Some(s) = resolve_azuro_side(leading_team, azuro_team1, azuro_team2, leading_side) {
        return Some(s);
    }
    // Fallback: match the LOSING team and invert
    if let Some(s) = resolve_azuro_side(losing_team, azuro_team1, azuro_team2, if leading_side == 1 { 2 } else { 1 }) {
        // losing team matched side s → leading team is the OTHER side
        return Some(if s == 1 { 2 } else { 1 });
    }
    None
}

fn teams_match_loose(a1: &str, a2: &str, b1: &str, b2: &str) -> bool {
    let a1n = normalize_team_name(a1);
    let a2n = normalize_team_name(a2);
    let b1n = normalize_team_name(b1);
    let b2n = normalize_team_name(b2);

    let direct = (a1n == b1n && a2n == b2n) || (a1n == b2n && a2n == b1n);
    if direct {
        return true;
    }

    let overlap = |x: &str, y: &str| -> bool {
        !x.is_empty() && !y.is_empty() && (x.contains(y) || y.contains(x))
    };

    if (overlap(&a1n, &b1n) && overlap(&a2n, &b2n)) || (overlap(&a1n, &b2n) && overlap(&a2n, &b1n)) {
        return true;
    }

    // Word-set match for name reversals (tennis/individual sports)
    let word_match = |x: &str, y: &str| -> bool {
        let mut wx: Vec<String> = x.to_lowercase().split_whitespace().map(|s| s.to_string()).collect();
        let mut wy: Vec<String> = y.to_lowercase().split_whitespace().map(|s| s.to_string()).collect();
        wx.len() >= 2 && wx.len() == wy.len() && { wx.sort(); wy.sort(); wx == wy }
    };

    (word_match(a1, b1) && word_match(a2, b2)) || (word_match(a1, b2) && word_match(a2, b1))
}

fn is_recent_seen_at(seen_at: &str, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(seen_at) {
        Ok(dt) => {
            let age = (now - dt.with_timezone(&Utc)).num_seconds();
            age >= 0 && age <= MAX_ODDS_AGE_SECS
        }
        Err(_) => true,
    }
}

///
/// Map scores (max <= 3): Bo3 map count
///   - 1-0 → ~68% match win
///   - 2-0 → match won (don't bet)
/// CS2 map win probability based on round differential AND total rounds played.
/// Uses empirical data from CS2 pro matches (MR12 format, first to 13).
///
/// Key insight: same diff at different stages means very different things:
///   5-0 (total=5, early) → 65-70% (half-switch at r13 can flip momentum)
///   9-4 (total=13, at half) → 80% (past half, momentum confirmed)
///   12-7 (total=19, late) → 95% (1 round away from win)
///
/// Half-switch at round 13: CT→T or T→CT changes dynamics significantly.
fn cs2_map_win_prob(diff: i32, total_rounds: i32) -> f64 {
    if diff <= 0 { return 0.50; }
    match (diff, total_rounds) {
        // EARLY GAME (total ≤ 8): high variance, half-switch coming
        // Even big diffs can reverse after side switch
        (d, t) if t <= 8 => match d {
            1..=2 => 0.55,
            3..=4 => 0.62,
            5..=6 => 0.68,
            _ => 0.75,   // 7-0 or 7-1 early = strong but volatile
        },
        // MID-EARLY (total 9-12): approaching half, some info but switch coming
        (d, t) if t <= 12 => match d {
            1..=2 => 0.57,
            3..=4 => 0.67,
            5..=6 => 0.76,
            7..=8 => 0.85,
            _ => 0.90,
        },
        // AT/PAST HALF (total 13-18): half-switch done, momentum visible
        (d, t) if t <= 18 => match d {
            1..=2 => 0.60,
            3..=4 => 0.73,
            5..=6 => 0.84,
            7..=8 => 0.92,
            _ => 0.96,
        },
        // LATE GAME (total 19+): very few rounds left, approaching 13
        (d, _) => match d {
            1..=2 => 0.65,
            3..=4 => 0.82,
            5..=6 => 0.93,
            _ => 0.97,
        },
    }
}

/// Confidence tier for dynamic odds cap:
///   "ULTRA"  → prob ≥ 90% AND late game → max odds 5.00
///   "HIGH"   → prob ≥ 80% AND mid+ game → max odds 3.00
///   "MEDIUM" → prob ≥ 70%              → max odds 2.00
///   "LOW"    → prob < 70%              → max odds 1.60
fn cs2_confidence_tier(map_win_prob: f64, total_rounds: i32) -> &'static str {
    if map_win_prob >= 0.90 && total_rounds >= 16 { "ULTRA" }
    else if map_win_prob >= 0.80 && total_rounds >= 13 { "HIGH" }
    else if map_win_prob >= 0.70 { "MEDIUM" }
    else { "LOW" }
}

/// Dynamic max odds for CS2 map_winner based on confidence tier
fn cs2_dynamic_max_odds(tier: &str) -> f64 {
    match tier {
        "ULTRA" => 5.00,
        "HIGH" => 3.00,
        "MEDIUM" => 2.00,
        "LOW" => 1.60,
        _ => 2.00,
    }
}

/// Sanitize tokenId from executor — reject bogus values < 1000
/// (false positives from recursive extraction hitting boolean/index fields)
fn sanitize_token_id(token_id: Option<String>) -> Option<String> {
    token_id.and_then(|tid| {
        if let Ok(num) = tid.parse::<u64>() {
            if num < 1000 {
                warn!("⚠️ Rejecting bogus tokenId {} from executor (< 1000)", tid);
                None
            } else {
                Some(tid)
            }
        } else {
            Some(tid) // non-numeric tokenId, keep as-is
        }
    })
}

fn score_to_win_prob(leading_score: i32, losing_score: i32) -> Option<f64> {
    let diff = leading_score - losing_score;
    if diff <= 0 { return None; }

    let max_score = leading_score.max(losing_score);

    if max_score > 3 {
        // ROUND scores within a map (CS2 MR12: first to 13)
        // BUG FIX: Round-level leads predict MAP wins, NOT MATCH wins.
        // The old `* 0.55` conversion was fabricated and empirically wrong:
        //   - fluxo_vs_oddik: 3 match_winner bets from round leads → ALL LOST
        //   - liquid_vs_lyon: match_winner from round lead → LOST
        // Round edges should ONLY generate map_winner bets (via find_score_edges
        // STEP 1). If no map_winner market exists, we skip entirely.
        // Returning None here ensures match_winner fallback never triggers
        // from round-level scores.
        return None;
    } else {
        // MAP scores (Bo3/Bo5 format)
        // IMPORTANT: (1, 0) is REMOVED — in CS2/LoL Bo3, winning map 1
        // means you won YOUR MAP PICK (expected). Opponent plays their pick
        // on map 2 → effectively ~55% not 68%. Azuro odds at 1.90-2.00 are
        // CORRECT, not stale. Real edge is only 5%, not 17%.
        // Only map_winner with ROUND-level edges (diff >= 3) are profitable.
        match (leading_score, losing_score) {
            (2, 0) => None,        // Already won → too late
            (2, 1) => None,        // Already won
            _ => None,             // (1,0) = won 1 map pick → no reliable edge
        }
    }
}

/// Tennis set score → estimated match win probability for the LEADING player
///
/// Tennis is Bo3 sets (Grand Slams Bo5, but Azuro mainly has Bo3).
/// SET lead is the strongest predictor:
///   - 1-0 in sets → ~65% (won first set but opponent can come back)
///   - 2-0 → match won (don't bet)
///   - Within a set: game lead matters less because service breaks/holds
///     are volatile — we only bet on SET leads for safety.
///
/// `leading_score` and `losing_score` represent SET counts.
fn tennis_score_to_win_prob(leading_sets: i32, losing_sets: i32) -> Option<f64> {
    if leading_sets <= losing_sets { return None; }

    match (leading_sets, losing_sets) {
        (1, 0) => Some(0.65),  // Won first set → ~65% match win
        (2, 0) => None,        // Already won → too late
        (2, 1) => None,        // Already won
        _ => None,
    }
}

/// Football goal score → estimated match win probability for the LEADING team.
///
/// Conservative estimates (we DON'T know how much time is left —
/// FlashScore sends goals, not minutes). The earlier the goal, the less
/// certain the outcome, so we stay conservative:
///   - 1-0 → ~62% (could easily equalize)
///   - 2-0 → ~85% (dominant but not impossible to come back)
///   - 3-0 → ~96% (almost certain)
///   - 2-1 → ~68%
///   - 3-1 → ~90%
fn football_score_to_win_prob(leading: i32, losing: i32) -> Option<f64> {
    if leading <= losing { return None; }
    let diff = leading - losing;
    let total = leading + losing;

    // Only bet when there's clear goal advantage
    if diff < 1 { return None; }

    // If either team has 4+ goals we're likely late in game → stronger signal
    match diff {
        1 => {
            // Single goal lead
            if total >= 3 {
                // Late-scoring game (e.g. 2-1, 3-2) → ~68%
                Some(0.68)
            } else {
                // Early single goal (1-0) → conservative 62%
                Some(0.62)
            }
        }
        2 => {
            // 2 goal lead (2-0, 3-1, 4-2)
            if total >= 4 {
                Some(0.90) // 3-1 or 4-2 → very strong
            } else {
                Some(0.85) // 2-0 → strong but early
            }
        }
        _ => Some(0.96), // 3+ goal lead → near-certain
    }
}

/// Dota-2 kill score → estimated win probability.
/// Kill leads in Dota-2 correlate with gold/XP advantage.
/// Requires significant lead to be actionable (kills swing fast).
///   - 5-9 kill lead: ~60-65%
///   - 10-14: ~72%
///   - 15+: ~82%
fn dota2_score_to_win_prob(leading: i32, losing: i32) -> Option<f64> {
    if leading <= losing { return None; }
    let diff = leading - losing;
    let total = leading + losing;

    // Need substantial kill lead AND enough kills total (early game is volatile)
    if diff < 3 || total < 5 { return None; }

    match diff {
        3..=4 => Some(0.58),
        5..=9 => Some(0.62),
        10..=14 => Some(0.72),
        _ => Some(0.82), // 15+ kills ahead
    }
}

/// Basketball / e-Basketball point lead → estimated win probability.
/// Without quarter/time info, we use total points as proxy for game stage.
///   total < 30:  very early (1st quarter) → point lead less reliable
///   total 30-80: mid-game
///   total 80+:   late game → leads are MUCH more valuable
///
/// Point lead thresholds (conservative — no time info):
///   10+ pts early: ~65%   10+ pts late: ~83%
///   15+ pts early: ~75%   15+ pts late: ~90%
///   20+ pts early: ~82%   20+ pts late: ~93%
fn basketball_score_to_win_prob(leading: i32, losing: i32) -> Option<f64> {
    if leading <= losing { return None; }
    let diff = leading - losing;
    let total = leading + losing;

    // Need at least some game played
    if total < 10 { return None; }

    // Early game (< 30 total) — leads are volatile
    if total < 30 {
        return match diff {
            1..=2  => None,            // Too close early
            3..=4  => Some(0.55),
            5..=9  => Some(0.60),
            10..=14 => Some(0.67),
            _ => Some(0.75),           // 15+ early
        };
    }

    // Mid game (30-80 total)
    if total < 80 {
        return match diff {
            1..=2  => None,            // Small lead, high variance
            3..=4  => Some(0.58),
            5..=9  => Some(0.62),
            10..=14 => Some(0.72),
            15..=19 => Some(0.80),
            _ => Some(0.87),           // 20+ mid
        };
    }

    // Late game (80+ total) — leads are decisive
    match diff {
        1..=2  => None,
        3..=4  => Some(0.62),
        5..=9  => Some(0.68),
        10..=14 => Some(0.80),
        15..=19 => Some(0.88),
        _ => Some(0.93),               // 20+ late game
    }
}

/// MMA round score → estimated win probability.
/// Azuro typically has MMA as match_winner with round scores.
/// Format: rounds won (Bo3 — first to 2 rounds)
///   1-0 → fighter A won round 1 → ~70% match win
///   2-0 → match over (skip — too late)
///   2-1 → match over
fn mma_score_to_win_prob(leading: i32, losing: i32) -> Option<f64> {
    if leading <= losing { return None; }
    match (leading, losing) {
        (1, 0) => Some(0.70), // Won 1 round in a Bo3 → ~70%
        _      => None,       // Match over or invalid
    }
}

/// Detect score-based edges: HLTV live score says one team leads,
/// but Azuro odds haven't adjusted yet → BET on the leading team!
fn find_score_edges(
    state: &StateResponse,
    tracker: &mut ScoreTracker,
    resync_freeze: &mut HashMap<String, ResyncState>,
) -> Vec<ScoreEdge> {
    let now = Utc::now();
    let mut edges = Vec::new();

    // Build live score map
    let live_map: HashMap<&str, &LiveItem> = state.live.iter()
        .map(|l| (l.match_key.as_str(), l))
        .collect();

    // Build Azuro odds map (only azuro_ bookmakers, match_winner)
    let mut azuro_by_match: HashMap<&str, &StateOddsItem> = HashMap::new();
    // Build map winner odds map: match_key → Vec<MapWinnerOdds>
    let mut map_winners_by_match: HashMap<&str, Vec<MapWinnerOdds>> = HashMap::new();
    for item in &state.odds {
        if !item.payload.bookmaker.starts_with("azuro_") {
            continue;
        }
        let market = item.payload.market.as_deref().unwrap_or("match_winner");
        if market == "match_winner" {
            azuro_by_match.entry(item.match_key.as_str())
                .or_insert(item);
        } else if market.starts_with("map") && market.ends_with("_winner") {
            // map1_winner, map2_winner, map3_winner
            map_winners_by_match.entry(item.match_key.as_str())
                .or_default()
                .push(MapWinnerOdds {
                    market: market.to_string(),
                    team1: item.payload.team1.clone(),
                    team2: item.payload.team2.clone(),
                    odds_team1: item.payload.odds_team1,
                    odds_team2: item.payload.odds_team2,
                    seen_at: item.seen_at.clone(),
                    condition_id: item.payload.condition_id.clone(),
                    outcome1_id: item.payload.outcome1_id.clone(),
                    outcome2_id: item.payload.outcome2_id.clone(),
                    bookmaker: item.payload.bookmaker.clone(),
                    chain: item.payload.chain.clone(),
                    url: item.payload.url.clone(),
                });
        }
    }

    for (match_key, live) in &live_map {
        let s1 = live.payload.score1;
        let s2 = live.payload.score2;

        // Check if score changed from previous poll
        let prev = tracker.prev_scores.get(*match_key).cloned();
        tracker.prev_scores.insert(match_key.to_string(), (s1, s2, now));

        let is_first_sight = prev.is_none();
        let (prev_s1, prev_s2) = match prev {
            Some((ps1, ps2, _)) => (ps1, ps2),
            None => (s1, s2), // First time: use current score as "previous" for edge calc
        };

        let score_changed = s1 != prev_s1 || s2 != prev_s2;
        // Guard against score-mode switches / parser glitches:
        // examples: 19-17 -> 1-0, 1-2 -> 0-0. These are often round->map or source resets.
        let backward_score_jump = score_changed
            && s1 <= prev_s1
            && s2 <= prev_s2
            && (s1 < prev_s1 || s2 < prev_s2);
        if backward_score_jump {
            info!(
                "  ⏭️ {} score jump backward {}-{} -> {}-{} (source/reset), skipping edge eval",
                match_key, prev_s1, prev_s2, s1, s2
            );
            tracker.edge_cooldown.insert(match_key.to_string(), now);
            continue;
        }

        // On first sight with an existing lead, treat as "startup edge" — don't skip!
        // This lets us catch edges when bot starts mid-game.
        let is_startup_edge = is_first_sight && s1 != s2;

        if !score_changed && !is_startup_edge {
            continue; // No change and not startup → skip
        }

        if score_changed {
            info!("🔥 SCORE CHANGE: {} → {}-{} (was {}-{})", match_key, s1, s2, prev_s1, prev_s2);
        } else if is_startup_edge {
            info!("🆕 STARTUP EDGE SCAN: {} at {}-{}", match_key, s1, s2);
        }

        // Cooldown: only for startup edges (repeated eval of same score state).
        // If score ACTUALLY CHANGED → always react instantly — that's our edge!
        if !score_changed {
            if let Some(last_alert) = tracker.edge_cooldown.get(*match_key) {
                if (now - *last_alert).num_seconds() < SCORE_EDGE_COOLDOWN_SECS {
                    continue;
                }
            }
        }

        // Determine which team is leading
        if s1 == s2 {
            continue; // Tied → no directional edge
        }

        // === SPORT-AWARE SCORE SANITY CHECK ===
        // Catches garbage scores from FlashScore DOM concatenation (e.g. 714-0, 19-45 labeled as football)
        let sport_prefix = match_key.split("::").next().unwrap_or("unknown");
        let max_score_for_sport: i32 = match sport_prefix {
            "football" => 8,       // max realistic football score per team (tightened from 15)
            "tennis" => 7,         // max sets in a match
            "hockey" => 10,        // max realistic hockey score (tightened from 15 — garbage scraper scores were 12+)
            "basketball" => 200,   // max realistic basketball score per team
            "cs2" => 40,           // round scores (30 + OT rounds)
            "dota-2" => 100,       // kill scores
            "mma" | "boxing" => 5, // round scores
            "handball" => 45,      // max realistic handball score (tightened from 50)
            "volleyball" => 5,     // set scores
            "esports" => 50,       // generic esports limit
            _ => 999,
        };
        if s1 > max_score_for_sport || s2 > max_score_for_sport {
            info!("  ⏭️ {} {}-{}: {} score sanity FAIL (max={}), skipping",
                match_key, s1, s2, sport_prefix, max_score_for_sport);
            continue;
        }

        let (leading_side, leading_maps, losing_maps) = if s1 > s2 {
            (1u8, s1, s2)
        } else {
            (2u8, s2, s1)
        };

        // Get expected win probability from score
        // Use sport-specific probability model
        let is_tennis = match_key.starts_with("tennis::");
        let is_football = match_key.starts_with("football::");
        let is_dota2 = match_key.starts_with("dota-2::");
        let is_basketball = match_key.starts_with("basketball::");
        let is_mma = match_key.starts_with("mma::");

        // ⛔ FOOTBALL SCORE-EDGE DISABLED — single-source Tipsport data unreliable
        // for obscure leagues (J3 League, Australian NPL), no cross-validation.
        // Campbelltown vs Metro Stars LOST despite 85% model prob → model/data broken.
        if is_football {
            info!("  ⏭️ {} {}-{}: football score-edge DISABLED (single-source risk)", match_key, s1, s2);
            continue;
        }

        let expected_prob = if is_tennis {
            // Tennis: scores are SET counts (0-2)
            match tennis_score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: tennis score not actionable",
                        match_key, s1, s2);
                    continue;
                }
            }
        } else if is_football {
            // Football: goal-based advantage
            match football_score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: football score not actionable",
                        match_key, s1, s2);
                    continue;
                }
            }
        } else if is_dota2 {
            // Dota-2: kill lead
            match dota2_score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: dota-2 score not actionable (diff={}, total={})",
                        match_key, s1, s2, leading_maps - losing_maps, s1 + s2);
                    continue;
                }
            }
        } else if is_basketball {
            // Basketball / e-Basketball (NBA 2K)
            // Point lead model — we don't have quarter/time, use total points as proxy.
            // Guard: garbage parse values (score > 200 = Tipsport concatenation artifact)
            if s1.max(s2) > 200 || s1.max(s2) < 0 {
                info!("  ⏭️ {} {}-{}: basketball score looks like garbage (max>200), skipping",
                    match_key, s1, s2);
                continue;
            }
            match basketball_score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: basketball score not actionable (diff={})",
                        match_key, s1, s2, leading_maps - losing_maps);
                    continue;
                }
            }
        } else if is_mma {
            // MMA: round scores (Bo3 format — first to 2 rounds)
            match mma_score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: MMA score not actionable", match_key, s1, s2);
                    continue;
                }
            }
        } else {
            // CS2: scores can be round-level or map-level
            // But FIRST: sanity check for generic "esports::" keys.
            // Tipsport sends e-football (FIFA) AND e-basketball (NBA 2K) under
            // the same "esports" label. Their scores look like:
            //   e-basketball: 36-30, 100-98 (NBA-style point scores)
            //   e-football:   2-1, 3-0 (FIFA goal counts) → ambiguous with CS2 map scores
            //   CS2 rounds:   12-4, 8-7 (same range as football → indistinguishable)
            //   CS2 maps:     1-0, 2-1 (same as football goals → indistinguishable)
            // Filter: scores > 30 are definitely NOT CS2 (basketball garbage)
            // For scores ≤ 30, we have to trust the data source labeling.
            if match_key.starts_with("esports::") {
                let max_s = s1.max(s2);
                if max_s > 30 {
                    info!("  ⏭️ {} {}-{}: esports score > 30 (e-basketball or parse garbage), skipping",
                        match_key, s1, s2);
                    continue;
                }
                // Also warn when triggering edge on generic esports:: (not verified CS2)
                info!("  ⚠️  {} is generic esports:: key (not confirmed cs2::) — team names may not be CS2. Score {}-{}",
                    match_key, s1, s2);
            }
            match score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: score not actionable (diff={}, max={})",
                        match_key, s1, s2, leading_maps - losing_maps,
                        leading_maps.max(losing_maps));
                    continue;
                }
            }
        };

        // ================================================================
        // CROSS-VALIDATION: Compare HLTV score vs Chance detailed_score
        // Mismatch → HARD SKIP + resync freeze (60s + 2 agreements)
        // Agreement → stake multiplier 1.25 (NOT applied to edge threshold!)
        // Only one source → neutral (no skip, multiplier 1.0)
        // ================================================================
        let detailed = live.payload.detailed_score.as_deref().unwrap_or("");
        let chance_round = if FF_CHANCE_ROUND_PARSER && !detailed.is_empty() {
            parse_cs2_round_score(detailed)
        } else { None };

        // Only cross-validate for CS2/esports matches with round-level scores
        let is_cs2_like = match_key.starts_with("cs2::") || match_key.starts_with("esports::");
        let (cv_skip, cv_stake_mult) = if FF_CROSS_VALIDATION && is_cs2_like && s1.max(s2) > 3 {
            cross_validation_check(Some((s1, s2)), chance_round)
        } else {
            (false, 1.0) // non-CS2 or non-round-level → skip validation
        };

        // RESYNC FREEZE: if this match was previously mismatched, check freeze status
        if FF_RESYNC_FREEZE && is_cs2_like {
            if cv_skip {
                // Mismatch detected → start or extend freeze
                let rs = resync_freeze.entry(match_key.to_string()).or_insert_with(ResyncState::new);
                rs.record_mismatch();
                info!("  🛑 {} CROSS-VALIDATION MISMATCH + FREEZE: HLTV={}-{} vs Chance={:?} detailed='{}' — frozen 60s",
                    match_key, s1, s2, chance_round, detailed);
                continue;
            } else if let Some(rs) = resync_freeze.get_mut(&match_key.to_string()) {
                if cv_stake_mult > 1.0 {
                    // Agreement after previous mismatch — check if resync complete
                    if rs.record_agreement() {
                        info!("  ✅ {} RESYNC COMPLETE: 2 consecutive agreements after freeze — unfreezing",
                            match_key);
                        resync_freeze.remove(&match_key.to_string());
                    } else if rs.is_frozen() {
                        info!("  🧊 {} STILL FROZEN: agreement #{} but need 2 + 60s elapsed",
                            match_key, rs.consecutive_agreements);
                        continue;
                    }
                } else if rs.is_frozen() {
                    // Single source during freeze → still frozen
                    info!("  🧊 {} FROZEN (single source): waiting for 2 cross-validated agreements",
                        match_key);
                    continue;
                }
            }
        } else if cv_skip {
            // Non-freeze mode: simple skip on mismatch
            info!("  🛑 {} CROSS-VALIDATION MISMATCH: HLTV={}-{} vs Chance={:?} detailed='{}' — SKIPPING",
                match_key, s1, s2, chance_round, detailed);
            continue;
        }

        if cv_stake_mult > 1.0 {
            info!("  ✅ {} CROSS-VALIDATED: HLTV={}-{} == Chance={:?} → stake×{:.2} (NOT edge threshold)",
                match_key, s1, s2, chance_round, cv_stake_mult);
        }

        // Cross-map momentum bonus (for match_winner, not map_winner)
        let momentum_bonus = if FF_CROSS_MAP_MOMENTUM && is_cs2_like && !detailed.is_empty() {
            let completed = parse_cs2_completed_maps(detailed);
            cross_map_momentum_bonus(&completed, leading_side)
        } else { 0.0 };

        // ================================================================
        // BET HIERARCHY: MAP WINNER > MATCH WINNER (never both!)
        //
        // When we see a round lead (e.g. 10-4), both markets may have edge:
        //   MAP WINNER → 90% certainty, lower odds (~1.10-1.30)
        //   MATCH WINNER → 72% certainty, higher odds (~1.50-2.00)
        //
        // Strategy: ALWAYS prefer MAP WINNER (higher certainty).
        // Map winner = almost guaranteed profit, match winner = risky
        // because team can win map but lose the Bo3 match 1-2.
        //
        // Only fall back to MATCH WINNER if no map winner odds exist.
        // NEVER bet both → that's double exposure on the same match!
        // ================================================================

        let max_score = s1.max(s2);
        let diff = leading_maps - losing_maps;
        let mut has_map_winner_edge = false;

        // ================================================================
        // ODDS LOOKUP KEY — for generic esports:: live keys (Tipsport labels
        // CS2 matches as "esports::"), try Azuro alternative sport prefixes.
        // E.g. "esports::isurus_vs_players" → check "cs2::isurus_vs_players" in Azuro.
        // The ORIGINAL match_key is kept for cooldown/dedup/logging.
        // ================================================================
        let esports_alts_list: &[&str] = &["cs2", "dota-2", "league-of-legends", "valorant", "basketball", "football", "mma"];
        let resolved_alt_key: Option<String> = if match_key.starts_with("esports::") {
            let tail = &match_key["esports::".len()..];
            esports_alts_list.iter().find_map(|alt| {
                let k = format!("{}::{}", alt, tail);
                if azuro_by_match.contains_key(k.as_str()) || map_winners_by_match.contains_key(k.as_str()) {
                    Some(k)
                } else {
                    None
                }
            })
        } else {
            None
        };
        let odds_lookup_key: &str = resolved_alt_key.as_deref().unwrap_or(match_key);
        if resolved_alt_key.is_some() {
            info!("  🔗 {} → esports→Azuro resolved: {}", match_key, odds_lookup_key);
        }

        let map_odds_list_opt: Option<&Vec<MapWinnerOdds>> = map_winners_by_match
            .get(odds_lookup_key)
            .or_else(|| {
                map_winners_by_match.iter().find_map(|(_k, list)| {
                    if list.iter().any(|mw| teams_match_loose(
                        &live.payload.team1,
                        &live.payload.team2,
                        &mw.team1,
                        &mw.team2,
                    )) {
                        Some(list)
                    } else {
                        None
                    }
                })
            });

        // === STEP 1: Check MAP WINNER edges FIRST (highest priority) ===
        if max_score > 3 && diff >= 3 {
            // This is a round-level score within a CS2 map
            if let Some(map_odds_list) = map_odds_list_opt {
                // CS2 map win probability: based on (diff, total_rounds)
                // Half-switch at round 13: CT→T or T→CT — changes momentum
                // Early game (total ≤ 12): high variance, half-switch coming
                // Mid game (total 13-18): past half, more predictable
                // Late game (total 19+): very few rounds left, high certainty
                let total_rounds = s1 + s2;
                let map_win_prob = cs2_map_win_prob(diff, total_rounds);
                let map_confidence_tier = cs2_confidence_tier(map_win_prob, total_rounds);

                for mw in map_odds_list {
                    if !is_recent_seen_at(&mw.seen_at, now) {
                        info!("  ⏭️ {} {}-{}: MW {} skipped (stale odds)",
                            match_key, s1, s2, mw.market);
                        continue;
                    }

                    // Resolve correct Azuro side by TEAM NAME — HARD BLOCK if ambiguous
                    let azuro_side = match resolve_azuro_side_pair(
                        &live.payload.team1, &live.payload.team2, leading_side,
                        &mw.team1, &mw.team2,
                    ) {
                        Some(s) => s,
                        None => {
                            let leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
                            info!("  🛑 {} MW {}: TEAM IDENTITY AMBIGUOUS! live={}+{} azuro={}+{} — BLOCKING bet",
                                match_key, mw.market, live.payload.team1, live.payload.team2, mw.team1, mw.team2);
                            continue;
                        }
                    };
                    if azuro_side != leading_side {
                        let leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
                        info!("  🔀 {} MW {}: team order fix! live leading={} (side {}), matched azuro side {} ({})",
                            match_key, mw.market, leading_team, leading_side, azuro_side, if azuro_side == 1 { &mw.team1 } else { &mw.team2 });
                    }

                    let mw_implied = if azuro_side == 1 {
                        1.0 / mw.odds_team1
                    } else {
                        1.0 / mw.odds_team2
                    };

                    let mw_edge = (map_win_prob - mw_implied) * 100.0;

                    if mw_edge < MIN_SCORE_EDGE_PCT {
                        info!("  🗺️ {} {}-{}: MW {} edge={:.1}% < min {}%",
                            match_key, s1, s2, mw.market, mw_edge, MIN_SCORE_EDGE_PCT);
                        continue;
                    }

                    let mw_confidence = if mw_edge >= 15.0 { "HIGH" } else { "MEDIUM" };
                    let mw_outcome_id = if azuro_side == 1 {
                        mw.outcome1_id.clone()
                    } else {
                        mw.outcome2_id.clone()
                    };

                    let leading_team_name = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
                    info!("🗺️ MAP WINNER EDGE [PRIORITY]: {} leads {}-{}, {} implied={:.1}%, map_prob={:.1}%, edge={:.1}% (azuro_side={}, tier={}, max_odds={:.2}) — BLOCKING match_winner",
                        leading_team_name, s1, s2, mw.market, mw_implied * 100.0, map_win_prob * 100.0, mw_edge, azuro_side,
                        map_confidence_tier, cs2_dynamic_max_odds(map_confidence_tier));

                    tracker.edge_cooldown.insert(match_key.to_string(), now);
                    has_map_winner_edge = true;

                    // Reorder azuro odds to match live team ordering
                    let (sw1, sw2, so1, so2) = if azuro_side == leading_side {
                        (mw.odds_team1, mw.odds_team2, mw.outcome1_id.clone(), mw.outcome2_id.clone())
                    } else {
                        (mw.odds_team2, mw.odds_team1, mw.outcome2_id.clone(), mw.outcome1_id.clone())
                    };

                    edges.push(ScoreEdge {
                        match_key: format!("{}::{}", match_key, mw.market),
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score1: s1,
                        score2: s2,
                        prev_score1: prev_s1,
                        prev_score2: prev_s2,
                        leading_side,
                        azuro_w1: sw1,
                        azuro_w2: sw2,
                        azuro_bookmaker: format!("{} [{}]", mw.bookmaker, mw.market),
                        azuro_implied_pct: mw_implied * 100.0,
                        score_implied_pct: map_win_prob * 100.0,
                        edge_pct: mw_edge,
                        confidence: mw_confidence,
                        game_id: None,
                        condition_id: mw.condition_id.clone(),
                        outcome1_id: so1,
                        outcome2_id: so2,
                        outcome_id: mw_outcome_id,
                        chain: mw.chain.clone(),
                        azuro_url: mw.url.clone(),
                        cs2_map_confidence: Some(map_confidence_tier),
                        cv_stake_mult,
                    });
                }
            }
        }

        // === STEP 2: MATCH WINNER — only if NO map winner edge found ===
        if has_map_winner_edge {
            info!("  ⏭️ {} {}-{}: SKIPPING match_winner (map_winner edge found — higher certainty)",
                match_key, s1, s2);
            continue;
        }

        // Get current Azuro odds for match winner
        let azuro = match azuro_by_match.get(odds_lookup_key).copied().or_else(|| {
            azuro_by_match.values().find(|item| {
                item.payload.market.as_deref().unwrap_or("match_winner") == "match_winner"
                    && is_recent_seen_at(&item.seen_at, now)
                    && teams_match_loose(
                        &live.payload.team1,
                        &live.payload.team2,
                        &item.payload.team1,
                        &item.payload.team2,
                    )
            }).copied()
        }) {
            Some(a) => a,
            None => {
                info!("  ⏭️ {} {}-{}: NO AZURO ODDS (tried key={}, similar: {})",
                    match_key, s1, s2, odds_lookup_key,
                    azuro_by_match.keys().filter(|k| {
                        let mk_parts: Vec<&str> = odds_lookup_key.split("::").collect();
                        let mk_name = mk_parts.last().unwrap_or(&"");
                        let first_team = mk_name.split("_vs_").next().unwrap_or("");
                        k.contains(first_team)
                    }).cloned().collect::<Vec<_>>().join(", "));
                continue;
            }
        };

        // Azuro implied probability for the leading team
        if !is_recent_seen_at(&azuro.seen_at, now) {
            info!("  ⏭️ {} {}-{}: azuro match_winner stale, skipping", match_key, s1, s2);
            continue;
        }

        // Resolve correct Azuro side by TEAM NAME — HARD BLOCK if ambiguous
        let mw_azuro_side = match resolve_azuro_side_pair(
            &live.payload.team1, &live.payload.team2, leading_side,
            &azuro.payload.team1, &azuro.payload.team2,
        ) {
            Some(s) => s,
            None => {
                info!("  🛑 {} match_winner: TEAM IDENTITY AMBIGUOUS! live={}+{} azuro={}+{} — BLOCKING bet",
                    match_key, live.payload.team1, live.payload.team2, azuro.payload.team1, azuro.payload.team2);
                continue;
            }
        };
        if mw_azuro_side != leading_side {
            let mw_leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
            info!("  🔀 {} MW match_winner: team order fix! live leading={} (side {}), matched azuro side {} ({})",
                match_key, mw_leading_team, leading_side, mw_azuro_side,
                if mw_azuro_side == 1 { &azuro.payload.team1 } else { &azuro.payload.team2 });
        }

        let azuro_implied = if mw_azuro_side == 1 {
            1.0 / azuro.payload.odds_team1
        } else {
            1.0 / azuro.payload.odds_team2
        };

        // EDGE = (expected + momentum) - azuro_implied (raw — cv_stake_mult applied to STAKE only)
        let expected_with_momentum = expected_prob + momentum_bonus;
        let edge = (expected_with_momentum - azuro_implied) * 100.0;
        if momentum_bonus > 0.0 {
            info!("  🔥 {} MOMENTUM BONUS: +{:.1}% (prev map dominant win), prob {:.1}% → {:.1}%",
                match_key, momentum_bonus * 100.0, expected_prob * 100.0, expected_with_momentum * 100.0);
        }

        if edge < MIN_SCORE_EDGE_PCT {
            info!("  ⏭️ {} {}-{}: edge={:.1}% < min {}% (prob={:.0}% az={:.0}%)",
                match_key, s1, s2, edge, MIN_SCORE_EDGE_PCT, expected_prob*100.0, azuro_implied*100.0);
            continue;
        }

        // SANITY CHECK: If expected prob is very high (>85%) but Azuro implied is
        // suspiciously low (<40%), the Azuro condition is likely NOT match_winner
        // (could be totals, handicap, or eFOOTBALL misclassification).
        // Real match_winner odds at 4-0 football lead should be >90% implied.
        if expected_prob > 0.85 && azuro_implied < 0.40 {
            info!("🛡️ SANITY REJECT: {} {}-{}: expected {:.0}% but Azuro only {:.0}% — likely wrong market or eFOOTBALL!",
                match_key, s1, s2, expected_prob * 100.0, azuro_implied * 100.0);
            continue;
        }

        // Confidence based on edge size
        let confidence = if edge >= 15.0 { "HIGH" } else { "MEDIUM" };

        let leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
        info!("⚡ MATCH WINNER EDGE [FALLBACK]: {} leads {}-{}, Azuro implied {:.1}%, expected {:.1}%, edge {:.1}% (azuro_side={}, no map_winner odds available)",
            leading_team, s1, s2, azuro_implied * 100.0, expected_prob * 100.0, edge, mw_azuro_side);

        tracker.edge_cooldown.insert(match_key.to_string(), now);

        let outcome_id = if mw_azuro_side == 1 {
            azuro.payload.outcome1_id.clone()
        } else {
            azuro.payload.outcome2_id.clone()
        };

        // Reorder azuro odds to match live team ordering
        let (sw1, sw2, so1, so2) = if mw_azuro_side == leading_side {
            (azuro.payload.odds_team1, azuro.payload.odds_team2,
             azuro.payload.outcome1_id.clone(), azuro.payload.outcome2_id.clone())
        } else {
            (azuro.payload.odds_team2, azuro.payload.odds_team1,
             azuro.payload.outcome2_id.clone(), azuro.payload.outcome1_id.clone())
        };

        edges.push(ScoreEdge {
            match_key: match_key.to_string(),
            team1: live.payload.team1.clone(),
            team2: live.payload.team2.clone(),
            score1: s1,
            score2: s2,
            prev_score1: prev_s1,
            prev_score2: prev_s2,
            leading_side,
            azuro_w1: sw1,
            azuro_w2: sw2,
            azuro_bookmaker: azuro.payload.bookmaker.clone(),
            azuro_implied_pct: azuro_implied * 100.0,
            score_implied_pct: expected_prob * 100.0,
            edge_pct: edge,
            confidence,
            game_id: azuro.payload.game_id.clone(),
            condition_id: azuro.payload.condition_id.clone(),
            outcome1_id: so1,
            outcome2_id: so2,
            outcome_id,
            chain: azuro.payload.chain.clone(),
            azuro_url: azuro.payload.url.clone(),
            cs2_map_confidence: None, // match_winner, not map_winner
            cv_stake_mult,
        });
    }

    // Cleanup old entries
    tracker.cleanup();

    edges
}

fn format_score_edge_alert(e: &ScoreEdge, alert_id: u32) -> String {
    let leading_team = if e.leading_side == 1 { &e.team1 } else { &e.team2 };
    let azuro_odds = if e.leading_side == 1 { e.azuro_w1 } else { e.azuro_w2 };

    let conf_emoji = if e.confidence == "HIGH" { "🟢" } else { "🟡" };

    let url_line = e.azuro_url.as_ref()
        .map(|u| format!("\n🔗 <a href=\"{}\">Azuro link</a>", u))
        .unwrap_or_default();

    let exec_ready = if e.condition_id.is_some() && e.outcome_id.is_some() {
        "✅ BET READY"
    } else {
        "⚠️ Manuální bet"
    };

    let sport = e.match_key.split("::").next().unwrap_or("?").to_uppercase();

    format!(
        "⚡ <b>#{}</b> {} <b>SCORE EDGE</b> [{}]\n\
         🏷️ <b>{}</b>\n\
         \n\
         <b>{}</b> vs <b>{}</b>\n\
         🔴 LIVE: <b>{}-{}</b> (bylo {}-{})\n\
         \n\
         📊 <b>{}</b> VEDE!\n\
         \n\
         🎯 Azuro kurz ({}):\n\
         {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         Azuro implied: <b>{:.1}%</b>\n\
         \n\
         📈 Score-implied: <b>{:.1}%</b>\n\
         ⚡ EDGE: <b>{:.1}%</b> — kurzy JEŠTĚ nereagovaly!\n\
         \n\
         🏦 {}\n\
         💡 BET <b>{}</b> @ <b>{:.2}</b>{}\n\
         \n\
         Reply: <code>{} YES $3</code> / <code>{} OPP $3</code> / <code>{} NO</code>",
        alert_id, conf_emoji, e.confidence, sport,
        e.team1, e.team2,
        e.score1, e.score2, e.prev_score1, e.prev_score2,
        leading_team,
        e.azuro_bookmaker,
        e.team1, e.azuro_w1, e.team2, e.azuro_w2,
        e.azuro_implied_pct,
        e.score_implied_pct,
        e.edge_pct,
        exec_ready,
        leading_team, azuro_odds, url_line,
        alert_id, alert_id, alert_id,
    )
}

// ====================================================================
// Odds comparison logic
// ====================================================================

#[derive(Clone)]
struct OddsAnomaly {
    detected_at: DateTime<Utc>,
    match_key: String,
    team1: String,
    team2: String,
    azuro_w1: f64,
    azuro_w2: f64,
    azuro_bookmaker: String,
    azuro_url: Option<String>,
    market_w1: f64,
    market_w2: f64,
    market_bookmaker: String,
    /// Which side has value on Azuro: 1 or 2
    value_side: u8,
    /// How much higher Azuro odds are vs market (%)
    discrepancy_pct: f64,
    /// Confidence: HIGH / MEDIUM / LOW
    confidence: &'static str,
    /// Reasons for confidence level
    confidence_reasons: Vec<String>,
    /// Was team order swapped for comparison?
    teams_swapped: bool,
    /// Is the match currently live?
    is_live: bool,
    /// Live score if available
    live_score: Option<String>,
    // === Azuro execution data ===
    game_id: Option<String>,
    condition_id: Option<String>,
    outcome1_id: Option<String>,
    outcome2_id: Option<String>,
    /// Outcome ID for the VALUE side
    outcome_id: Option<String>,
    chain: Option<String>,
}

// === Executor types ===

#[derive(Debug, Clone, Serialize)]
struct ActiveBet {
    alert_id: u32,
    bet_id: String,
    match_key: String,
    team1: String,
    team2: String,
    value_team: String,
    amount_usd: f64,
    odds: f64,
    placed_at: String,
    condition_id: String,
    outcome_id: String,
    graph_bet_id: Option<String>,
    token_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExecutorBetResponse {
    status: Option<String>,
    #[serde(rename = "betId")]
    bet_id: Option<String>,
    #[serde(rename = "tokenId")]
    token_id: Option<String>,
    #[serde(rename = "graphBetId")]
    graph_bet_id: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExecutorHealthResponse {
    status: Option<String>,
    wallet: Option<String>,
    balance: Option<String>,
    #[serde(rename = "relayerAllowance")]
    relayer_allowance: Option<String>,
    #[serde(rename = "activeBets")]
    active_bets: Option<u32>,
    #[serde(rename = "toolkitAvailable")]
    toolkit_available: Option<bool>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExecutorCashoutResponse {
    status: Option<String>,
    #[serde(rename = "cashoutId")]
    cashout_id: Option<String>,
    state: Option<String>,
    #[serde(rename = "cashoutOdds")]
    cashout_odds: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CashoutCheckResponse {
    available: Option<bool>,
    #[serde(rename = "cashoutOdds")]
    cashout_odds: Option<String>,
    #[serde(rename = "calculationId")]
    calculation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckPayoutResponse {
    #[serde(rename = "tokenId")]
    token_id: Option<String>,
    #[serde(rename = "payoutUsd")]
    payout_usd: Option<f64>,
    claimable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ClaimResponse {
    status: Option<String>,
    #[serde(rename = "txHash")]
    tx_hash: Option<String>,
    claimed: Option<u32>,
    #[serde(rename = "totalPayoutUsd")]
    total_payout_usd: Option<f64>,
    #[serde(rename = "newBalanceUsd")]
    new_balance_usd: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BetStatusResponse {
    id: Option<String>,
    state: Option<String>,
    result: Option<String>,
    #[serde(rename = "tokenId")]
    token_id: Option<String>,
    #[serde(rename = "graphBetId")]
    graph_bet_id: Option<String>,
    #[serde(rename = "conditionId")]
    condition_id_resp: Option<String>,
}

/// Normalize team name for comparison: lowercase, strip whitespace, remove common suffixes
fn norm_team(name: &str) -> String {
    name.to_lowercase()
        .replace(" esports", "")
        .replace(" gaming", "")
        .replace(" cs go", "")
        .replace(" cs2", "")
        .replace(" (w)", "")
        .replace("(w)", "")
        .trim()
        .to_string()
}

/// Check if two team names likely refer to the same team
fn teams_match(a: &str, b: &str) -> bool {
    let na = norm_team(a);
    let nb = norm_team(b);
    if na == nb { return true; }
    // One contains the other (e.g. "MIBR" vs "MIBR Academy")
    if na.contains(&nb) || nb.contains(&na) { return true; }
    // Word-set match: handles first/last name reversal (tennis, individual sports)
    // "lea ma" vs "ma lea", "andrea pellegrino" vs "pellegrino andrea"
    if words_match(&na, &nb) { return true; }
    // Levenshtein-like: if short and differ by 1-2 chars, might be typo
    if na.len() >= 3 && nb.len() >= 3 {
        let shorter = na.len().min(nb.len());
        let common = na.chars().zip(nb.chars()).filter(|(a, b)| a == b).count();
        if common as f64 / shorter as f64 > 0.75 { return true; }
    }
    false
}

/// Check if two names have the same set of words (order-independent)
/// Handles "Firstname Lastname" vs "Lastname Firstname" common in tennis/individual sports
fn words_match(a: &str, b: &str) -> bool {
    let mut wa: Vec<&str> = a.split_whitespace().collect();
    let mut wb: Vec<&str> = b.split_whitespace().collect();
    if wa.len() < 2 || wb.len() < 2 { return false; }
    // Must have same number of words
    if wa.len() != wb.len() { return false; }
    wa.sort();
    wb.sort();
    wa == wb
}

/// Detect if odds from two sources have team1/team2 swapped
/// Returns (market_w1_aligned, market_w2_aligned, is_swapped, is_ambiguous)
/// ambiguous = true when normal_score == swap_score (cannot determine team order)
fn align_teams(azuro: &OddsPayload, market: &OddsPayload) -> (f64, f64, bool, bool) {
    let a1 = norm_team(&azuro.team1);
    let a2 = norm_team(&azuro.team2);
    let m1 = norm_team(&market.team1);
    let m2 = norm_team(&market.team2);

    // Normal order: azuro.t1 ↔ market.t1
    let normal_score = (if teams_match(&a1, &m1) { 1 } else { 0 })
                     + (if teams_match(&a2, &m2) { 1 } else { 0 });
    // Swapped: azuro.t1 ↔ market.t2
    let swap_score = (if teams_match(&a1, &m2) { 1 } else { 0 })
                   + (if teams_match(&a2, &m1) { 1 } else { 0 });

    let ambiguous = normal_score == swap_score;

    if swap_score > normal_score {
        // Teams are swapped — flip market odds
        (market.odds_team2, market.odds_team1, true, ambiguous)
    } else {
        (market.odds_team1, market.odds_team2, false, ambiguous)
    }
}

fn find_odds_anomalies(state: &StateResponse) -> Vec<OddsAnomaly> {
    let now = Utc::now();
    // Build set of currently live match_keys
    let live_keys: std::collections::HashMap<String, &LiveItem> = state.live.iter()
        .map(|l| (l.match_key.clone(), l))
        .collect();

    // Group odds by match_key
    let mut by_match: std::collections::HashMap<String, Vec<&StateOddsItem>> = std::collections::HashMap::new();
    for item in &state.odds {
        by_match.entry(item.match_key.clone()).or_default().push(item);
    }

    let mut anomalies = Vec::new();

    for (match_key, items) in &by_match {
        let azuro_items: Vec<&&StateOddsItem> = items.iter()
            .filter(|i| i.payload.bookmaker.starts_with("azuro_") && is_recent_seen_at(&i.seen_at, now))
            .collect();
        // Include hltv-featured (20bet, ggbet, etc.) as market reference!
        let market_items: Vec<&&StateOddsItem> = items.iter()
            .filter(|i| !i.payload.bookmaker.starts_with("azuro_") && is_recent_seen_at(&i.seen_at, now))
            .collect();

        if azuro_items.is_empty() || market_items.is_empty() {
            continue;
        }

        let azuro = &azuro_items[0].payload;
        let is_live = live_keys.contains_key(match_key.as_str());
        let live_score = live_keys.get(match_key.as_str()).map(|l| {
            format!("{}-{}", l.payload.score1, l.payload.score2)
        });

        // LIVE-ONLY mode: ignore prematch odds anomalies completely.
        if !is_live {
            continue;
        }

        // For each market source, align teams and compute discrepancy
        let mut total_m_w1 = 0.0_f64;
        let mut total_m_w2 = 0.0_f64;
        let mut any_swapped = false;
        let mut any_ambiguous = false;
        let mut market_count = 0;

        for mi in &market_items {
            let (mw1, mw2, swapped, ambiguous) = align_teams(azuro, &mi.payload);
            total_m_w1 += mw1;
            total_m_w2 += mw2;
            if swapped { any_swapped = true; }
            if ambiguous { any_ambiguous = true; }
            market_count += 1;
        }

        // HARD BLOCK: if team identity is ambiguous, skip entirely (same safety as score edge path)
        if any_ambiguous {
            info!("🚫 ODDS ANOMALY TEAM AMBIGUOUS: {} — azuro({} vs {}) cannot reliably match market teams, skipping",
                match_key, azuro.team1, azuro.team2);
            continue;
        }

        let avg_w1 = total_m_w1 / market_count as f64;
        let avg_w2 = total_m_w2 / market_count as f64;

        let market_bookie = market_items.iter().map(|i| i.payload.bookmaker.as_str()).collect::<Vec<_>>().join("+");

        let disc_w1 = (azuro.odds_team1 / avg_w1 - 1.0) * 100.0;
        let disc_w2 = (azuro.odds_team2 / avg_w2 - 1.0) * 100.0;

        // === Confidence scoring ===
        let mut reasons: Vec<String> = Vec::new();
        let mut penalty = 0;

        // PENALTY: teams were swapped
        if any_swapped {
            reasons.push(format!("Týmy v jiném pořadí ✅ zarovnáno (azuro: {} vs {}, trh: {} vs {})",
                azuro.team1, azuro.team2,
                market_items[0].payload.team1, market_items[0].payload.team2));
            penalty += 1;
        }

        // PENALTY: extreme odds (likely near-resolved match)
        let max_odds = azuro.odds_team1.max(azuro.odds_team2);
        if max_odds > 8.0 {
            reasons.push(format!("Extrémní odds ({:.2}) — pravděpodobně rozhodnutý zápas", max_odds));
            penalty += 2;
        }

        // PENALTY: very high discrepancy is suspicious
        let max_disc = disc_w1.max(disc_w2);
        if max_disc > 40.0 {
            reasons.push(format!("{:.0}% discrepancy je podezřele vysoká — stale data?", max_disc));
            penalty += 2;
        }

        // CRITICAL: Favorite/underdog FLIP detection
        // If Azuro says team1 is favorite (w1 < w2) but market says team1 is underdog (w1 > w2)
        // → odds_team1/odds_team2 are probably SWAPPED in one source → FALSE signal!
        let azuro_fav1 = azuro.odds_team1 < azuro.odds_team2; // Azuro thinks team1 is favorite
        let market_fav1 = avg_w1 < avg_w2; // Market thinks team1 is favorite
        if azuro_fav1 != market_fav1 {
            reasons.push("⚠️ FAVORIT PROHOZENÝ: Azuro a trh se neshodují kdo je favorit!".into());
            penalty += 4; // Very strong signal this is data error
        }

        // BONUS: multiple market sources agree
        if market_count >= 2 {
            reasons.push(format!("{} market zdrojů se shoduje", market_count));
            penalty -= 1;
        }

        // CRITICAL: Identical Azuro odds guard (e.g. 1.84/1.84 = oracle didn't set real prices)
        // When both sides have same odds, any "edge" is phantom — pure data artifact
        let azuro_odds_diff = (azuro.odds_team1 - azuro.odds_team2).abs();
        if azuro_odds_diff < 0.02 {
            reasons.push(format!("⚠️ IDENTICKÉ AZURO ODDS: {:.2}/{:.2} — oracle bug, phantom edge!",
                azuro.odds_team1, azuro.odds_team2));
            penalty += 6; // Guarantees LOW confidence → skip entirely
        }

        // BONUS: Azuro odds are reasonable (1.2 - 5.0 range)
        if azuro.odds_team1 > 1.15 && azuro.odds_team1 < 5.0 && azuro.odds_team2 > 1.15 && azuro.odds_team2 < 5.0 {
            reasons.push("Azuro odds v normálním rozsahu".into());
        } else {
            penalty += 1;
        }

        let confidence = if penalty <= 0 {
            "HIGH"
        } else if penalty <= 2 {
            "MEDIUM"
        } else {
            "LOW"
        };

        // === Only alert HIGH and MEDIUM confidence ===
        // LOW = skip entirely (stale data, live mismatch, etc.)
        if confidence == "LOW" {
            continue;
        }

        let side1_ok = disc_w1 > MIN_EDGE_PCT;
        let side2_ok = disc_w2 > MIN_EDGE_PCT;

        // === FAVORITE-ONLY LOGIC ===
        // Strategie: sázíme POUZE na FAVORITA když Azuro nabízí lepší odds než trh.
        // Underdog anomálie BEZ score-edge je noise — favorit s lepším kurzem = reálná value.
        // Favorit = strana s NIŽŠÍMI Azuro odds (vyšší implied probability).
        let fav_side = if azuro.odds_team1 < azuro.odds_team2 {
            1 // team1 je favorit (nižší odds)
        } else if azuro.odds_team2 < azuro.odds_team1 {
            2 // team2 je favorit
        } else {
            // Odds jsou rovné — povolíme obě strany
            0
        };

        let selected_side = if fav_side == 0 {
            // Equal odds — pick stronger discrepancy
            match (side1_ok, side2_ok) {
                (true, true) => { if disc_w1 >= disc_w2 { 1 } else { 2 } }
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 0,
            }
        } else if fav_side == 1 {
            // team1 je favorit — jen side1
            if side1_ok {
                if side2_ok && !side1_ok {
                    // Log underdog anomaly but skip it
                    info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                        match_key, disc_w2, azuro.team2);
                }
                1
            } else {
                if side2_ok {
                    info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                        match_key, disc_w2, azuro.team2);
                }
                0 // Favorit nemá edge → skip
            }
        } else {
            // team2 je favorit — jen side2
            if side2_ok {
                if side1_ok && !side2_ok {
                    info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                        match_key, disc_w1, azuro.team1);
                }
                2
            } else {
                if side1_ok {
                    info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                        match_key, disc_w1, azuro.team1);
                }
                0 // Favorit nemá edge → skip
            }
        };

        if any_swapped {
            info!("🔀 ODDS ANOMALY {}: team order different (azuro: {} vs {} | market: {} vs {}) — odds aligned correctly, value_side={}",
                match_key, azuro.team1, azuro.team2,
                market_items[0].payload.team1, market_items[0].payload.team2,
                selected_side);
        }

        if selected_side == 1 {
            anomalies.push(OddsAnomaly {
                detected_at: now,
                match_key: match_key.clone(),
                team1: azuro.team1.clone(),
                team2: azuro.team2.clone(),
                azuro_w1: azuro.odds_team1,
                azuro_w2: azuro.odds_team2,
                azuro_bookmaker: azuro.bookmaker.clone(),
                azuro_url: azuro.url.clone(),
                market_w1: avg_w1,
                market_w2: avg_w2,
                market_bookmaker: market_bookie,
                value_side: 1,
                discrepancy_pct: disc_w1,
                confidence,
                confidence_reasons: reasons,
                teams_swapped: any_swapped,
                is_live,
                live_score,
                game_id: azuro.game_id.clone(),
                condition_id: azuro.condition_id.clone(),
                outcome1_id: azuro.outcome1_id.clone(),
                outcome2_id: azuro.outcome2_id.clone(),
                outcome_id: azuro.outcome1_id.clone(),
                chain: azuro.chain.clone(),
            });
        } else if selected_side == 2 {
            anomalies.push(OddsAnomaly {
                detected_at: now,
                match_key: match_key.clone(),
                team1: azuro.team1.clone(),
                team2: azuro.team2.clone(),
                azuro_w1: azuro.odds_team1,
                azuro_w2: azuro.odds_team2,
                azuro_bookmaker: azuro.bookmaker.clone(),
                azuro_url: azuro.url.clone(),
                market_w1: avg_w1,
                market_w2: avg_w2,
                market_bookmaker: market_bookie,
                value_side: 2,
                discrepancy_pct: disc_w2,
                confidence,
                confidence_reasons: reasons,
                teams_swapped: any_swapped,
                is_live,
                live_score,
                game_id: azuro.game_id.clone(),
                condition_id: azuro.condition_id.clone(),
                outcome1_id: azuro.outcome1_id.clone(),
                outcome2_id: azuro.outcome2_id.clone(),
                outcome_id: azuro.outcome2_id.clone(),
                chain: azuro.chain.clone(),
            });
        }
    }

    // Sort: HIGH first, then by discrepancy desc
    anomalies.sort_by(|a, b| {
        let conf_ord = match (a.confidence, b.confidence) {
            ("HIGH", "HIGH") | ("MEDIUM", "MEDIUM") => std::cmp::Ordering::Equal,
            ("HIGH", _) => std::cmp::Ordering::Less,
            (_, "HIGH") => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        };
        conf_ord.then_with(|| b.discrepancy_pct.partial_cmp(&a.discrepancy_pct).unwrap_or(std::cmp::Ordering::Equal))
    });
    anomalies
}

fn format_anomaly_alert(a: &OddsAnomaly, alert_id: u32) -> String {
    let value_team = if a.value_side == 1 { &a.team1 } else { &a.team2 };
    let azuro_odds = if a.value_side == 1 { a.azuro_w1 } else { a.azuro_w2 };
    let market_odds = if a.value_side == 1 { a.market_w1 } else { a.market_w2 };

    let conf_emoji = match a.confidence {
        "HIGH" => "🟢",
        "MEDIUM" => "🟡",
        _ => "🔴",
    };

    let url_line = a.azuro_url.as_ref()
        .map(|u| format!("\n🔗 <a href=\"{}\">Azuro link</a>", u))
        .unwrap_or_default();

    let swap_warn = if a.teams_swapped {
        "\n✅ Týmy v jiném pořadí mezi zdroji — odds správně zarovnány"
    } else {
        ""
    };

    let live_line = if a.is_live {
        format!("\n🔴 LIVE: {}", a.live_score.as_deref().unwrap_or("probíhá"))
    } else {
        "\n⏳ Prematch".to_string()
    };

    let reasons_text = if a.confidence_reasons.is_empty() {
        String::new()
    } else {
        format!("\n📋 {}", a.confidence_reasons.join(" | "))
    };

    let exec_ready = if a.condition_id.is_some() && a.outcome_id.is_some() {
        "✅ BET READY"
    } else {
        "⚠️ Manuální bet (chybí contract data)"
    };

    let sport = a.match_key.split("::").next().unwrap_or("?").to_uppercase();

    format!(
        "🎯 <b>#{}</b> {} <b>ODDS ANOMALY</b> [{}]\n\
         🏷️ <b>{}</b>\n\
         \n\
         <b>{}</b> vs <b>{}</b>{}{}\n\
         \n\
         📊 Azuro ({}):\n\
         {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         \n\
         📊 Trh ({}):\n\
         {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         \n\
         ⚡ <b>{}</b> na Azuru o <b>{:.1}%</b> VYŠŠÍ než trh\n\
         Azuro: {:.2} vs Trh: {:.2}{}{}\n\
         \n\
         🏦 {}\n\
         💡 BET <b>{}</b> @ <b>{:.2}</b>\n\
         \n\
         Reply: <code>{} YES $3</code> / <code>{} OPP $3</code> / <code>{} NO</code>",
        alert_id, conf_emoji, a.confidence, sport,
        a.team1, a.team2, live_line, swap_warn,
        a.azuro_bookmaker,
        a.team1, a.azuro_w1, a.team2, a.azuro_w2,
        a.market_bookmaker,
        a.team1, a.market_w1, a.team2, a.market_w2,
        value_team, a.discrepancy_pct,
        azuro_odds, market_odds, reasons_text, url_line,
        exec_ready,
        value_team, azuro_odds,
        alert_id, alert_id, alert_id
    )
}

fn format_opportunity_alert(opp: &Opportunity) -> String {
    let emoji = match opp.opp_type.as_str() {
        "arb_cross_book" => "💰",
        "score_momentum" => "📈",
        "tight_spread_underdog" => "🎲",
        _ => "❓",
    };

    let score_str = if let Some(detailed) = &opp.detailed_score {
        format!("{} ({})", opp.score, detailed)
    } else {
        opp.score.clone()
    };

    format!(
        "{} <b>{}</b>\n\
         \n\
         <b>{}</b> vs <b>{}</b>\n\
         Score: <b>{}</b>\n\
         \n\
         Signal: {}\n\
         Edge: <b>{:.1}%</b> | Odds: <b>{:.2}</b>\n\
         Bookmaker: {}\n\
         Confidence: {:.0}%\n\
         \n\
         Reply: <code>YES $5</code> / <code>NO</code>",
        emoji, opp.opp_type.replace('_', " ").to_uppercase(),
        opp.team1, opp.team2,
        score_str,
        opp.signal,
        opp.edge_pct, opp.odds,
        opp.bookmaker,
        opp.confidence * 100.0
    )
}

// ====================================================================
// Main loop
// ====================================================================

/// Parse reply for manual bet.
/// Supports:
/// - "3 YES $5", "3 YES", "YES $5", "YES"
/// - "3 OPP $5", "3 OPP", "OPP $5", "OPP"
/// - "3 $5" (shorthand for YES)
/// Returns: (alert_id, amount, opposite_side)
/// If no alert_id given, returns 0 (caller uses latest alert)
fn parse_bet_reply(text: &str) -> Option<(u32, f64, bool)> {
    fn parse_amount_token(token: &str) -> Option<f64> {
        let cleaned = token.trim().trim_start_matches('$').trim_end_matches('$').trim();
        cleaned.parse::<f64>().ok().filter(|v| *v > 0.0)
    }

    let text = text.trim();
    let parts: Vec<&str> = text.splitn(4, char::is_whitespace).collect();
    if parts.is_empty() { return None; }

    // Format 1: "{id} YES|OPP [$]{amount}" e.g. "3 YES $5", "3 OPP $5"
    // Format 2: "{id} YES|OPP" e.g. "3 YES" / "3 OPP" → default $3
    // Format 3: "YES|OPP [$]{amount}" e.g. "YES $5" / "OPP $5" → latest alert (id=0)
    // Format 4: "YES|OPP" → latest alert, default $3
    // Format 5: "{id} [$]{amount}" e.g. "3 $5" or "3 5$" → shorthand for YES

    let first = parts[0].trim_start_matches('#');

    if let Ok(id) = first.parse::<u32>() {
        // Starts with number → Format 1/2/5
        if parts.len() < 2 { return None; }
        if parts[1].eq_ignore_ascii_case("YES") || parts[1].eq_ignore_ascii_case("OPP") {
            let opposite = parts[1].eq_ignore_ascii_case("OPP");
            let amount = if parts.len() >= 3 {
                parse_amount_token(parts[2]).unwrap_or(MANUAL_BET_DEFAULT_USD)
            } else {
                MANUAL_BET_DEFAULT_USD
            };
            Some((id, amount, opposite))
        } else {
            // Shorthand: "{id} $5"
            match parse_amount_token(parts[1]) {
                Some(amount) => Some((id, amount, false)),
                _ => None,
            }
        }
    } else if parts[0].eq_ignore_ascii_case("YES") || parts[0].eq_ignore_ascii_case("OPP") {
        // Starts with YES/OPP → Format 3 or 4 (id=0 means "latest")
        let opposite = parts[0].eq_ignore_ascii_case("OPP");
        let amount = if parts.len() >= 2 {
            parse_amount_token(parts[1]).unwrap_or(MANUAL_BET_DEFAULT_USD)
        } else {
            MANUAL_BET_DEFAULT_USD
        };
        Some((0, amount, opposite))
    } else {
        None
    }
}

/// Parse reply like "3 NO" → alert_id
fn parse_no_reply(text: &str) -> Option<u32> {
    let text = text.trim();
    let parts: Vec<&str> = text.splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 { return None; }
    let id: u32 = parts[0].trim_start_matches('#').parse().ok()?;
    if parts[1].eq_ignore_ascii_case("NO") || parts[1].eq_ignore_ascii_case("SKIP") {
        Some(id)
    } else {
        None
    }
}

fn extract_alert_id_from_text(text: &str) -> Option<u32> {
    for token in text.split_whitespace() {
        let cleaned = token
            .trim_start_matches('#')
            .trim_matches(|c: char| !c.is_ascii_digit());
        if cleaned.is_empty() {
            continue;
        }
        if let Ok(id) = cleaned.parse::<u32>() {
            return Some(id);
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt().with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?)).init();

    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .unwrap_or_else(|_| "7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk".to_string());
    let feed_hub_url = std::env::var("FEED_HUB_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let executor_url = std::env::var("EXECUTOR_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3030".to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    // Get bot info
    let bot_id = tg_get_me(&client, &token).await?;
    info!("Telegram bot started, bot_id={}", bot_id);

    // Discover chat_id: either from env or from first message
    let mut chat_id: Option<i64> = std::env::var("TELEGRAM_CHAT_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    let mut update_offset: i64 = 0;
    let mut sent_alerts: Vec<SentAlert> = Vec::new();
    let mut alert_counter: u32 = 0;
    let mut alert_map: HashMap<u32, OddsAnomaly> = HashMap::new();
    let mut msg_id_to_alert_id: HashMap<i64, u32> = HashMap::new();
    let mut active_bets: Vec<ActiveBet> = Vec::new();
    let mut score_tracker = ScoreTracker::new();
    // In-flight dedup: condition IDs currently being sent to executor (prevents race condition
    // where two score edges for same match arrive in same poll tick before executor responds)
    let mut inflight_conditions: HashSet<String> = HashSet::new();

    // === RE-BET STATE: track bets per condition for re-bet logic ===
    let mut rebet_tracker: HashMap<String, ReBetState> = HashMap::new();

    // === EXPOSURE TRACKING: per-condition and per-match wagered amounts ===
    // condition_id → total USD wagered today
    let mut condition_exposure: HashMap<String, f64> = HashMap::new();
    // base_match_key → total USD wagered today
    let mut match_exposure: HashMap<String, f64> = HashMap::new();
    // sport → total USD wagered today (per-sport cap)
    let mut sport_exposure: HashMap<String, f64> = HashMap::new();
    // Total USD in all pending/inflight bets (for inflight cap)
    let mut inflight_wagered_total: f64 = 0.0;

    // === RESYNC FREEZE: track cross-validation mismatches per match ===
    let mut resync_freeze: HashMap<String, ResyncState> = HashMap::new();

    // === BANKROLL: fetched from executor at startup, updated on claims ===
    let mut current_bankroll: f64 = 65.0; // default, updated from /health
    // Start-of-day bankroll: frozen at day start, used for daily loss limit calc
    // Prevents "shrinking box" where losing bets reduce bankroll → reduce limit → stop earlier
    let mut start_of_day_bankroll: f64 = 65.0;
    let mut sod_loaded_from_file = false; // guard: don't overwrite SOD from executor if file had valid value

    // BUG #6 FIX: Persist auto_bet_count across restarts (daily file)
    let bet_count_path = "data/bet_count_daily.txt";
    let mut auto_bet_count: u32 = 0;
    {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        if Path::new(bet_count_path).exists() {
            if let Ok(contents) = std::fs::read_to_string(bet_count_path) {
                let parts: Vec<&str> = contents.trim().split('|').collect();
                if parts.len() >= 2 && parts[0] == today {
                    auto_bet_count = parts[1].parse().unwrap_or(0);
                    info!("📋 Loaded auto_bet_count={} for today ({})", auto_bet_count, today);
                } else {
                    info!("📋 bet_count_daily.txt is from a different day, resetting to 0");
                }
            }
        }
    }

    // === DAILY P&L TRACKING (NET loss limit) ===
    let mut daily_wagered: f64 = 0.0;
    let mut daily_returned: f64 = 0.0;
    let mut daily_date = Utc::now().format("%Y-%m-%d").to_string();
    let mut daily_loss_alert_sent = false;
    let mut daily_loss_last_reminder: Option<DateTime<Utc>> = None;
    // Load from daily_pnl.json if exists (includes SOD bankroll persistence)
    {
        let pnl_path = "data/daily_pnl.json";
        if Path::new(pnl_path).exists() {
            if let Ok(contents) = std::fs::read_to_string(pnl_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if v["date"].as_str() == Some(&daily_date) {
                        daily_wagered = v["wagered"].as_f64().unwrap_or(0.0);
                        daily_returned = v["returned"].as_f64().unwrap_or(0.0);
                        // Restore SOD bankroll from file (survives mid-day restarts)
                        if let Some(sod) = v["sod_bankroll"].as_f64() {
                            if sod > 0.0 {
                                start_of_day_bankroll = sod;
                                sod_loaded_from_file = true;
                                info!("📋 Restored SOD bankroll from file: ${:.2}", sod);
                            }
                        }
                        // Fallback: if no sod_bankroll in file but we have daily P&L data,
                        // mark as loaded to prevent executor overwrite (SOD = default, which is
                        // better than using current depleted balance)
                        if !sod_loaded_from_file && daily_wagered > 0.0 {
                            sod_loaded_from_file = true;
                            info!("📋 SOD bankroll not in file, but mid-day restart detected (wagered > 0). Keeping default SOD=${:.2} to prevent shrinking-box", start_of_day_bankroll);
                        }
                        info!("📋 Loaded daily P&L: wagered={:.2} returned={:.2} net={:.2} sod_br=${:.2}",
                            daily_wagered, daily_returned, daily_returned - daily_wagered, start_of_day_bankroll);
                    } else {
                        info!("📋 daily_pnl.json is from different day, resetting");
                    }
                }
            }
        }
    }

    // === MUTE MANUAL ALERTS (toggle via /nabidka and /nabidkaup) ===
    // When true, only auto-bet confirmations + portfolio + claim messages are sent.
    // Manual "opportunity" alerts (score-edge MEDIUM, odds anomaly manual) are suppressed.
    let mut mute_manual_alerts = false;

    // === WATCHDOG: SAFE MODE ===
    let mut safe_mode = false;
    let mut last_good_data = std::time::Instant::now();

    // === EVENT LOG HELPER ===
    let events_path = "data/events.jsonl";
    let log_event = |event_type: &str, data: &serde_json::Value| {
        let mut event = data.clone();
        if let Some(obj) = event.as_object_mut() {
            obj.insert("ts".to_string(), serde_json::json!(Utc::now().to_rfc3339()));
            obj.insert("type".to_string(), serde_json::json!(event_type));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(events_path) {
            use std::io::Write;
            let _ = writeln!(f, "{}", event);
        }
    };

    // === PERMANENT BET LEDGER (append-only, NEVER deleted) ===
    let ledger_path = "data/ledger.jsonl";
    let ledger_write = |event: &str, data: &serde_json::Value| {
        let mut entry = data.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("ts".to_string(), serde_json::json!(Utc::now().to_rfc3339()));
            obj.insert("event".to_string(), serde_json::json!(event));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(ledger_path) {
            use std::io::Write;
            let _ = writeln!(f, "{}", entry);
        }
    };

    // === DEDUP: track already-bet match keys + condition IDs (persisted across restarts) ===
    let bet_history_path = "data/bet_history.txt";
    let mut already_bet_matches: HashSet<String> = HashSet::new();
    let mut already_bet_conditions: HashSet<String> = HashSet::new();
    // BUG #1 FIX: Also track base match keys (without ::mapN_winner suffix)
    // to prevent multiple map-winner bets on the same match (triple exposure)
    let mut already_bet_base_matches: HashSet<String> = HashSet::new();
    // Load from file on startup
    if Path::new(bet_history_path).exists() {
        if let Ok(contents) = std::fs::read_to_string(bet_history_path) {
            for line in contents.lines() {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 2 {
                    already_bet_matches.insert(parts[0].to_string());
                    already_bet_conditions.insert(parts[1].to_string());
                    // Extract base match key (strip ::mapN_winner suffix)
                    let base_key = strip_map_winner_suffix(parts[0]);
                    already_bet_base_matches.insert(base_key);
                }
            }
            info!("📋 Loaded {} previous bets from history (dedup protection, {} base matches)",
                already_bet_matches.len(), already_bet_base_matches.len());
        }
    }

    // === PENDING CLAIMS: persist token IDs for bets waiting to be claimed ===
    let pending_claims_path = "data/pending_claims.txt";
    // Format per line: tokenId|betId|matchKey|valueTeam|amountUsd|odds|timestamp
    // Load on startup → add to active_bets for auto-claim monitoring
    if Path::new(pending_claims_path).exists() {
        if let Ok(contents) = std::fs::read_to_string(pending_claims_path) {
            let mut seen_bet_ids: HashSet<String> = HashSet::new();
            for line in contents.lines() {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 6 {
                    let token_id_raw = parts[0].to_string();
                    let bet_id = parts[1].to_string();
                    // BUG #12 FIX: Skip duplicate bet_ids
                    if seen_bet_ids.contains(&bet_id) {
                        info!("⏭️ Skipping duplicate pending claim: betId={}", bet_id);
                        continue;
                    }
                    seen_bet_ids.insert(bet_id.clone());
                    let match_key = parts[2].to_string();
                    let value_team = parts[3].to_string();
                    let amount_usd: f64 = parts[4].parse().unwrap_or(2.0);
                    let odds: f64 = parts[5].parse().unwrap_or(1.5);
                    // "?" means tokenId not yet discovered — set to None so PATH B will discover it
                    // Also treat tokenId < 1000 as bogus (false positive from recursive extraction)
                    let token_id = if token_id_raw == "?" || token_id_raw.is_empty() {
                        None
                    } else if let Ok(tid_num) = token_id_raw.parse::<u64>() {
                        if tid_num < 1000 {
                            info!("⚠️ Bogus tokenId {} for bet {} — treating as undiscovered", token_id_raw, bet_id);
                            None
                        } else {
                            Some(token_id_raw)
                        }
                    } else {
                        Some(token_id_raw)
                    };
                    active_bets.push(ActiveBet {
                        alert_id: 0,
                        bet_id: bet_id.clone(),
                        match_key: match_key.clone(),
                        team1: value_team.clone(),
                        team2: "?".to_string(),
                        value_team: value_team.clone(),
                        amount_usd,
                        odds,
                        placed_at: "loaded".to_string(),
                        condition_id: String::new(),
                        outcome_id: String::new(),
                        graph_bet_id: None,
                        token_id,
                    });
                }
            }
            info!("📋 Loaded {} pending claims from file", active_bets.len());
        }
    }

    // If no chat_id, wait for user to send /start
    if chat_id.is_none() {
        info!("No TELEGRAM_CHAT_ID set. Waiting for /start message from user...");
        info!("Open Telegram and send /start to your bot");

        loop {
            match tg_get_updates(&client, &token, update_offset).await {
                Ok(updates) => {
                    for u in &updates.result {
                        update_offset = u.update_id + 1;
                        if let Some(msg) = &u.message {
                            let text = msg.text.as_deref().unwrap_or("");
                            if text.starts_with("/start") {
                                chat_id = Some(msg.chat.id);
                                info!("Chat ID discovered: {}", msg.chat.id);
                                tg_send_message(&client, &token, msg.chat.id,
                                    &format!(
                                        "🤖 <b>RustMisko Alert Bot v3</b> activated!\n\n\
                                         Automatický CS2 Azuro betting system.\n\
                                         Alert → Reply → BET → AUTO-CASHOUT.\n\n\
                                         ⚙️ Min edge: 5%\n\
                                         📡 Polling: 30s\n\
                                         🏠 Feed Hub: {}\n\
                                         🔧 Executor: {}", feed_hub_url, executor_url
                                    )
                                ).await?;
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!("getUpdates error: {}", e),
            }
            if chat_id.is_some() { break; }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    let chat_id = chat_id.unwrap();
    info!("Alert bot running. chat_id={}, feed_hub={}, executor={}", chat_id, feed_hub_url, executor_url);

    // Check executor health at startup
    let executor_status = match client.get(format!("{}/health", executor_url)).send().await {
        Ok(resp) => {
            match resp.json::<ExecutorHealthResponse>().await {
                Ok(h) => {
                    let wallet = h.wallet.as_deref().unwrap_or("?");
                    let balance = h.balance.as_deref().unwrap_or("?");
                    let allowance = h.relayer_allowance.as_deref().unwrap_or("?");
                    // Update bankroll from executor balance
                    if let Ok(bal) = balance.parse::<f64>() {
                        current_bankroll = bal;
                        // Only set SOD from executor if NOT already loaded from daily_pnl.json
                        // (mid-day restart: file has the real SOD, executor has current depleted balance)
                        if !sod_loaded_from_file {
                            start_of_day_bankroll = bal;
                            info!("💰 Bankroll set from executor: ${:.2} (SOD locked)", current_bankroll);
                        } else {
                            info!("💰 Bankroll from executor: ${:.2} (SOD kept from file: ${:.2})", bal, start_of_day_bankroll);
                        }
                    }
                    format!("✅ Executor ONLINE\n   Wallet: <code>{}</code>\n   Balance: {} USDT\n   Allowance: {}", wallet, balance, allowance)
                }
                Err(_) => "⚠️ Executor odpověděl, ale nevalidní JSON".to_string(),
            }
        }
        Err(_) => "❌ Executor OFFLINE — sázky nebudou fungovat!\n   Spusť: cd executor && node index.js".to_string(),
    };

    // Startup message
    let session_limit_str = "∞ (UNLIMITED)".to_string();
    let auto_bet_info = if AUTO_BET_ENABLED {
        format!("🤖 <b>AUTO-BET v5: ON</b>\n   \
                 CS2/Esports: map_winner, edge ≥12%\n   \
                 Tennis: match_winner, edge ≥15% (set_diff≥1)\n   \
                 Basketball: match_winner, edge ≥12%, stake 0.5x\n   \
                 Football: match_winner, edge ≥18% (goal_diff≥2)\n   \
                 Base stake: ${:.0} | Odds {:.2}-{:.2}\n   \
                 Daily loss limit: ${:.0} | Watchdog: {}s\n\
                 💰 <b>AUTO-CLAIM: ON</b> (každých {}s)",
                AUTO_BET_STAKE_USD, AUTO_BET_MIN_ODDS, AUTO_BET_MAX_ODDS,
                DAILY_LOSS_LIMIT_USD, WATCHDOG_TIMEOUT_SECS,
                CLAIM_CHECK_SECS)
    } else {
        "🔒 AUTO-BET: OFF (manuální YES/NO)".to_string()
    };

    tg_send_message(&client, &token, chat_id,
        &format!(
            "🟢 <b>Alert Bot v3 Online</b>\n\n\
             {}\n\n\
             {}\n\n\
             Monitoruji Azuro vs HLTV score.\n\
             Score Edge → AUTO-BET (HIGH) / Alert (MEDIUM).\n\n\
             /status — stav systému + executor + bety\n\
             /odds — aktuální anomálie\n\
             /bets — aktivní sázky\n\
             /help — nápověda", executor_status, auto_bet_info
        )
    ).await?;

    let mut poll_ticker = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    let mut cashout_ticker = tokio::time::interval(Duration::from_secs(CASHOUT_CHECK_SECS));
    let mut claim_ticker = tokio::time::interval(Duration::from_secs(CLAIM_CHECK_SECS));
    let mut portfolio_ticker = tokio::time::interval(Duration::from_secs(PORTFOLIO_REPORT_SECS));
    let mut tg_ticker = tokio::time::interval(Duration::from_secs(3));
    // Bets that have been settled and claimed (to avoid re-processing)
    let mut settled_bet_ids: HashSet<String> = HashSet::new();
    // Running profit/loss tracker
    let mut total_wagered: f64 = 0.0;
    let mut total_returned: f64 = 0.0;
    // Safety net counter for /auto-claim (every 5th claim tick = ~5 min)
    let mut claim_safety_counter: u32 = 0;
    // Session start time for portfolio reporting
    let session_start = Utc::now();

    loop {
        tokio::select! {
            // === POLL feed-hub for anomalies ===
            _ = poll_ticker.tick() => {
                // Clean old alerts from cooldown
                let now = Utc::now();
                sent_alerts.retain(|a| (now - a.sent_at).num_seconds() < ALERT_COOLDOWN_SECS);

                let already_alerted: HashSet<String> = sent_alerts.iter()
                    .map(|a| a.match_key.clone()).collect();

                // === WATCHDOG: check for feed-hub timeout ===
                if last_good_data.elapsed().as_secs() > WATCHDOG_TIMEOUT_SECS && !safe_mode {
                    safe_mode = true;
                    let elapsed = last_good_data.elapsed().as_secs();
                    warn!("⚠️ SAFE MODE: Feed-hub silent for {}s > {}s threshold", elapsed, WATCHDOG_TIMEOUT_SECS);
                    let _ = tg_send_message(&client, &token, chat_id,
                        &format!("⚠️ <b>SAFE MODE ACTIVATED</b>\n\nFeed-hub neodpovídá {}s.\nAuto-bety POZASTAVENY.\nAlerty stále fungují.\n\nZkontroluj Chrome tab + Tampermonkey.", elapsed)
                    ).await;
                    log_event("SAFE_MODE_ON", &serde_json::json!({"elapsed_secs": elapsed}));
                }

                // 1. Check /state for cross-bookmaker odds anomalies
                match client.get(format!("{}/state", feed_hub_url)).send().await {
                    Ok(resp) => {
                        match resp.json::<StateResponse>().await {
                            Ok(state) => {
                                // === WATCHDOG: feed-hub is alive ===
                                last_good_data = std::time::Instant::now();
                                if safe_mode {
                                    safe_mode = false;
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        "✅ Feed-hub ONLINE. Auto-bety obnoveny.").await;
                                    log_event("SAFE_MODE_OFF", &serde_json::json!({}));
                                }

                                // === DAILY DATE RESET (midnight UTC) ===
                                let today_now = Utc::now().format("%Y-%m-%d").to_string();
                                if today_now != daily_date {
                                    log_event("DAILY_RESET", &serde_json::json!({
                                        "date": daily_date,
                                        "wagered": daily_wagered,
                                        "returned": daily_returned,
                                        "pnl": daily_returned - daily_wagered,
                                    }));
                                    info!("📅 New day {} — resetting daily P&L (yesterday net={:.2})",
                                        today_now, daily_returned - daily_wagered);
                                    daily_wagered = 0.0;
                                    daily_returned = 0.0;
                                    daily_date = today_now;
                                    daily_loss_alert_sent = false;
                                    daily_loss_last_reminder = None;
                                    // Lock start-of-day bankroll for today's loss limit calc
                                    start_of_day_bankroll = current_bankroll;
                                    info!("📅 SOD bankroll locked: ${:.2}", start_of_day_bankroll);
                                    // Persist SOD bankroll for day-rollover
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                    }
                                    // === RESET EXPOSURE + REBET TRACKERS ===
                                    condition_exposure.clear();
                                    match_exposure.clear();
                                    rebet_tracker.clear();
                                    sport_exposure.clear();
                                    resync_freeze.clear();
                                    inflight_wagered_total = 0.0;
                                    info!("📅 Cleared condition_exposure, match_exposure, rebet_tracker, sport_exposure, resync_freeze for new day");
                                }

                                // === DAILY LOSS CAP NOTIFICATION ===
                                // NET loss = settled losses minus claimed returns
                                // e.g. wagered=$20 on losses, returned=$30 from wins => net = -$10 (profit!)
                                let daily_net_loss = (daily_wagered - daily_returned).max(0.0);
                                // Effective daily limit: min(hard $30, tier-based cap)
                                // Uses START-OF-DAY bankroll to prevent "shrinking box" during losing streaks
                                let effective_daily_limit = {
                                    let (_, _, _, dl_frac, _) = get_exposure_caps(start_of_day_bankroll);
                                    DAILY_LOSS_LIMIT_USD.min(start_of_day_bankroll * dl_frac)
                                };
                                // OBSERVABILITY: log daily loss evaluation every cycle
                                debug!("📊 DAILY_LOSS_EVAL: net_loss=${:.2} limit=${:.2} sod_br=${:.2} cur_br=${:.2} wagered=${:.2} returned=${:.2}",
                                    daily_net_loss, effective_daily_limit, start_of_day_bankroll, current_bankroll, daily_wagered, daily_returned);
                                if daily_net_loss >= effective_daily_limit {
                                    let now_utc = Utc::now();
                                    let reminder_due = daily_loss_last_reminder
                                        .map(|ts| (now_utc - ts).num_seconds() >= DAILY_LOSS_REMINDER_SECS)
                                        .unwrap_or(true);

                                    if !daily_loss_alert_sent || reminder_due {
                                        let msg = format!(
                                            "🛑 <b>DAILY LOSS LIMIT HIT</b>\n\nDnešní NET loss: <b>${:.2}</b> (wagered ${:.2} - returned ${:.2})\nLimit: <b>${:.2}</b> (min of ${:.0} hard, {:.0}% SOD BR=${:.0})\n\n🤖 Auto-bety jsou pozastavené do dalšího dne nebo ručního resetu.\n📡 Monitoring + alerty jedou dál.",
                                            daily_net_loss,
                                            daily_wagered,
                                            daily_returned,
                                            effective_daily_limit,
                                            DAILY_LOSS_LIMIT_USD,
                                            get_exposure_caps(start_of_day_bankroll).3 * 100.0,
                                            start_of_day_bankroll,
                                        );
                                        let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                                        daily_loss_alert_sent = true;
                                        daily_loss_last_reminder = Some(now_utc);
                                    }
                                }

                                // === 1. SCORE EDGE detection (primary strategy!) ===
                                let score_edges = find_score_edges(&state, &mut score_tracker, &mut resync_freeze);
                                let mut sent_score_edges = 0usize;
                                for edge in &score_edges {
                                    let alert_key = format!("score:{}:{}:{}-{}", edge.match_key, edge.leading_side, edge.score1, edge.score2);
                                    if already_alerted.contains(&alert_key) {
                                        continue;
                                    }

                                    alert_counter += 1;
                                    let aid = alert_counter;

                                    // Store as OddsAnomaly for YES/BET compatibility
                                    let anomaly = OddsAnomaly {
                                        detected_at: Utc::now(),
                                        match_key: edge.match_key.clone(),
                                        team1: edge.team1.clone(),
                                        team2: edge.team2.clone(),
                                        azuro_w1: edge.azuro_w1,
                                        azuro_w2: edge.azuro_w2,
                                        azuro_bookmaker: edge.azuro_bookmaker.clone(),
                                        azuro_url: edge.azuro_url.clone(),
                                        market_w1: 0.0,
                                        market_w2: 0.0,
                                        market_bookmaker: "score-edge".to_string(),
                                        value_side: edge.leading_side,
                                        discrepancy_pct: edge.edge_pct,
                                        confidence: edge.confidence,
                                        confidence_reasons: vec![format!("Score {}-{} → edge {:.1}%", edge.score1, edge.score2, edge.edge_pct)],
                                        teams_swapped: false,
                                        is_live: true,
                                        live_score: Some(format!("{}-{}", edge.score1, edge.score2)),
                                        game_id: edge.game_id.clone(),
                                        condition_id: edge.condition_id.clone(),
                                        outcome1_id: edge.outcome1_id.clone(),
                                        outcome2_id: edge.outcome2_id.clone(),
                                        outcome_id: edge.outcome_id.clone(),
                                        chain: edge.chain.clone(),
                                    };

                                    let azuro_odds = if edge.leading_side == 1 { edge.azuro_w1 } else { edge.azuro_w2 };
                                    let leading_team = if edge.leading_side == 1 { &edge.team1 } else { &edge.team2 };

                                    // === AUTO-BET: place bet automatically on high-confidence edges ===
                                    let cond_id_str = anomaly.condition_id.as_deref().unwrap_or("").to_string();
                                    let match_key_for_bet = edge.match_key.clone();
                                    let base_match_key = strip_map_winner_suffix(&match_key_for_bet);

                                    // === RE-BET LOGIC: check if we already bet, and if re-bet is allowed ===
                                    let is_inflight = (!cond_id_str.is_empty() && inflight_conditions.contains(&cond_id_str))
                                        || inflight_conditions.contains(&match_key_for_bet);

                                    // BUG FIX: Also check base_match_key to prevent triple exposure
                                    // (e.g. match_winner + map2_winner + map3_winner on same match = $9 loss)
                                    let base_already_bet = already_bet_base_matches.contains(&base_match_key)
                                        && match_key_for_bet != base_match_key; // only block map-variants, not the base itself re-checking

                                    let (already_bet_this, rebet_ok) = if is_inflight {
                                        (true, false) // In-flight → always block
                                    } else if base_already_bet {
                                        // Base match already has a bet → block all map-winner variants
                                        info!("🛡️ BASE-MATCH DEDUP: {} blocked (base {} already bet)",
                                            match_key_for_bet, base_match_key);
                                        (true, false)
                                    } else if (!cond_id_str.is_empty() && already_bet_conditions.contains(&cond_id_str))
                                        || already_bet_matches.contains(&match_key_for_bet) {
                                        // Already bet → check if re-bet is allowed (only when FF enabled)
                                        let can_rebet = FF_REBET_ENABLED && !cond_id_str.is_empty() && {
                                            let cond_exp_rb = condition_exposure.get(&cond_id_str).copied().unwrap_or(0.0);
                                            let match_exp_rb = match_exposure.get(&base_match_key).copied().unwrap_or(0.0);
                                            let (_, cond_frac, match_frac, _, _) = get_exposure_caps(current_bankroll);
                                            let cond_cap_left = (current_bankroll * cond_frac - cond_exp_rb).max(0.0);
                                            let match_cap_left = (current_bankroll * match_frac - match_exp_rb).max(0.0);
                                            if let Some(rb_state) = rebet_tracker.get(&cond_id_str) {
                                                rebet_allowed(rb_state, edge.confidence, edge.edge_pct, cond_cap_left, match_cap_left)
                                            } else { false }
                                        };
                                        if can_rebet {
                                            info!("🔄 RE-BET ALLOWED: {} cond={} (tier upgrade or edge jump)",
                                                match_key_for_bet, cond_id_str);
                                        }
                                        (!can_rebet, can_rebet)
                                    } else {
                                        (false, false) // Never bet → fresh bet
                                    };

                                    if already_bet_this && !rebet_ok {
                                        info!("🚫 DEDUP: Already bet on {} (base={}, cond={}, inflight={}), skipping auto-bet",
                                            match_key_for_bet, base_match_key, cond_id_str, is_inflight);
                                    }

                                    // === SPORT-SPECIFIC AUTO-BET CONFIG ===
                                    let sport = edge.match_key.split("::").next().unwrap_or("?");
                                    let (sport_auto_allowed, sport_min_edge, sport_multiplier, preferred_market) = get_sport_config(sport);
                                    // Dynamic base stake: bankroll-scaled instead of hardcoded $3
                                    let base_stake = dynamic_base_stake(current_bankroll, sport);
                                    let raw_stake = base_stake * sport_multiplier;

                                    // === EXPOSURE CAPS + STAKE TRIMMER ===
                                    let cond_exp = condition_exposure.get(&cond_id_str).copied().unwrap_or(0.0);
                                    let match_exp = match_exposure.get(&base_match_key).copied().unwrap_or(0.0);
                                    let sport_exp = sport_exposure.get(sport).copied().unwrap_or(0.0);
                                    let daily_net_loss_for_cap = (daily_wagered - daily_returned).max(0.0);
                                    let cv_sm = edge.cv_stake_mult;
                                    let stake = trim_stake(raw_stake, current_bankroll, cond_exp, match_exp, daily_net_loss_for_cap,
                                        inflight_wagered_total, sport_exp, sport, cv_sm, start_of_day_bankroll);
                                    if stake < 0.50 && raw_stake >= 0.50 {
                                        info!("🛡️ EXPOSURE CAP: {} stake trimmed from ${:.2} to $0 (bank=${:.0} cond_exp=${:.2} match_exp=${:.2} daily_loss=${:.2})",
                                            match_key_for_bet, raw_stake, current_bankroll, cond_exp, match_exp, daily_net_loss_for_cap);
                                    }

                                    let generic_esports_blocked = BLOCK_GENERIC_ESPORTS_BETS && edge.match_key.starts_with("esports::");
                                    let is_preferred_market = match preferred_market {
                                        "map_winner" => edge.match_key.contains("::map"),
                                        "set_winner" => edge.match_key.contains("::set"),
                                        "match_or_map" => true,
                                        "match_winner" => true,
                                        _ => false,
                                    };

                                    // Check daily NET LOSS limit (settled losses minus claimed returns)
                                    // This prevents oracle lag from blocking us when we're actually in profit
                                    let daily_net_loss = (daily_wagered - daily_returned).max(0.0);
                                    let within_daily_limit = daily_net_loss < effective_daily_limit;

                                    // Sport-specific safety guard
                                    let sport_guard_ok = match sport {
                                        "tennis" => {
                                            // Only auto-bet when there's >= 1 set lead
                                            let set_diff = (edge.score1 - edge.score2).abs();
                                            set_diff >= 1
                                        }
                                        "football" => {
                                            // Only auto-bet when goal_diff >= 2
                                            let goal_diff = (edge.score1 - edge.score2).abs();
                                            goal_diff >= 2
                                        }
                                        // ESPORTS guard: BLOCK match_winner auto-bet on map-level scores!
                                        // In Bo3 (CS2/LoL/Dota), score 1-0 means won your map pick → no real edge.
                                        // Only map_winner bets with ROUND-level edges are reliable.
                                        // Match_winner is only allowed on ROUND-level edges (score > 3).
                                        "cs2" | "esports" | "lol" | "dota-2" | "valorant" | "league-of-legends" => {
                                            let is_map_winner_bet = edge.match_key.contains("::map");
                                            if is_map_winner_bet {
                                                true // map_winner bets (round-level edges) are always OK
                                            } else {
                                                // match_winner: only allow if score is round-level (max > 3)
                                                let max_s = edge.score1.max(edge.score2);
                                                if max_s <= 3 {
                                                    info!("🛡️ ESPORTS GUARD: {} score {}-{} is MAP-LEVEL — blocking match_winner auto-bet (use map_winner instead)",
                                                        edge.match_key, edge.score1, edge.score2);
                                                    false
                                                } else {
                                                    true // round-level scores (e.g. 10-4) → OK for match_winner
                                                }
                                            }
                                        }
                                        "basketball" => {
                                            // Basketball has high scores (80-120+ total).
                                            // Only auto-bet when game is well underway AND lead is significant.
                                            let total_points = edge.score1 + edge.score2;
                                            let point_diff = (edge.score1 - edge.score2).abs();
                                            if total_points < 80 {
                                                info!("🏀 BASKETBALL GUARD: {} score {}-{} total={} < 80 — too early, blocking auto-bet",
                                                    edge.match_key, edge.score1, edge.score2, total_points);
                                                false
                                            } else if point_diff < 20 {
                                                info!("🏀 BASKETBALL GUARD: {} score {}-{} diff={} < 20 — lead not significant enough, blocking auto-bet",
                                                    edge.match_key, edge.score1, edge.score2, point_diff);
                                                false
                                            } else {
                                                true // 80+ total points AND 20+ point lead → OK
                                            }
                                        }
                                        _ => true, // other sports: no extra guard
                                    };

                                    // Dynamic odds cap: CS2 map_winner uses confidence-based cap, others use fixed cap
                                    let is_map_winner_edge = edge.match_key.contains("::map");
                                    let effective_max_odds = if let Some(tier) = edge.cs2_map_confidence {
                                        cs2_dynamic_max_odds(tier)
                                    } else if is_map_winner_edge {
                                        AUTO_BET_MAX_ODDS_CS2_MAP // fallback for non-CS2 map winners
                                    } else {
                                        AUTO_BET_MAX_ODDS
                                    };

                                    // CRITICAL: Identical Azuro odds guard (score-edge path)
                                    // When oracle hasn't set real prices (e.g. basketball 1.84/1.84),
                                    // score-edge sees phantom 38%+ edge. Block auto-bet, allow alert.
                                    let azuro_odds_identical_se = (edge.azuro_w1 - edge.azuro_w2).abs() < 0.02;
                                    if azuro_odds_identical_se && edge.confidence == "HIGH" {
                                        info!("🛡️ IDENTICAL ODDS GUARD (score-edge): {} azuro={:.2}/{:.2} — oracle bug, blocking auto-bet (alert only)",
                                            edge.match_key, edge.azuro_w1, edge.azuro_w2);
                                    }

                                    let should_auto_bet = AUTO_BET_ENABLED
                                        && sport_auto_allowed
                                        && is_preferred_market
                                        && sport_guard_ok
                                        && within_daily_limit
                                        && !safe_mode
                                        && edge.confidence == "HIGH"
                                        && edge.edge_pct >= sport_min_edge
                                        && azuro_odds >= AUTO_BET_MIN_ODDS
                                        && azuro_odds <= effective_max_odds
                                        && !azuro_odds_identical_se // BUG FIX: block phantom edge from identical oracle odds
                                        && anomaly.condition_id.is_some()
                                        && anomaly.outcome_id.is_some()
                                        && !generic_esports_blocked
                                        && (!already_bet_this || rebet_ok) // RE-BET: allow if re-bet conditions met
                                        && stake >= 0.50; // EXPOSURE CAP: stake trimmer didn't zero it out

                                    if !sport_auto_allowed && edge.confidence == "HIGH" {
                                        info!("📢 {} ALERT ONLY (auto-bet disabled for {})", edge.match_key, sport);
                                    }
                                    if !within_daily_limit {
                                        info!("🛑 DAILY LOSS LIMIT: net losses={:.2} >= {:.2} (effective), skipping auto-bet", daily_net_loss, effective_daily_limit);
                                    }
                                    if !is_preferred_market && sport_auto_allowed {
                                        info!("🛡️ MARKET GUARD: {} needs {} but got match_winner — alert only", edge.match_key, preferred_market);
                                    }
                                    if !sport_guard_ok && sport_auto_allowed && edge.confidence == "HIGH" {
                                        info!("🛡️ SPORT GUARD: {} ({}): score {}-{} doesn't meet safety threshold — alert only",
                                            edge.match_key, sport, edge.score1, edge.score2);
                                    }
                                    if generic_esports_blocked {
                                        info!("🛡️ REALITY GUARD: {} uses generic esports:: key — auto-bet blocked", edge.match_key);
                                    }
                                    // DEBUG: log ALL reasons when auto-bet is blocked but edge is high
                                    if !should_auto_bet && edge.confidence == "HIGH" && edge.edge_pct >= 10.0 {
                                        info!("🔍 AUTO-BET BLOCKED for {} edge={:.1}%: enabled={} sport_ok={} market_ok={} guard_ok={} daily_ok={} safe={} conf={} min_edge={:.1} odds={:.2} min={:.2} max={:.2} cond={} out={} dedup={} esports_block={}",
                                            edge.match_key, edge.edge_pct,
                                            AUTO_BET_ENABLED, sport_auto_allowed, is_preferred_market, sport_guard_ok,
                                            within_daily_limit, safe_mode, edge.confidence, sport_min_edge,
                                            azuro_odds, AUTO_BET_MIN_ODDS, effective_max_odds,
                                            anomaly.condition_id.is_some(), anomaly.outcome_id.is_some(),
                                            already_bet_this, generic_esports_blocked);
                                    }

                                    let mut score_alert_sent = false;

                                    if should_auto_bet {
                                        // AUTO-BET with sport-specific stake (set above)
                                        let condition_id = anomaly.condition_id.as_ref().unwrap().clone();
                                        let outcome_id = anomaly.outcome_id.as_ref().unwrap().clone();

                                        // RACE CONDITION FIX: mark in-flight BEFORE sending to executor
                                        inflight_conditions.insert(cond_id_str.clone());
                                        inflight_conditions.insert(match_key_for_bet.clone());

                                        info!("🤖 AUTO-BET #{}: {} @ {:.2} ${:.2} edge={:.1}%",
                                            aid, leading_team, azuro_odds, stake, edge.edge_pct);

                                        // Send alert WITH auto-bet notice
                                        let sport_label = edge.match_key.split("::").next().unwrap_or("?").to_uppercase();
                                        let msg = format!(
                                            "🤖 <b>#{} AUTO-BET</b> 🟢 HIGH\n\
                                             🏷️ <b>{}</b>\n\
                                             \n\
                                             <b>{}</b> vs <b>{}</b>\n\
                                             🔴 LIVE: <b>{}-{}</b> (bylo {}-{})\n\
                                             \n\
                                             📊 <b>{}</b> VEDE!\n\
                                             ⚡ EDGE: <b>{:.1}%</b>\n\
                                             🎯 Kurz: <b>{:.2}</b>\n\
                                             💰 Stake: <b>${:.2}</b>\n\
                                             \n\
                                             ⏳ Automaticky sázím...",
                                            aid,
                                            sport_label,
                                            edge.team1, edge.team2,
                                            edge.score1, edge.score2, edge.prev_score1, edge.prev_score2,
                                            leading_team,
                                            edge.edge_pct,
                                            azuro_odds,
                                            stake
                                        );
                                        if let Err(e) = tg_send_message(&client, &token, chat_id, &msg).await {
                                            error!("Failed to send auto-bet pre-alert: {}", e);
                                        } else {
                                            score_alert_sent = true;
                                        }

                                        // Place the bet — with retry on "condition not active"
                                        let decision_instant = std::time::Instant::now();
                                        let min_odds = (azuro_odds * min_odds_factor_for_match(&match_key_for_bet) * 1e12) as u64;
                                        let amount_raw = (stake * 1e6) as u64;
                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "team1": edge.team1,
                                            "team2": edge.team2,
                                            "valueTeam": leading_team,
                                        });

                                        // Retry loop: Azuro pauses conditions during score events
                                        // (set/game point in tennis, goal in football). We retry twice
                                        // after 5s each in case the condition re-activates.
                                        let max_retries = AUTO_BET_RETRY_MAX;
                                        let mut attempt = 0;
                                        let mut bet_success = false;
                                        loop {
                                        // Signal TTL check — abort if decision is stale
                                        if decision_instant.elapsed() > std::time::Duration::from_secs(SIGNAL_TTL_SECS) {
                                            warn!("⏰ AUTO-BET #{}: Signal TTL expired ({}ms elapsed) — aborting stale bet",
                                                aid, decision_instant.elapsed().as_millis());
                                            inflight_conditions.remove(&cond_id_str);
                                            inflight_conditions.remove(&match_key_for_bet);
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!("⏰ <b>AUTO-BET #{} TTL EXPIRED</b> ({}ms) — sázka zastaralá, zrušeno.",
                                                    aid, decision_instant.elapsed().as_millis())
                                            ).await;
                                            break;
                                        }
                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                match resp.json::<ExecutorBetResponse>().await {
                                                    Ok(br) => {
                                                        let is_rejected = br.state.as_deref()
                                                            .map(|s| s == "Rejected" || s == "Failed" || s == "Cancelled")
                                                            .unwrap_or(false);
                                                        if let Some(err) = &br.error {
                                                            // Check if retryable (condition paused/not active)
                                                            let err_lower = err.to_lowercase();
                                                            let is_condition_paused = err_lower.contains("not active")
                                                                || err_lower.contains("paused")
                                                                || err_lower.contains("not exist");
                                                            let is_fatal = err_lower.contains("insufficient")
                                                                || err_lower.contains("allowance")
                                                                || err_lower.contains("revert")
                                                                || err_lower.contains("nonce");
                                                            if is_condition_paused && !is_fatal && attempt < max_retries {
                                                                attempt += 1;
                                                                let delay_ms = AUTO_BET_RETRY_DELAYS_MS.get(attempt.saturating_sub(1)).copied().unwrap_or(400);
                                                                info!("🔄 AUTO-BET #{} retry {}/{}: condition paused, waiting {}ms... ({})",
                                                                    aid, attempt, max_retries, delay_ms, err);
                                                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                                                continue; // retry the loop
                                                            }
                                                            error!("❌ AUTO-BET #{} FAILED: {} (cond={}, outcome={}, match={})",
                                                                aid, err,
                                                                &condition_id,
                                                                &outcome_id,
                                                                match_key_for_bet);
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>AUTO-BET #{} FAILED</b>\n\nError: {}\nCondition: {}\nMatch: {}\nRetries: {}",
                                                                    aid, err,
                                                                    &condition_id,
                                                                    match_key_for_bet,
                                                                    attempt)
                                                            ).await;
                                                            // Remove from inflight so we can retry on next edge
                                                            inflight_conditions.remove(&cond_id_str);
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break; // exit retry loop
                                                        } else if is_rejected {
                                                            error!("❌ AUTO-BET #{} REJECTED: state={} (cond={}, match={})",
                                                                aid, br.state.as_deref().unwrap_or("?"),
                                                                &condition_id,
                                                                match_key_for_bet);
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>AUTO-BET #{} REJECTED</b>\n\nState: {}\nCondition: {}\nMatch: {}\nCondition may be paused/resolved.",
                                                                    aid, br.state.as_deref().unwrap_or("?"),
                                                                    &condition_id,
                                                                    match_key_for_bet)
                                                            ).await;
                                                            // === LEDGER: REJECTED ===
                                                            ledger_write("REJECTED", &serde_json::json!({
                                                                "alert_id": aid, "match_key": match_key_for_bet,
                                                                "condition_id": condition_id,
                                                                "state": br.state, "path": "edge"
                                                            }));
                                                            // Remove from inflight so we can retry on next edge
                                                            inflight_conditions.remove(&cond_id_str);
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break; // exit retry loop
                                                        } else {
                                                            auto_bet_count += 1;
                                                            // daily_wagered += stake; // REMOVED: Only count settled losses
                                                            // Persist daily P&L
                                                            {
                                                                let today = Utc::now().format("%Y-%m-%d").to_string();
                                                                let _ = std::fs::write(bet_count_path, format!("{}|{}", today, auto_bet_count));
                                                                let _ = std::fs::write("data/daily_pnl.json",
                                                                    serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                                            }
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let bet_state = br.state.as_deref().unwrap_or("?");
                                                            let token_id_opt = sanitize_token_id(br.token_id.clone());
                                                            let graph_bet_id_opt = br.graph_bet_id.clone();

                                                            // === DEDUP: record bet to prevent duplicates ===
                                                            already_bet_matches.insert(match_key_for_bet.clone());
                                                            already_bet_conditions.insert(cond_id_str.clone());
                                                            // BUG #1 FIX: Also record base match key
                                                            already_bet_base_matches.insert(base_match_key.clone());

                                                            // === EXPOSURE TRACKING: update condition + match + sport + inflight ===
                                                            *condition_exposure.entry(cond_id_str.clone()).or_insert(0.0) += stake;
                                                            *match_exposure.entry(base_match_key.clone()).or_insert(0.0) += stake;
                                                            *sport_exposure.entry(sport.to_string()).or_insert(0.0) += stake;
                                                            inflight_wagered_total += stake;

                                                            // === RE-BET TRACKING: update or create state ===
                                                            if let Some(rb) = rebet_tracker.get_mut(&cond_id_str) {
                                                                rb.bet_count += 1;
                                                                rb.highest_tier = edge.confidence.to_string();
                                                                rb.last_edge_pct = edge.edge_pct;
                                                                rb.last_bet_at = Utc::now();
                                                                rb.total_wagered += stake;
                                                                info!("🔄 RE-BET #{}: {} total bets on cond={}, total wagered=${:.2}",
                                                                    rb.bet_count, match_key_for_bet, cond_id_str, rb.total_wagered);
                                                            } else {
                                                                rebet_tracker.insert(cond_id_str.clone(),
                                                                    ReBetState::new(edge.confidence, edge.edge_pct, stake));
                                                            }

                                                            // Remove from inflight (bet is now in persistent dedup)
                                                            inflight_conditions.remove(&cond_id_str);
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            // Persist to file
                                                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                .create(true).append(true)
                                                                .open(bet_history_path) {
                                                                use std::io::Write;
                                                                let _ = writeln!(f, "{}|{}|{}|{}|{}",
                                                                    match_key_for_bet, cond_id_str,
                                                                    leading_team, azuro_odds, Utc::now().to_rfc3339());
                                                            }

                                                            let is_dry_run = bet_state == "DRY-RUN" || bet_id.starts_with("dry-");
                                                            if !is_dry_run {
                                                                active_bets.push(ActiveBet {
                                                                    alert_id: aid,
                                                                    bet_id: bet_id.to_string(),
                                                                    match_key: edge.match_key.clone(),
                                                                    team1: edge.team1.clone(),
                                                                    team2: edge.team2.clone(),
                                                                    value_team: leading_team.to_string(),
                                                                    amount_usd: stake,
                                                                    odds: azuro_odds,
                                                                    placed_at: Utc::now().to_rfc3339(),
                                                                    condition_id: condition_id.clone(),
                                                                    outcome_id: outcome_id.clone(),
                                                                    graph_bet_id: graph_bet_id_opt.clone(),
                                                                    token_id: token_id_opt.clone(),
                                                                });
                                                                // Persist pending claim (prefer real tokenId from executor)
                                                                let token_to_write = token_id_opt.as_deref().unwrap_or("?");
                                                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                    .create(true).append(true)
                                                                    .open(pending_claims_path) {
                                                                    use std::io::Write;
                                                                    let _ = writeln!(f, "{}|{}|{}|{}|{}|{}|{}",
                                                                        token_to_write,
                                                                        bet_id, edge.match_key,
                                                                        leading_team, stake, azuro_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                                // === LEDGER: BET PLACED ===
                                                                ledger_write("PLACED", &serde_json::json!({
                                                                    "alert_id": aid, "bet_id": bet_id,
                                                                    "match_key": edge.match_key,
                                                                    "team1": edge.team1, "team2": edge.team2,
                                                                    "value_team": leading_team,
                                                                    "amount_usd": stake, "odds": azuro_odds,
                                                                    "condition_id": condition_id,
                                                                    "outcome_id": outcome_id,
                                                                    "token_id": token_id_opt,
                                                                    "graph_bet_id": graph_bet_id_opt,
                                                                    "path": "edge",
                                                                    "edge_pct": edge.edge_pct,
                                                                    "cv_stake_mult": cv_sm,
                                                                    "flags": {
                                                                        "FF_EXPOSURE_CAPS": FF_EXPOSURE_CAPS,
                                                                        "FF_REBET_ENABLED": FF_REBET_ENABLED,
                                                                        "FF_CROSS_VALIDATION": FF_CROSS_VALIDATION,
                                                                        "FF_CASHOUT_ENABLED": FF_CASHOUT_ENABLED,
                                                                        "FF_INFLIGHT_CAP": FF_INFLIGHT_CAP,
                                                                        "FF_PER_SPORT_CAP": FF_PER_SPORT_CAP,
                                                                        "FF_RESYNC_FREEZE": FF_RESYNC_FREEZE,
                                                                    }
                                                                }));
                                                            }

                                                            let lim_s = "∞".to_string();
                                                            let result_msg = if is_dry_run {
                                                                format!("🧪 <b>AUTO-BET #{} DRY-RUN</b>\n{} @ {:.2} ${:.2}\n⚠️ Nebyl odeslán on-chain.", aid, leading_team, azuro_odds, stake)
                                                            } else {
                                                                format!(
                                                                    "✅ <b>AUTO-BET #{} PLACED!</b>\n\
                                                                     {} @ {:.2} | ${:.2}\n\
                                                                     Bet ID: <code>{}</code>\n\
                                                                     State: {}\n\
                                                                     Auto-bets dnes: {}/{}",
                                                                    aid, leading_team, azuro_odds, stake,
                                                                    bet_id, bet_state,
                                                                    auto_bet_count, lim_s
                                                                )
                                                            };
                                                            let _ = tg_send_message(&client, &token, chat_id, &result_msg).await;
                                                            bet_success = true;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        // Remove from inflight on parse error too
                                                        inflight_conditions.remove(&cond_id_str);
                                                        inflight_conditions.remove(&match_key_for_bet);
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            &format!("❌ Auto-bet #{} response error: {}", aid, e)
                                                        ).await;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                // Remove from inflight on executor error
                                                inflight_conditions.remove(&cond_id_str);
                                                inflight_conditions.remove(&match_key_for_bet);
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("❌ Executor offline pro auto-bet #{}: {}", aid, e)
                                                ).await;
                                            }
                                        }
                                        break; // exit retry loop (success, parse error, or executor offline)
                                        } // end retry loop
                                    } else if !mute_manual_alerts {
                                        // Manual alert (MEDIUM confidence or auto-bet disabled)
                                        let msg = format_score_edge_alert(edge, aid);
                                        match tg_send_message(&client, &token, chat_id, &msg).await {
                                            Ok(msg_id) => {
                                                score_alert_sent = true;
                                                msg_id_to_alert_id.insert(msg_id, aid);
                                            }
                                            Err(e) => {
                                                error!("Failed to send score edge alert: {}", e);
                                            }
                                        }
                                    } else {
                                        info!("🔇 MUTED manual score-edge alert #{}: {} edge={:.1}%",
                                            aid, edge.match_key, edge.edge_pct);
                                    }

                                    if score_alert_sent {
                                        info!("⚡ Score Edge #{} sent: {} {}-{} side={} edge={:.1}%",
                                            aid, edge.match_key, edge.score1, edge.score2, edge.leading_side, edge.edge_pct);
                                        sent_score_edges += 1;
                                        sent_alerts.push(SentAlert {
                                            match_key: alert_key,
                                            sent_at: Utc::now(),
                                        });
                                        alert_map.insert(aid, anomaly);
                                    } else {
                                        warn!("⚠️ Score Edge #{} NOT marked as sent (Telegram delivery failed)", aid);
                                    }
                                }

                                // === 2. Cross-book odds anomaly (secondary strategy) ===
                                let anomalies = find_odds_anomalies(&state);
                                let mut actually_sent = sent_score_edges;
                                let total_anomalies = anomalies.len();
                                for anomaly in anomalies {
                                    let alert_key = format!("{}:{}:{}", anomaly.match_key, anomaly.value_side, anomaly.azuro_bookmaker);
                                    if already_alerted.contains(&alert_key) {
                                        continue;
                                    }

                                    alert_counter += 1;
                                    let aid = alert_counter;

                                    let value_team = if anomaly.value_side == 1 {
                                        anomaly.team1.clone()
                                    } else {
                                        anomaly.team2.clone()
                                    };
                                    let azuro_odds = if anomaly.value_side == 1 {
                                        anomaly.azuro_w1
                                    } else {
                                        anomaly.azuro_w2
                                    };

                                    let market_source_count = anomaly.market_bookmaker
                                        .split('+')
                                        .filter(|s| !s.trim().is_empty())
                                        .count();

                                    let cond_id_str = anomaly.condition_id.as_deref().unwrap_or("").to_string();
                                    let match_key_for_bet = anomaly.match_key.clone();
                                    let base_match_key = strip_map_winner_suffix(&match_key_for_bet);
                                    // BUG FIX: Check base_match_key too → prevents triple exposure on same match
                                    let base_already_bet_anom = already_bet_base_matches.contains(&base_match_key)
                                        && match_key_for_bet != base_match_key;
                                    let already_bet_this = base_already_bet_anom
                                        || (!cond_id_str.is_empty() && already_bet_conditions.contains(&cond_id_str))
                                        || already_bet_matches.contains(&match_key_for_bet);
                                    if base_already_bet_anom {
                                        info!("🛡️ BASE-MATCH DEDUP (anomaly): {} blocked (base {} already bet)",
                                            match_key_for_bet, base_match_key);
                                    }

                                    // ENABLED: Odds anomaly auto-bet (ONLY for LIVE matches)
                                    // Prefer confirmation from multiple market sources.
                                    // Odds cap: CS2 map_winner → 3.00, everything else → 2.00
                                    let is_cs2_map = match_key_for_bet.starts_with("cs2::") && match_key_for_bet.contains("::map");
                                    let anomaly_max_odds = if is_cs2_map { AUTO_BET_MAX_ODDS_CS2_MAP } else { AUTO_BET_MAX_ODDS };
                                    let anomaly_odds_ok = azuro_odds <= anomaly_max_odds;

                                    // === EXPOSURE CAPS for odds anomaly ===
                                    let anomaly_cond_exp = condition_exposure.get(&cond_id_str).copied().unwrap_or(0.0);
                                    let anomaly_match_exp = match_exposure.get(&base_match_key).copied().unwrap_or(0.0);
                                    let anomaly_sport = match_key_for_bet.split("::").next().unwrap_or("?");
                                    let anomaly_sport_exp = sport_exposure.get(anomaly_sport).copied().unwrap_or(0.0);
                                    let anomaly_daily_loss = (daily_wagered - daily_returned).max(0.0);
                                    let anomaly_raw_stake = AUTO_BET_ODDS_ANOMALY_STAKE_USD;
                                    let anomaly_stake = trim_stake(anomaly_raw_stake, current_bankroll, anomaly_cond_exp, anomaly_match_exp, anomaly_daily_loss,
                                        inflight_wagered_total, anomaly_sport_exp, anomaly_sport, 1.0, start_of_day_bankroll);

                                    // SAFETY: block anomaly auto-bet when Azuro has identical odds (oracle bug)
                                    let azuro_odds_identical = (anomaly.azuro_w1 - anomaly.azuro_w2).abs() < 0.02;
                                    if azuro_odds_identical {
                                        info!("🛡️ IDENTICAL ODDS GUARD (anomaly): {} azuro={:.2}/{:.2} — phantom edge, blocking",
                                            anomaly.match_key, anomaly.azuro_w1, anomaly.azuro_w2);
                                    }

                                    // === SPORT-SPECIFIC ANOMALY GUARD ===
                                    // Score-edge path has sport_auto_bet_guard + model validation;
                                    // anomaly path is purely odds-comparison → needs stricter sport rules.
                                    let anomaly_sport_allowed = match anomaly_sport {
                                        // Football: DISABLED completely — single-source Tipsport data
                                        // for obscure leagues is unreliable. No cross-validation.
                                        // Score-edge also disabled. Football OFF across all paths.
                                        "football" => {
                                            info!("⚽ ANOMALY SPORT GUARD: {} — football DISABLED for all auto-bet paths",
                                                anomaly.match_key);
                                            false
                                        }
                                        // Basketball: DISABLED for anomaly path. NBA/basket odds on Azuro
                                        // are too noisy/stale — nearly all "anomalies" are false signals.
                                        // Score-edge path (with point-diff model) is the only valid path.
                                        "basketball" => {
                                            info!("🏀 ANOMALY SPORT GUARD: {} — basketball disabled for anomaly auto-bet (score-edge only)",
                                                anomaly.match_key);
                                            false
                                        }
                                        // Tennis: only auto-bet via anomaly when there's a SET LEAD (≥1 set diff).
                                        // At match start (0-0) or equal sets (1-1), odds discrepancy is noise,
                                        // not a real signal. Prevents betting on every new tennis match.
                                        "tennis" => {
                                            let tennis_ok = if let Some(ref score) = anomaly.live_score {
                                                let parts: Vec<&str> = score.split('-').collect();
                                                if parts.len() == 2 {
                                                    if let (Ok(s1), Ok(s2)) = (parts[0].trim().parse::<i32>(), parts[1].trim().parse::<i32>()) {
                                                        (s1 - s2).abs() >= 1 // require ≥1 set difference
                                                    } else { false }
                                                } else { false }
                                            } else {
                                                false // no live score → cannot validate → skip
                                            };
                                            if !tennis_ok {
                                                info!("🎾 ANOMALY SPORT GUARD: {} score={:?} — tennis needs ≥1 set lead for anomaly auto-bet",
                                                    anomaly.match_key, anomaly.live_score);
                                            }
                                            tennis_ok
                                        }
                                        _ => true,
                                    };

                                    // Check daily NET LOSS limit for anomaly path too
                                    let anomaly_within_daily_limit = {
                                        let net = (daily_wagered - daily_returned).max(0.0);
                                        let (_, _, _, dl_frac, _) = get_exposure_caps(current_bankroll);
                                        net < DAILY_LOSS_LIMIT_USD.min(current_bankroll * dl_frac)
                                    };

                                    let should_auto_bet_anomaly = AUTO_BET_ENABLED
                                        && AUTO_BET_ODDS_ANOMALY_ENABLED
                                        && anomaly.is_live
                                        && anomaly.confidence == "HIGH" // BUG FIX: was missing! MEDIUM anomalies were auto-bet
                                        && anomaly_odds_ok
                                        && anomaly_sport_allowed // sport-specific anomaly guard
                                        && anomaly_within_daily_limit // BUG FIX: anomaly path was missing explicit daily limit check
                                        && azuro_odds >= AUTO_BET_MIN_ODDS
                                        && !azuro_odds_identical
                                        && market_source_count >= AUTO_BET_MIN_MARKET_SOURCES
                                        && !already_bet_this
                                        && anomaly.condition_id.is_some()
                                        && anomaly.outcome_id.is_some()
                                        && anomaly_stake >= 0.50; // EXPOSURE CAP

                                    if anomaly.is_live && market_source_count < AUTO_BET_MIN_MARKET_SOURCES {
                                        info!("⏭️ ODDS ANOMALY {} skipped for auto-bet: only {} market source(s)",
                                            anomaly.match_key, market_source_count);
                                    }

                                    let mut anomaly_alert_sent = false;

                                    if should_auto_bet_anomaly {
                                        let stake = anomaly_stake;
                                        let condition_id = anomaly.condition_id.as_ref().unwrap().clone();
                                        let outcome_id = anomaly.outcome_id.as_ref().unwrap().clone();

                                        let conf_emoji = if anomaly.confidence == "HIGH" { "🟢" } else { "🟡" };
                                        let pre_msg = format!(
                                            "🤖 <b>#{} AUTO-BET ODDS ANOMALY</b> {} {}\n\
                                             \n\
                                             <b>{}</b> vs <b>{}</b>\n\
                                             🎯 Value side: <b>{}</b> @ <b>{:.2}</b>\n\
                                             ⚡ Discrepancy: <b>{:.1}%</b>\n\
                                             💰 Stake: <b>${:.2}</b>\n\
                                             \n\
                                             ⏳ Automaticky sázím...",
                                            aid,
                                            conf_emoji,
                                            anomaly.confidence,
                                            anomaly.team1,
                                            anomaly.team2,
                                            value_team,
                                            azuro_odds,
                                            anomaly.discrepancy_pct,
                                            stake
                                        );
                                        if let Err(e) = tg_send_message(&client, &token, chat_id, &pre_msg).await {
                                            error!("Failed to send anomaly auto-bet pre-alert: {}", e);
                                        } else {
                                            anomaly_alert_sent = true;
                                        }

                                        let decision_instant = std::time::Instant::now();
                                        let min_odds = (azuro_odds * min_odds_factor_for_match(&match_key_for_bet) * 1e12) as u64;
                                        let amount_raw = (stake * 1e6) as u64;
                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "team1": anomaly.team1,
                                            "team2": anomaly.team2,
                                            "valueTeam": value_team,
                                        });

                                        let max_retries = AUTO_BET_RETRY_MAX;
                                        let mut attempt = 0;
                                        loop {
                                        // Signal TTL check — abort if decision is stale
                                        if decision_instant.elapsed() > std::time::Duration::from_secs(SIGNAL_TTL_SECS) {
                                            warn!("⏰ AUTO-BET ODDS #{}: Signal TTL expired ({}ms elapsed) — aborting stale bet",
                                                aid, decision_instant.elapsed().as_millis());
                                            inflight_conditions.remove(&cond_id_str);
                                            inflight_conditions.remove(&match_key_for_bet);
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!("⏰ <b>AUTO-BET ODDS #{} TTL EXPIRED</b> ({}ms) — sázka zastaralá, zrušeno.",
                                                    aid, decision_instant.elapsed().as_millis())
                                            ).await;
                                            break;
                                        }
                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                match resp.json::<ExecutorBetResponse>().await {
                                                    Ok(br) => {
                                                        let is_rejected = br.state.as_deref()
                                                            .map(|s| s == "Rejected" || s == "Failed" || s == "Cancelled")
                                                            .unwrap_or(false);
                                                        if let Some(err) = &br.error {
                                                            let err_lower = err.to_lowercase();
                                                            let is_condition_paused = err_lower.contains("not active")
                                                                || err_lower.contains("paused")
                                                                || err_lower.contains("not exist");
                                                            let is_fatal = err_lower.contains("insufficient")
                                                                || err_lower.contains("allowance")
                                                                || err_lower.contains("revert")
                                                                || err_lower.contains("nonce");
                                                            if is_condition_paused && !is_fatal && attempt < max_retries {
                                                                attempt += 1;
                                                                let delay_ms = AUTO_BET_RETRY_DELAYS_MS.get(attempt.saturating_sub(1)).copied().unwrap_or(400);
                                                                info!("🔄 AUTO-BET ODDS #{} retry {}/{}: condition paused, waiting {}ms... ({})",
                                                                    aid, attempt, max_retries, delay_ms, err);
                                                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                                                continue;
                                                            }
                                                            error!("❌ AUTO-BET ODDS #{} FAILED: {} (cond={}, match={})",
                                                                aid, err,
                                                                &condition_id,
                                                                match_key_for_bet);
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>AUTO-BET ODDS #{} FAILED</b>\n\nError: {}\nCondition: {}",
                                                                    aid, err, &condition_id)
                                                            ).await;
                                                            inflight_conditions.remove(&cond_id_str);
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break;
                                                        } else if is_rejected {
                                                            error!("❌ AUTO-BET ODDS #{} REJECTED: state={} (cond={}, match={})",
                                                                aid, br.state.as_deref().unwrap_or("?"),
                                                                &condition_id,
                                                                match_key_for_bet);
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>AUTO-BET ODDS #{} REJECTED</b>\n\nState: {}\nCondition: {}",
                                                                    aid, br.state.as_deref().unwrap_or("?"),
                                                                    &condition_id)
                                                            ).await;
                                                            inflight_conditions.remove(&cond_id_str);
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break;
                                                        } else {
                                                            auto_bet_count += 1;
                                                            // daily_wagered += stake; // REMOVED: Only count settled losses
                                                            // Persist daily P&L
                                                            {
                                                                let today = Utc::now().format("%Y-%m-%d").to_string();
                                                                let _ = std::fs::write(bet_count_path, format!("{}|{}", today, auto_bet_count));
                                                                let _ = std::fs::write("data/daily_pnl.json",
                                                                    serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                                            }
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let bet_state = br.state.as_deref().unwrap_or("?");
                                                            let token_id_opt = sanitize_token_id(br.token_id.clone());
                                                            let graph_bet_id_opt = br.graph_bet_id.clone();

                                                            already_bet_matches.insert(match_key_for_bet.clone());
                                                            already_bet_conditions.insert(cond_id_str.clone());
                                                            already_bet_base_matches.insert(base_match_key.clone());

                                                            // === EXPOSURE TRACKING (odds anomaly path) ===
                                                            *condition_exposure.entry(cond_id_str.clone()).or_insert(0.0) += stake;
                                                            *match_exposure.entry(base_match_key.clone()).or_insert(0.0) += stake;
                                                            *sport_exposure.entry(anomaly_sport.to_string()).or_insert(0.0) += stake;
                                                            inflight_wagered_total += stake;

                                                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                .create(true).append(true)
                                                                .open(bet_history_path) {
                                                                use std::io::Write;
                                                                let _ = writeln!(f, "{}|{}|{}|{}|{}",
                                                                    match_key_for_bet, cond_id_str,
                                                                    value_team, azuro_odds, Utc::now().to_rfc3339());
                                                            }

                                                            let is_dry_run = bet_state == "DRY-RUN" || bet_id.starts_with("dry-");
                                                            if !is_dry_run {
                                                                active_bets.push(ActiveBet {
                                                                    alert_id: aid,
                                                                    bet_id: bet_id.to_string(),
                                                                    match_key: anomaly.match_key.clone(),
                                                                    team1: anomaly.team1.clone(),
                                                                    team2: anomaly.team2.clone(),
                                                                    value_team: value_team.clone(),
                                                                    amount_usd: stake,
                                                                    odds: azuro_odds,
                                                                    placed_at: Utc::now().to_rfc3339(),
                                                                    condition_id: condition_id.clone(),
                                                                    outcome_id: outcome_id.clone(),
                                                                    graph_bet_id: graph_bet_id_opt.clone(),
                                                                    token_id: token_id_opt.clone(),
                                                                });
                                                                let token_to_write = token_id_opt.as_deref().unwrap_or("?");
                                                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                    .create(true).append(true)
                                                                    .open(pending_claims_path) {
                                                                    use std::io::Write;
                                                                    let _ = writeln!(f, "{}|{}|{}|{}|{}|{}|{}",
                                                                        token_to_write,
                                                                        bet_id, anomaly.match_key,
                                                                        value_team, stake, azuro_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                                // === LEDGER: BET PLACED ===
                                                                ledger_write("PLACED", &serde_json::json!({
                                                                    "alert_id": aid, "bet_id": bet_id,
                                                                    "match_key": anomaly.match_key,
                                                                    "team1": anomaly.team1, "team2": anomaly.team2,
                                                                    "value_team": value_team,
                                                                    "amount_usd": stake, "odds": azuro_odds,
                                                                    "condition_id": condition_id,
                                                                    "outcome_id": outcome_id,
                                                                    "token_id": token_id_opt,
                                                                    "graph_bet_id": graph_bet_id_opt,
                                                                    "path": "anomaly_odds",
                                                                    "flags": {
                                                                        "FF_EXPOSURE_CAPS": FF_EXPOSURE_CAPS,
                                                                        "FF_REBET_ENABLED": FF_REBET_ENABLED,
                                                                        "FF_CROSS_VALIDATION": FF_CROSS_VALIDATION,
                                                                        "FF_CASHOUT_ENABLED": FF_CASHOUT_ENABLED,
                                                                        "FF_INFLIGHT_CAP": FF_INFLIGHT_CAP,
                                                                        "FF_PER_SPORT_CAP": FF_PER_SPORT_CAP,
                                                                        "FF_RESYNC_FREEZE": FF_RESYNC_FREEZE,
                                                                    }
                                                                }));
                                                            }

                                                            let lim_a = "∞".to_string();
                                                            let result_msg = if is_dry_run {
                                                                format!("🧪 <b>AUTO-BET ODDS #{} DRY-RUN</b>\n{} @ {:.2} ${:.2}",
                                                                    aid, value_team, azuro_odds, stake)
                                                            } else {
                                                                format!(
                                                                    "✅ <b>AUTO-BET ODDS #{} PLACED!</b>\n\
                                                                     {} @ {:.2} | ${:.2}\n\
                                                                     Bet ID: <code>{}</code>\n\
                                                                     State: {}\n\
                                                                     Auto-bets dnes: {}/{}",
                                                                    aid, value_team, azuro_odds, stake,
                                                                    bet_id, bet_state,
                                                                    auto_bet_count, lim_a
                                                                )
                                                            };
                                                            let _ = tg_send_message(&client, &token, chat_id, &result_msg).await;
                                                            break;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            &format!("❌ Auto-bet odds #{} response error: {}", aid, e)
                                                        ).await;
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("❌ Executor offline pro auto-bet odds #{}: {}", aid, e)
                                                ).await;
                                                break;
                                            }
                                        }
                                        } // end loop
                                    } else if !mute_manual_alerts {
                                        let msg = format_anomaly_alert(&anomaly, aid);
                                        match tg_send_message(&client, &token, chat_id, &msg).await {
                                            Ok(msg_id) => {
                                                anomaly_alert_sent = true;
                                                msg_id_to_alert_id.insert(msg_id, aid);
                                            }
                                            Err(e) => {
                                                error!("Failed to send alert: {}", e);
                                            }
                                        }
                                    } else {
                                        info!("🔇 MUTED manual anomaly alert #{}: {} disc={:.1}%",
                                            aid, anomaly.match_key, anomaly.discrepancy_pct);
                                    }

                                    if anomaly_alert_sent {
                                        info!("Alert #{} sent: {} side={} disc={:.1}% conf={}",
                                            aid, anomaly.match_key, anomaly.value_side, anomaly.discrepancy_pct, anomaly.confidence);
                                        actually_sent += 1;
                                        sent_alerts.push(SentAlert {
                                            match_key: alert_key,
                                            sent_at: Utc::now(),
                                        });
                                        alert_map.insert(aid, anomaly);
                                    }
                                }

                                // Clean old alerts from map (keep last 50)
                                if alert_map.len() > 50 {
                                    let min_keep = alert_counter.saturating_sub(50);
                                    alert_map.retain(|k, _| *k > min_keep);
                                    msg_id_to_alert_id.retain(|_, aid| *aid > min_keep);
                                }

                                info!("Poll: {} score edges, {} odds anomalies, {} sent (cooldown={})",
                                    score_edges.len(), total_anomalies, actually_sent, sent_alerts.len());
                            }
                            Err(e) => warn!("Failed to parse /state: {}", e),
                        }
                    }
                    Err(e) => warn!("Failed to fetch /state: {}", e),
                }

                // arb_cross_book alerts DISABLED — odds_anomaly covers the same 
                // matches with better context (condition_id, numbered alerts, BET READY)
            }

            // === AUTO-CASHOUT check (gated by FF_CASHOUT_ENABLED) ===
            _ = cashout_ticker.tick() => {
                if !FF_CASHOUT_ENABLED {
                    continue; // Cashout disabled — no EV/fair_value calc = margin leak risk
                }
                if active_bets.is_empty() { continue; }

                for bet in &mut active_bets {
                    // Need graph_bet_id or token_id for cashout
                    let token_id = match &bet.token_id {
                        Some(tid) => tid.clone(),
                        None => {
                            // Try to get bet status from executor to discover token_id
                            if let Ok(resp) = client.get(format!("{}/bet/{}", executor_url, bet.bet_id)).send().await {
                                if let Ok(status) = resp.json::<serde_json::Value>().await {
                                    // Azuro toolkit returns "betId" (number) not "tokenId" (string)
                                    let discovered_tid = status.get("tokenId").and_then(|v| v.as_str().map(|s| s.to_string()))
                                        .or_else(|| status.get("betId").and_then(|v| {
                                            v.as_u64().map(|n| n.to_string())
                                                .or_else(|| v.as_str().map(|s| s.to_string()))
                                        }));
                                    if let Some(tid) = sanitize_token_id(discovered_tid) {
                                        info!("🔍 Discovered tokenId {} for bet {} (cashout)", tid, bet.bet_id);
                                        bet.token_id = Some(tid.clone());
                                        tid
                                    } else if let Some(gid) = status.get("graphBetId").and_then(|v| v.as_str()) {
                                        bet.graph_bet_id = Some(gid.to_string());
                                        continue; // still no tokenId
                                    } else {
                                        continue;
                                    }
                                } else { continue; }
                            } else { continue; }
                        }
                    };

                    // Check cashout availability
                    // Send both graphBetId and tokenId — executor constructs graphBetId from tokenId if needed
                    let check_body = if let Some(ref gid) = bet.graph_bet_id {
                        serde_json::json!({
                            "graphBetId": gid,
                            "tokenId": token_id,
                        })
                    } else {
                        serde_json::json!({
                            "tokenId": token_id,
                        })
                    };
                    let cashout_check = match client.post(format!("{}/check-cashout", executor_url))
                        .json(&check_body).send().await {
                        Ok(r) => r.json::<CashoutCheckResponse>().await.ok(),
                        Err(_) => None,
                    };

                    if let Some(check) = cashout_check {
                        if check.available.unwrap_or(false) {
                            if let Some(odds_str) = &check.cashout_odds {
                                let cashout_odds: f64 = odds_str.parse().unwrap_or(0.0);
                                // BUG #10 FIX: cashoutOdds = current market odds for our outcome.
                                // When odds DROP (outcome more likely), our bet is MORE valuable.
                                // Profit = (original_odds / current_odds - 1) * 100
                                // E.g. bet@2.0, now@1.5 → profit = (2.0/1.5 - 1)*100 = +33%
                                // E.g. bet@2.0, now@2.5 → profit = (2.0/2.5 - 1)*100 = -20%
                                let profit_pct = if bet.odds > 0.0 && cashout_odds > 0.0 {
                                    (bet.odds / cashout_odds - 1.0) * 100.0
                                } else { 0.0 };

                                if profit_pct >= CASHOUT_MIN_PROFIT_PCT {
                                    info!("Auto-cashout #{}: odds {:.3} → cashout {:.3} (+{:.1}%)",
                                        bet.alert_id, bet.odds, cashout_odds, profit_pct);

                                    // Execute cashout
                                    let cashout_body = if let Some(ref gid) = bet.graph_bet_id {
                                        serde_json::json!({
                                            "graphBetId": gid,
                                            "tokenId": token_id,
                                        })
                                    } else {
                                        serde_json::json!({
                                            "tokenId": token_id,
                                        })
                                    };
                                    match client.post(format!("{}/cashout", executor_url))
                                        .json(&cashout_body).send().await {
                                        Ok(resp) => {
                                            match resp.json::<ExecutorCashoutResponse>().await {
                                                Ok(cr) => {
                                                    let state = cr.state.as_deref().unwrap_or("?");
                                                    let _ = tg_send_message(&client, &token, chat_id,
                                                        &format!(
                                                            "💰 <b>AUTO-CASHOUT #{}</b>\n\n\
                                                             {} vs {}\n\
                                                             Bet: ${:.2} @ {:.2}\n\
                                                             Cashout odds: {:.3}\n\
                                                             Profit: <b>+{:.1}%</b>\n\
                                                             Status: {}",
                                                            bet.alert_id, bet.team1, bet.team2,
                                                            bet.amount_usd, bet.odds,
                                                            cashout_odds, profit_pct, state
                                                        )
                                                    ).await;
                                                }
                                                Err(e) => warn!("Cashout response parse error: {}", e),
                                            }
                                        }
                                        Err(e) => warn!("Cashout request failed: {}", e),
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // === AUTO-CLAIM: check settled bets, claim payouts, notify ===
            _ = claim_ticker.tick() => {
                // === STEP 0: Discover tokenIds from Azuro subgraph via /my-bets ===
                // This is the most reliable way to find tokenIds for placed bets
                let has_undiscovered = active_bets.iter().any(|b| b.token_id.is_none());
                if has_undiscovered {
                    if let Ok(resp) = client.get(format!("{}/my-bets", executor_url)).send().await {
                        if let Ok(my_bets) = resp.json::<serde_json::Value>().await {
                            if let Some(bets_arr) = my_bets.get("bets").and_then(|v| v.as_array()) {
                                for ab in &mut active_bets {
                                    if ab.token_id.is_some() { continue; }
                                    // Match by conditionId
                                    for sb in bets_arr {
                                        let sb_cond = sb.get("conditionId").and_then(|v| v.as_str()).unwrap_or("");
                                        if !ab.condition_id.is_empty() && sb_cond == ab.condition_id {
                                            if let Some(tid) = sb.get("tokenId").and_then(|v| v.as_str()) {
                                                let sanitized = sanitize_token_id(Some(tid.to_string()));
                                                if let Some(clean_tid) = sanitized {
                                                    info!("🔍 /my-bets discovered tokenId {} for bet {} (cond={})",
                                                        clean_tid, ab.bet_id, ab.condition_id);
                                                    ab.token_id = Some(clean_tid);
                                                    if let Some(gid) = sb.get("graphBetId").and_then(|v| v.as_str()) {
                                                        ab.graph_bet_id = Some(gid.to_string());
                                                    }
                                                }
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if active_bets.is_empty() {
                    // Even with no tracked bets, call /auto-claim every 5th tick as safety net
                    claim_safety_counter += 1;
                    if claim_safety_counter % 5 == 0 {
                        if let Ok(resp) = client.post(format!("{}/auto-claim", executor_url))
                            .json(&serde_json::json!({})).send().await {
                            if let Ok(cr) = resp.json::<serde_json::Value>().await {
                                let status = cr.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                                if status == "ok" {
                                    let claimed = cr.get("claimed").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let payout = cr.get("totalPayoutUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    let new_bal = cr.get("newBalanceUsd").and_then(|v| v.as_str()).unwrap_or("?");
                                    total_returned += payout;
                                    // BUG FIX: Update DAILY returned for safety-net claims too
                                    daily_returned += payout;
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                    }
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        &format!("💰 <b>AUTO-CLAIM (safety net)</b>\n\nVyplaceno {} sázek, ${:.2}\n💰 Nový zůstatek: {} USDT",
                                            claimed, payout, new_bal)
                                    ).await;
                                    // === LEDGER: SAFETY NET CLAIM (no active bets path) ===
                                    if claimed > 0 {
                                        ledger_write("SAFETY_CLAIM", &serde_json::json!({
                                            "claimed_count": claimed, "payout_usd": payout,
                                            "new_balance": new_bal, "context": "no_active_bets"
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }

                let mut bets_to_remove: Vec<String> = Vec::new();
                let mut tokens_to_claim: Vec<String> = Vec::new();
                let mut claim_details: Vec<(u32, String, String, String, f64, f64, String)> = Vec::new(); // (alert_id, team1, team2, value_team, amount, odds, result)
                let mut needs_pending_rewrite = false;

                for bet in &mut active_bets {
                    // Skip already settled
                    if settled_bet_ids.contains(&bet.bet_id) {
                        continue;
                    }

                    // === PATH A: If we have tokenId already, check payout directly ===
                    if let Some(tid) = &bet.token_id {
                        let payout_body = serde_json::json!({ "tokenId": tid });
                        let payout_resp = match client.post(format!("{}/check-payout", executor_url))
                            .json(&payout_body).send().await {
                            Ok(r) => r.json::<serde_json::Value>().await.ok(),
                            Err(_) => None,
                        };

                        if let Some(pr) = payout_resp {
                            let claimable = pr.get("claimable").and_then(|v| v.as_bool()).unwrap_or(false);
                            let pending = pr.get("pending").and_then(|v| v.as_bool()).unwrap_or(false);
                            let payout_usd = pr.get("payoutUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);

                            if pending {
                                // Bet not yet resolved on chain — skip for now
                                continue;
                            }

                            if claimable && payout_usd > 0.0 {
                                // WON or CANCELED — claim it!
                                let result = if payout_usd > bet.amount_usd * 1.1 { "Won" } else { "Canceled" };
                                // === LEDGER: WON/CANCELED detected (PATH A) ===
                                ledger_write(if result == "Won" { "WON" } else { "CANCELED" }, &serde_json::json!({
                                    "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                    "match_key": bet.match_key,
                                    "value_team": bet.value_team,
                                    "amount_usd": bet.amount_usd, "odds": bet.odds,
                                    "payout_usd": payout_usd,
                                    "token_id": bet.token_id, "path": "A"
                                }));
                                tokens_to_claim.push(tid.clone());
                                claim_details.push((
                                    bet.alert_id,
                                    bet.team1.clone(),
                                    bet.team2.clone(),
                                    bet.value_team.clone(),
                                    bet.amount_usd,
                                    bet.odds,
                                    result.to_string(),
                                ));
                                settled_bet_ids.insert(bet.bet_id.clone());
                                bets_to_remove.push(bet.bet_id.clone());
                                total_wagered += bet.amount_usd;
                            } else {
                                // payout = 0 and not pending: this is NOT enough to mark LOST.
                                // We must confirm real settlement state/result via /bet/:id.
                                let status_resp = match client
                                    .get(format!("{}/bet/{}", executor_url, bet.bet_id))
                                    .send()
                                    .await
                                {
                                    Ok(r) => r.json::<serde_json::Value>().await.ok(),
                                    Err(_) => None,
                                };

                                let status = match status_resp {
                                    Some(s) => s,
                                    None => continue,
                                };

                                let state = status.get("state").and_then(|v| v.as_str()).unwrap_or("");
                                let result = status.get("result").and_then(|v| v.as_str()).unwrap_or("");

                                let is_settled = match state {
                                    "Resolved" | "Canceled" | "Settled" => true,
                                    _ => !result.is_empty() && (result == "Won" || result == "Lost" || result == "Canceled"),
                                };

                                if !is_settled {
                                    continue;
                                }

                                if result == "Lost" {
                                    total_wagered += bet.amount_usd;
                                    daily_wagered += bet.amount_usd;
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write(
                                            "data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string(),
                                        );
                                    }
                                    let loss_msg = format!(
                                        "❌ <b>PROHRA</b>\n\n\
                                         {} vs {}\n\
                                         Sázka: <b>{}</b> @ {:.2} — ${:.2}\n\
                                         Výsledek: <b>PROHRA</b> — -${:.2}",
                                        bet.team1,
                                        bet.team2,
                                        bet.value_team,
                                        bet.odds,
                                        bet.amount_usd,
                                        bet.amount_usd
                                    );
                                    let _ = tg_send_message(&client, &token, chat_id, &loss_msg).await;
                                    // === LEDGER: LOST (PATH A) ===
                                    ledger_write("LOST", &serde_json::json!({
                                        "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                        "match_key": bet.match_key,
                                        "value_team": bet.value_team,
                                        "amount_usd": bet.amount_usd, "odds": bet.odds,
                                        "token_id": bet.token_id, "path": "A"
                                    }));
                                    settled_bet_ids.insert(bet.bet_id.clone());
                                    bets_to_remove.push(bet.bet_id.clone());
                                } else if result == "Won" || result == "Canceled" || state == "Canceled" {
                                    // Resolved non-loss with zero payout response race: leave for claim/status path,
                                    // but do not classify as loss.
                                    continue;
                                } else {
                                    // Unknown settled state/result: keep waiting, never force-loss.
                                    continue;
                                }
                            }
                        }
                        continue;
                    }

                    // === PATH B: No tokenId yet — check via /bet/:id API ===
                    // Query bet status from executor
                    let status_resp = match client.get(format!("{}/bet/{}", executor_url, bet.bet_id))
                        .send().await {
                        Ok(r) => r.json::<serde_json::Value>().await.ok(),
                        Err(_) => None,
                    };

                    let status = match status_resp {
                        Some(s) => s,
                        None => continue,
                    };

                    // Update token_id if discovered
                    // Azuro toolkit returns "betId" (number like 220860) not "tokenId" (string)
                    if bet.token_id.is_none() {
                        let discovered_tid = status.get("tokenId").and_then(|v| v.as_str().map(|s| s.to_string()))
                            .or_else(|| status.get("betId").and_then(|v| {
                                v.as_u64().map(|n| n.to_string())
                                    .or_else(|| v.as_str().map(|s| s.to_string()))
                            }));
                        if let Some(tid) = sanitize_token_id(discovered_tid) {
                            bet.token_id = Some(tid.clone());
                            info!("🔍 Discovered tokenId {} for bet {}", tid, bet.bet_id);
                            // Flag: rewrite pending_claims after this loop ends
                            needs_pending_rewrite = true;
                        }
                    }

                    let state = status.get("state").and_then(|v| v.as_str()).unwrap_or("");
                    let result = status.get("result").and_then(|v| v.as_str()).unwrap_or("");

                    // Check if bet is settled
                    let is_settled = match state {
                        "Resolved" | "Canceled" | "Settled" => true,
                        _ => !result.is_empty() && (result == "Won" || result == "Lost" || result == "Canceled"),
                    };

                    if !is_settled {
                        continue;
                    }

                    info!("🏁 Bet #{} settled: state={} result={}", bet.alert_id, state, result);

                    let effective_result = if !result.is_empty() {
                        result.to_string()
                    } else if state == "Canceled" {
                        "Canceled".to_string()
                    } else {
                        "Unknown".to_string()
                    };

                    // Track settled amount for session stats
                    total_wagered += bet.amount_usd;

                    // If we have a token_id, try to claim payout
                    if let Some(tid) = &bet.token_id {
                        match effective_result.as_str() {
                            "Won" | "Canceled" => {
                                // === LEDGER: WON/CANCELED (PATH B) ===
                                ledger_write(if effective_result == "Won" { "WON" } else { "CANCELED" }, &serde_json::json!({
                                    "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                    "match_key": bet.match_key,
                                    "value_team": bet.value_team,
                                    "amount_usd": bet.amount_usd, "odds": bet.odds,
                                    "token_id": bet.token_id, "path": "B"
                                }));
                                tokens_to_claim.push(tid.clone());
                                claim_details.push((
                                    bet.alert_id,
                                    bet.team1.clone(),
                                    bet.team2.clone(),
                                    bet.value_team.clone(),
                                    bet.amount_usd,
                                    bet.odds,
                                    effective_result.clone(),
                                ));
                            }
                            "Lost" => {
                                // === LEDGER: LOST (PATH B) ===
                                ledger_write("LOST", &serde_json::json!({
                                    "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                    "match_key": bet.match_key,
                                    "value_team": bet.value_team,
                                    "amount_usd": bet.amount_usd, "odds": bet.odds,
                                    "token_id": bet.token_id, "path": "B"
                                }));
                                daily_wagered += bet.amount_usd;
                                {
                                    let today = Utc::now().format("%Y-%m-%d").to_string();
                                    let _ = std::fs::write("data/daily_pnl.json",
                                        serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                }
                                // Notify about loss immediately
                                let loss_msg = format!(
                                    "❌ <b>PROHRA #{}</b>\n\n\
                                     {} vs {}\n\
                                     Sázka: <b>{}</b> @ {:.2} — ${:.2}\n\
                                     Výsledek: <b>PROHRA</b> — -${:.2}\n\n\
                                     📊 Session: vsazeno ${:.2}, vráceno ${:.2}",
                                    bet.alert_id, bet.team1, bet.team2,
                                    bet.value_team, bet.odds, bet.amount_usd,
                                    bet.amount_usd,
                                    total_wagered, total_returned
                                );
                                let _ = tg_send_message(&client, &token, chat_id, &loss_msg).await;
                            }
                            _ => {}
                        }
                    } else {
                        // No token_id — just notify
                        let msg = format!(
                            "🏁 <b>Bet #{} settled</b>: {} (no tokenId pro claim)\n{} vs {}",
                            bet.alert_id, effective_result, bet.team1, bet.team2
                        );
                        let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                    }

                    settled_bet_ids.insert(bet.bet_id.clone());
                    bets_to_remove.push(bet.bet_id.clone());
                }

                // Claim payouts in batch
                if !tokens_to_claim.is_empty() {
                    // PRE-FILTER: verify each token is actually claimable on-chain
                    // (subgraph can be ahead of chain — bet shows Canceled in subgraph but
                    //  viewPayout still reverts on chain)
                    let mut verified_tokens: Vec<String> = Vec::new();
                    let mut verified_details: Vec<(u32, String, String, String, f64, f64, String)> = Vec::new();
                    let mut deferred_bets: Vec<String> = Vec::new(); // tokens that are still pending on-chain

                    for (i, tid) in tokens_to_claim.iter().enumerate() {
                        let payout_body = serde_json::json!({ "tokenId": tid });
                        let payout_check = match client.post(format!("{}/check-payout", executor_url))
                            .json(&payout_body).send().await {
                            Ok(r) => r.json::<serde_json::Value>().await.ok(),
                            Err(_) => None,
                        };

                        let is_chain_ready = payout_check.as_ref()
                            .map(|p| {
                                let claimable = p.get("claimable").and_then(|v| v.as_bool()).unwrap_or(false);
                                let pending = p.get("pending").and_then(|v| v.as_bool()).unwrap_or(true);
                                claimable && !pending
                            })
                            .unwrap_or(false);

                        if is_chain_ready {
                            verified_tokens.push(tid.clone());
                            if i < claim_details.len() {
                                verified_details.push(claim_details[i].clone());
                            }
                        } else {
                            let reason = payout_check.as_ref()
                                .and_then(|p| p.get("reason").and_then(|v| v.as_str()))
                                .unwrap_or("unknown");
                            info!("⏳ Token {} not ready on-chain yet ({}), deferring claim", tid, reason);
                            deferred_bets.push(tid.clone());
                        }
                    }

                    // Remove deferred bets from the "settled" and "remove" lists — they need to stay active
                    if !deferred_bets.is_empty() {
                        info!("⏳ {} bets deferred (chain not ready): {:?}", deferred_bets.len(), deferred_bets);
                        // Find bet_ids that match deferred tokens and remove from bets_to_remove
                        let deferred_set: std::collections::HashSet<&str> = deferred_bets.iter().map(|s| s.as_str()).collect();
                        bets_to_remove.retain(|bid| {
                            let bet_token = active_bets.iter().find(|b| b.bet_id == *bid).and_then(|b| b.token_id.as_deref());
                            !bet_token.map(|t| deferred_set.contains(t)).unwrap_or(false)
                        });
                    }

                    if verified_tokens.is_empty() {
                        info!("⏳ All {} tokens pending on-chain, skipping claim batch", tokens_to_claim.len());
                    } else {
                    info!("💰 Claiming {} settled bets: {:?}", verified_tokens.len(), verified_tokens);

                    let claim_body = serde_json::json!({
                        "tokenIds": verified_tokens,
                    });

                    match client.post(format!("{}/claim", executor_url))
                        .json(&claim_body).send().await {
                        Ok(resp) => {
                            match resp.json::<ClaimResponse>().await {
                                Ok(cr) => {
                                    let tx = cr.tx_hash.as_deref().unwrap_or("?");
                                    let total_payout = cr.total_payout_usd.unwrap_or(0.0);
                                    let new_balance = cr.new_balance_usd.as_deref().unwrap_or("?");

                                    total_returned += total_payout;
                                    // BUG FIX: Update DAILY returned so daily loss cap works correctly
                                    daily_returned += total_payout;
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                    }

                                    // Build detailed notification
                                    let mut msg = String::from("💰 <b>AUTO-CLAIM úspěšný!</b>\n\n");
                                    for (aid, _t1, _t2, vt, amt, odds, res) in &verified_details {
                                        let emoji = if res == "Won" { "✅" } else { "🔄" };
                                        let result_text = if res == "Won" {
                                            format!("VÝHRA! +${:.2}", amt * odds - amt)
                                        } else {
                                            format!("ZRUŠENO, refund ${:.2}", amt)
                                        };
                                        msg.push_str(&format!(
                                            "{} #{} {} @ {:.2} — {}\n",
                                            emoji, aid, vt, odds, result_text
                                        ));
                                    }

                                    let daily_pnl_claim = daily_returned - daily_wagered;
                                    let pnl_sign = if daily_pnl_claim >= 0.0 { "+" } else { "" };

                                    msg.push_str(&format!(
                                        "\n💵 Vyplaceno: <b>${:.2}</b>\n\
                                         📤 TX: <code>{}</code>\n\
                                         💰 <b>Nový zůstatek: {} USDT</b>\n\n\
                                         📊 Daily P/L: <b>{}{:.2} USDT</b>\n\
                                         (vsazeno: ${:.2}, vráceno: ${:.2})",
                                        total_payout, tx, new_balance,
                                        pnl_sign, daily_pnl_claim,
                                        daily_wagered, daily_returned
                                    ));

                                    let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                                    // === LEDGER: CLAIMED (batch) ===
                                    for (aid, t1, t2, vt, amt, odds, res) in &verified_details {
                                        ledger_write("CLAIMED", &serde_json::json!({
                                            "alert_id": aid, "value_team": vt,
                                            "amount_usd": amt, "odds": odds,
                                            "result": res,
                                            "total_payout_usd": total_payout,
                                            "tx_hash": tx, "new_balance": new_balance
                                        }));
                                    }
                                    info!("✅ Claimed ${:.2}, new balance: {} USDT", total_payout, new_balance);
                                }
                                Err(e) => {
                                    warn!("Claim response parse error: {}", e);
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        &format!("⚠️ Claim error: {}", e)
                                    ).await;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Claim request failed: {}", e);
                            let _ = tg_send_message(&client, &token, chat_id,
                                &format!("⚠️ Claim request failed: {}", e)
                            ).await;
                        }
                    }
                    } // end if verified_tokens not empty
                }

                // Remove settled bets from active list
                active_bets.retain(|b| !bets_to_remove.contains(&b.bet_id));

                // Rewrite pending_claims file when bets removed OR tokenIds discovered
                if !bets_to_remove.is_empty() || needs_pending_rewrite {
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true).write(true).truncate(true)
                        .open(pending_claims_path) {
                        use std::io::Write;
                        for bet in &active_bets {
                            let tid = bet.token_id.as_deref().unwrap_or("?");
                            let _ = writeln!(f, "{}|{}|{}|{}|{}|{}",
                                tid, bet.bet_id, bet.match_key,
                                bet.value_team, bet.amount_usd, bet.odds);
                        }
                    }
                }

                // === SAFETY NET: Call /auto-claim every 5th tick to catch any missed bets ===
                claim_safety_counter += 1;
                if claim_safety_counter % 5 == 0 {
                    match client.post(format!("{}/auto-claim", executor_url))
                        .json(&serde_json::json!({})).send().await {
                        Ok(resp) => {
                            if let Ok(cr) = resp.json::<serde_json::Value>().await {
                                let status = cr.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                                if status == "ok" {
                                    let claimed = cr.get("claimed").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let payout = cr.get("totalPayoutUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    let new_bal = cr.get("newBalanceUsd").and_then(|v| v.as_str()).unwrap_or("?");
                                    total_returned += payout;
                                    // BUG FIX: Update DAILY returned for safety-net claims (main loop) too
                                    daily_returned += payout;
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll}).to_string());
                                    }
                                    info!("💰 Safety-net auto-claim: {} bets, ${:.2} (daily_returned now ${:.2})", claimed, payout, daily_returned);
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        &format!("💰 <b>AUTO-CLAIM (safety net)</b>\n\nVyplaceno {} sázek, ${:.2}\n💰 Nový zůstatek: {} USDT",
                                            claimed, payout, new_bal)
                                    ).await;
                                    // === LEDGER: SAFETY NET CLAIM (main loop) ===
                                    if claimed > 0 {
                                        ledger_write("SAFETY_CLAIM", &serde_json::json!({
                                            "claimed_count": claimed, "payout_usd": payout,
                                            "new_balance": new_bal, "context": "main_loop"
                                        }));
                                    }
                                }
                                // "nothing" status = no redeemable bets, silent
                            }
                        }
                        Err(e) => warn!("Auto-claim safety net error: {}", e),
                    }
                }
            }

            // === PORTFOLIO STATUS REPORT (every 30 min) ===
            _ = portfolio_ticker.tick() => {
                let mut msg = String::from("📊 <b>PORTFOLIO STATUS</b>\n\n");
                let uptime_mins = (Utc::now() - session_start).num_minutes();
                msg.push_str(&format!("⏱️ Uptime: {}h {}min\n\n", uptime_mins / 60, uptime_mins % 60));

                // Get wallet balance from executor (try /balance for live on-chain data)
                let executor_ok = match client.get(format!("{}/balance", executor_url)).send().await {
                    Ok(resp) => {
                        match resp.json::<serde_json::Value>().await {
                            Ok(b) => {
                                let bal = b.get("betToken").and_then(|v| v.as_str()).unwrap_or("?");
                                let nat = b.get("native").and_then(|v| v.as_str()).unwrap_or("?");
                                msg.push_str(&format!("💰 <b>Wallet: {} USDT</b> ({} MATIC)\n", bal, &nat[..nat.len().min(6)]));
                                // === BANKROLL REFRESH for exposure caps ===
                                if let Ok(parsed_bal) = bal.parse::<f64>() {
                                    if parsed_bal > 0.0 {
                                        let old_br = current_bankroll;
                                        current_bankroll = parsed_bal;
                                        if (old_br - parsed_bal).abs() > 1.0 {
                                            info!("💰 BANKROLL REFRESH: ${:.2} → ${:.2}", old_br, parsed_bal);
                                        }
                                    }
                                }
                                true
                            }
                            Err(_) => {
                                // Fallback to /health
                                match client.get(format!("{}/health", executor_url)).send().await {
                                    Ok(r) => {
                                        if let Ok(h) = r.json::<ExecutorHealthResponse>().await {
                                            let balance = h.balance.as_deref().unwrap_or("?");
                                            msg.push_str(&format!("💰 <b>Wallet: {} USDT</b>\n", balance));
                                            true
                                        } else { false }
                                    }
                                    Err(_) => false,
                                }
                            }
                        }
                    }
                    Err(_) => false,
                };
                if !executor_ok {
                    msg.push_str("💰 Wallet: ⚠️ executor offline (spusť: cd executor && node index.js)\n");
                }

                // Active bets — try subgraph first for real-time data
                let subgraph_bets: Option<serde_json::Value> = if executor_ok {
                    match client.get(format!("{}/my-bets", executor_url)).send().await {
                        Ok(r) => r.json::<serde_json::Value>().await.ok(),
                        Err(_) => None,
                    }
                } else { None };

                if let Some(ref mb) = subgraph_bets {
                    let total = mb.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                    let won = mb.get("won").and_then(|v| v.as_u64()).unwrap_or(0);
                    let lost = mb.get("lost").and_then(|v| v.as_u64()).unwrap_or(0);
                    let pending_sg = mb.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
                    let redeemable = mb.get("redeemable").and_then(|v| v.as_u64()).unwrap_or(0);
                    let src = mb.get("source").and_then(|v| v.as_str()).unwrap_or("?");
                    msg.push_str(&format!(
                        "📋 Bety na Azuro (subgraph, {}):\n\
                         \u{2022} Celkem: {} | Won: {} | Lost: {} | Pending: {}\n\
                         \u{2022} Vyplatitelné: <b>{}</b>\n",
                        src, total, won, lost, pending_sg, redeemable
                    ));
                    if redeemable > 0 {
                        msg.push_str("⚠️ <b>Nevybráno!</b> Pošlu /auto-claim...\n");
                    }
                }

                // Local tracked active bets
                if active_bets.is_empty() {
                    msg.push_str("🎰 Pending sázek: 0\n");
                } else {
                    let total_at_risk: f64 = active_bets.iter().map(|b| b.amount_usd).sum();
                    msg.push_str(&format!("🎰 Pending sázek: <b>{}</b> (ve hře: ${:.2})\n", active_bets.len(), total_at_risk));
                    for b in &active_bets {
                        msg.push_str(&format!("  \u{2022} {} @ {:.2} ${:.2}\n", b.value_team, b.odds, b.amount_usd));
                    }
                }

                // Daily P&L (persisted across restarts)
                let daily_pnl = daily_returned - daily_wagered;
                let (pnl_sign, pnl_emoji) = if daily_pnl >= 0.0 { ("+", "📈") } else { ("", "📉") };
                msg.push_str(&format!("\n{} Daily P/L: <b>{}{:.2} USDT</b>\n", pnl_emoji, pnl_sign, daily_pnl));
                msg.push_str(&format!("   Vsazeno: ${:.2} | Vráceno: ${:.2}\n", daily_wagered, daily_returned));
                let daily_loss = daily_wagered - daily_returned;
                msg.push_str(&format!("   Loss limit: ${:.2} / ${:.0}\n", if daily_loss > 0.0 { daily_loss } else { 0.0 }, DAILY_LOSS_LIMIT_USD));
                msg.push_str(&format!("   Auto-bets dnes: {}\n", auto_bet_count));

                // Feed-hub live info
                match client.get(format!("{}/state", feed_hub_url)).send().await {
                    Ok(resp) => {
                        if let Ok(state) = resp.json::<StateResponse>().await {
                            let azuro_count = state.odds.iter().filter(|o| o.payload.bookmaker.starts_with("azuro_")).count();
                            let market_count = state.odds.iter().filter(|o| !o.payload.bookmaker.starts_with("azuro_")).count();
                            let map_winner_count = state.odds.iter().filter(|o| {
                                o.payload.market.as_deref().map(|m| m.starts_with("map")).unwrap_or(false)
                            }).count();
                            let tennis_count = state.odds.iter().filter(|o| {
                                o.payload.sport.as_deref() == Some("tennis")
                            }).count();
                            msg.push_str(&format!(
                                "\n📡 Feed-hub: {} live | Azuro: {} odds ({} map, {} tennis) | Market: {}\n",
                                state.live_items, azuro_count, map_winner_count, tennis_count, market_count
                            ));
                        }
                    }
                    Err(_) => {}
                }

                let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                info!("📊 Portfolio report sent");
            }

            // === Check Telegram for user replies ===
            _ = tg_ticker.tick() => {
                match tg_get_updates(&client, &token, update_offset).await {
                    Ok(updates) => {
                        for u in &updates.result {
                            update_offset = u.update_id + 1;
                            let mut text_owned: Option<String> = None;
                            let mut reply_text_owned: Option<String> = None;
                            let mut force_opposite_side = false;

                            if let Some(msg) = &u.message {
                                if msg.chat.id != chat_id { continue; }
                                text_owned = Some(msg.text.as_deref().unwrap_or("").trim().to_string());
                                reply_text_owned = msg.reply_to_message
                                    .as_ref()
                                    .and_then(|rm| rm.text.clone());
                            } else if let Some(mr) = &u.message_reaction {
                                if mr.chat.id != chat_id { continue; }
                                let has_heart = mr.new_reaction.iter().any(|r| {
                                    r.reaction_type == "emoji"
                                        && r.emoji.as_deref().map(|e| e == "❤️" || e == "❤").unwrap_or(false)
                                });
                                let has_blue_heart = mr.new_reaction.iter().any(|r| {
                                    r.reaction_type == "emoji"
                                        && r.emoji.as_deref().map(|e| e == "💙").unwrap_or(false)
                                });
                                if !has_heart && !has_blue_heart {
                                    continue;
                                }

                                if let Some(aid) = msg_id_to_alert_id.get(&mr.message_id).copied() {
                                    force_opposite_side = has_blue_heart;
                                    info!("{} TG reaction detected -> alert_id={} (msg_id={})",
                                        if force_opposite_side { "💙" } else { "❤️" }, aid, mr.message_id);
                                    text_owned = Some(format!("{} YES ${:.0}", aid, MANUAL_BET_DEFAULT_USD));
                                } else {
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        "⚠️ Reakce je na zprávu mimo aktivní alerty (mimo okno posledních alertů). Použij prosím `YES $5` nebo `OPP $5` jako reply.").await;
                                    continue;
                                }
                            }

                            if let Some(text_ref) = text_owned.as_deref() {
                                let text = text_ref.trim();
                                if !text.is_empty() {
                                    info!("📩 TG message: '{}'", text);
                                }

                                // === Commands ===
                                if text == "/status" {
                                    let mut status_msg = String::new();

                                    // Feed Hub status
                                    match client.get(format!("{}/health", feed_hub_url)).send().await {
                                        Ok(r) => {
                                            let health = r.text().await.unwrap_or_default();
                                            match client.get(format!("{}/state", feed_hub_url)).send().await {
                                                Ok(sr) => {
                                                    match sr.json::<StateResponse>().await {
                                                        Ok(s) => {
                                                            let azuro_count = s.odds.iter().filter(|o| o.payload.bookmaker.starts_with("azuro_")).count();
                                                            let market_count = s.odds.iter().filter(|o| !o.payload.bookmaker.starts_with("azuro_")).count();
                                                            status_msg.push_str(&format!(
                                                                "📊 <b>Status</b>\n\n\
                                                                 Feed Hub: {}\n\
                                                                 Connections: {}\n\
                                                                 Live matches: {}\n\
                                                                 Azuro odds: {}\n\
                                                                 Market odds: {}\n\
                                                                 Fused: {}\n",
                                                                health, s.connections, s.live_items,
                                                                azuro_count, market_count, s.fused_ready
                                                            ));
                                                        }
                                                        Err(_) => status_msg.push_str("Feed Hub /state error\n"),
                                                    }
                                                }
                                                Err(_) => status_msg.push_str(&format!("Feed Hub health: {} (state err)\n", health)),
                                            }
                                        }
                                        Err(e) => status_msg.push_str(&format!("❌ Feed Hub offline: {}\n", e)),
                                    };

                                    // Executor status
                                    match client.get(format!("{}/health", executor_url)).send().await {
                                        Ok(resp) => {
                                            match resp.json::<ExecutorHealthResponse>().await {
                                                Ok(h) => {
                                                    status_msg.push_str(&format!(
                                                        "\n🔧 <b>Executor</b>\n\
                                                         Wallet: <code>{}</code>\n\
                                                         Balance: {} USDT\n\
                                                         Allowance: {}\n",
                                                        h.wallet.as_deref().unwrap_or("?"),
                                                        h.balance.as_deref().unwrap_or("?"),
                                                        h.relayer_allowance.as_deref().unwrap_or("?"),
                                                    ));
                                                }
                                                Err(_) => status_msg.push_str("\n⚠️ Executor: nevalidní odpověď\n"),
                                            }
                                        }
                                        Err(_) => status_msg.push_str("\n❌ Executor OFFLINE\n"),
                                    };

                                    status_msg.push_str(&format!(
                                        "\nAlerts: {} (cooldown {}s)\nAktivní bety: {}",
                                        sent_alerts.len(), ALERT_COOLDOWN_SECS, active_bets.len()
                                    ));

                                    let _ = tg_send_message(&client, &token, chat_id, &status_msg).await;

                                } else if text == "/odds" {
                                    match client.get(format!("{}/state", feed_hub_url)).send().await {
                                        Ok(resp) => {
                                            match resp.json::<StateResponse>().await {
                                                Ok(state) => {
                                                    let anomalies = find_odds_anomalies(&state);
                                                    if anomalies.is_empty() {
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            "📭 Žádné odds anomálie právě teď.\nAzuro a trh se shodují."
                                                        ).await;
                                                    } else {
                                                        let summary = anomalies.iter().take(5)
                                                            .map(|a| {
                                                                let team = if a.value_side == 1 { &a.team1 } else { &a.team2 };
                                                                format!("• {} <b>+{:.1}%</b> ({})", team, a.discrepancy_pct, a.match_key)
                                                            })
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        let msg_text = format!("📊 <b>Top {} anomálií:</b>\n\n{}", anomalies.len().min(5), summary);
                                                        let _ = tg_send_message(&client, &token, chat_id, &msg_text).await;
                                                        // Send top anomaly as full alert
                                                        if let Some(top) = anomalies.first() {
                                                            alert_counter += 1;
                                                            let aid = alert_counter;
                                                            match tg_send_message(&client, &token, chat_id,
                                                                &format_anomaly_alert(top, aid)).await {
                                                                Ok(msg_id) => {
                                                                    msg_id_to_alert_id.insert(msg_id, aid);
                                                                    alert_map.insert(aid, top.clone());
                                                                }
                                                                Err(e) => {
                                                                    warn!("/odds full alert send failed: {}", e);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(_) => { let _ = tg_send_message(&client, &token, chat_id, "❌ /state parse error").await; }
                                            }
                                        }
                                        Err(e) => { let _ = tg_send_message(&client, &token, chat_id, &format!("❌ Feed Hub offline: {}", e)).await; }
                                    }

                                } else if text == "/bets" || text == "/mybets" || text == "/my-bets" {
                                    // Show both local tracked bets AND subgraph bets (real-time)
                                    let mut bets_msg = String::from("🎰 <b>SÁZKY</b>\n\n");

                                    // Subgraph bets (real-time from Azuro)
                                    match client.get(format!("{}/my-bets", executor_url)).send().await {
                                        Ok(resp) => {
                                            match resp.json::<serde_json::Value>().await {
                                                Ok(mb) => {
                                                    let total = mb.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let won = mb.get("won").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let lost = mb.get("lost").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let pending_sg = mb.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let redeemable = mb.get("redeemable").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let src = mb.get("source").and_then(|v| v.as_str()).unwrap_or("?");
                                                    bets_msg.push_str(&format!(
                                                        "📊 <b>Azuro subgraph</b> ({}):\n\
                                                         Celkem: {} | ✅ Won: {} | ❌ Lost: {} | ⏳ Pending: {}\n\
                                                         💰 Vyplatitelné: <b>{}</b>\n\n",
                                                        src, total, won, lost, pending_sg, redeemable
                                                    ));
                                                    if let Some(bets_arr) = mb.get("bets").and_then(|v| v.as_array()) {
                                                        for b in bets_arr.iter().take(8) {
                                                            let tid = b.get("tokenId").and_then(|v| v.as_str()).unwrap_or("?");
                                                            let status = b.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                                                            let result = b.get("result").and_then(|v| v.as_str()).unwrap_or("");
                                                            let odds = b.get("odds").and_then(|v| v.as_str()).unwrap_or("?");
                                                            let redeemable_b = b.get("isRedeemable").and_then(|v| v.as_bool()).unwrap_or(false);
                                                            let redeemed_b = b.get("isRedeemed").and_then(|v| v.as_bool()).unwrap_or(false);
                                                            let emoji = if result == "Won" { "✅" } else if result == "Lost" { "❌" } else if redeemable_b && !redeemed_b { "💰" } else { "⏳" };
                                                            bets_msg.push_str(&format!(
                                                                "{} tokenId:{} @ {} — {} {}\n",
                                                                emoji, &tid[..tid.len().min(12)], odds, status,
                                                                if redeemable_b && !redeemed_b { "[CLAIM!]" } else { result }
                                                            ));
                                                        }
                                                    }
                                                }
                                                Err(_) => bets_msg.push_str("⚠️ Subgraph parse error\n\n"),
                                            }
                                        }
                                        Err(_) => bets_msg.push_str("❌ Executor offline — nelze načíst subgraph bety\n\n"),
                                    }

                                    // Local tracked bets
                                    if !active_bets.is_empty() {
                                        bets_msg.push_str(&format!("🔍 <b>Lokálně sledované ({})</b>:\n", active_bets.len()));
                                        for b in &active_bets {
                                            let tid = b.token_id.as_deref().unwrap_or("?");
                                            bets_msg.push_str(&format!(
                                                "  \u{2022} {} @ {:.2} ${:.2} ({})\n",
                                                b.value_team, b.odds, b.amount_usd,
                                                &tid[..tid.len().min(10)]
                                            ));
                                        }
                                    }

                                    let _ = tg_send_message(&client, &token, chat_id, &bets_msg).await;

                                } else if text == "/claim" || text == "/autoclaim" {
                                    // Manual trigger of auto-claim
                                    let _ = tg_send_message(&client, &token, chat_id, "⏳ Spouštím /auto-claim...").await;
                                    match client.post(format!("{}/auto-claim", executor_url))
                                        .json(&serde_json::json!({})).send().await {
                                        Ok(resp) => {
                                            match resp.json::<serde_json::Value>().await {
                                                Ok(cr) => {
                                                    let status = cr.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                                                    let claimed = cr.get("claimed").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let payout = cr.get("totalPayoutUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                                    let new_bal = cr.get("newBalanceUsd").and_then(|v| v.as_str()).unwrap_or("?");
                                                    let tx = cr.get("txHash").and_then(|v| v.as_str()).unwrap_or("");
                                                    let msg_text = if status == "ok" {
                                                        format!("✅ <b>Claim hotovo!</b>\nVyplaceno: {} sázek, ${:.2}\nNový balance: {} USDT\nTX: <code>{}</code>",
                                                            claimed, payout, new_bal, tx)
                                                    } else {
                                                        format!("ℹ️ Claim: {} — {}", status,
                                                            cr.get("message").and_then(|v| v.as_str()).unwrap_or("?"))
                                                    };
                                                    let _ = tg_send_message(&client, &token, chat_id, &msg_text).await;
                                                }
                                                Err(e) => {
                                                    let _ = tg_send_message(&client, &token, chat_id,
                                                        &format!("❌ Claim response error: {}", e)).await;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!("❌ Executor offline: {}", e)).await;
                                        }
                                    }

                                } else if text == "/nabidka" {
                                    mute_manual_alerts = true;
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        "🔇 <b>Manuální nabídky VYPNUTY</b>\n\n\
                                         Anomaly + score-edge alerty pro manuální sázení nebudou chodit.\n\
                                         Auto-bety, portfolio, claimy a status běží normálně.\n\n\
                                         Pro zapnutí pošli: /nabidkaup"
                                    ).await;

                                } else if text == "/nabidkaup" {
                                    mute_manual_alerts = false;
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        "🔔 <b>Manuální nabídky ZAPNUTY</b>\n\n\
                                         Anomaly + score-edge alerty opět chodí.\n\
                                         Pokud chceš vypnout: /nabidka"
                                    ).await;

                                } else if text == "/reset_daily" || text == "/resetdaily" {
                                    let old_w = daily_wagered;
                                    let old_r = daily_returned;
                                    let old_net = (old_w - old_r).max(0.0);
                                    daily_wagered = 0.0;
                                    daily_returned = 0.0;
                                    daily_loss_alert_sent = false;
                                    daily_loss_last_reminder = None;
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": 0.0, "returned": 0.0, "sod_bankroll": start_of_day_bankroll}).to_string());
                                    }
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        &format!(
                                            "🔄 <b>DAILY P&L RESET</b>\n\n\
                                             Předchozí: wagered ${:.2} / returned ${:.2} (net loss ${:.2})\n\
                                             Nový stav: wagered $0.00 / returned $0.00\n\n\
                                             ✅ Daily loss limit odemčen, auto-bety jedou dál.",
                                            old_w, old_r, old_net
                                        )
                                    ).await;
                                    info!("🔄 /reset_daily: wagered {:.2}->{:.2}, returned {:.2}->{:.2}", old_w, 0.0, old_r, 0.0);
                                    ledger_write("DAILY_RESET", &serde_json::json!({
                                        "old_wagered": old_w, "old_returned": old_r,
                                        "old_net_loss": old_net, "trigger": "manual_command"
                                    }));

                                } else if text == "/help" {
                                    let lim_h = "∞".to_string();
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        &format!("🤖 <b>RustMisko Alert Bot v4.5</b>\n\n\
                                         CS2 + Tennis + Football + Basketball\n\
                                         Match + Map Winner | Auto-bet + Auto-claim\n\n\
                                         <b>Commands:</b>\n\
                                         /status — systém + feed-hub + executor\n\
                                         /portfolio — wallet + P/L + report\n\
                                         /bets — sázky ze subgraphu (live) + lokální\n\
                                         /odds — aktuální odds anomálie\n\
                                         /nabidka — vypnout manuální alerty (tichý mód)\n\
                                         /nabidkaup — zapnout manuální alerty\n\
                                         /reset_daily — reset daily loss limitu\n\
                                         /claim — manuální auto-claim výher\n\
                                         /help — tato zpráva\n\n\
                                         <b>Na alert odpověz:</b>\n\
                                         <code>3 YES $3</code> — sázka $3 na alert #3\n\
                                         <code>3 OPP $3</code> — sázka na druhý tým/kurz\n\
                                         <code>3 $3</code> — zkratka pro YES\n\
                                         <code>3 NO</code> — skip alert #3\n\
                                         ❤️ reakce na alert — default bet $3\n\
                                         💙 reakce na alert — bet $3 na druhý tým\n\n\
                                         Auto-bet: edge ≥15% HIGH → auto $2 (limit: {})\n\
                                         Auto-claim: každých 60s, safety-net každých 5min.\n\
                                         Portfolio report: každých 30 min.", lim_h)
                                    ).await;

                                } else if text == "/portfolio" {
                                    // On-demand portfolio report — same logic as ticker
                                    let mut msg = String::from("📊 <b>PORTFOLIO</b>\n\n");
                                    let uptime_mins = (Utc::now() - session_start).num_minutes();
                                    msg.push_str(&format!("⏱️ Session: {}h {}min\n\n", uptime_mins / 60, uptime_mins % 60));

                                    // Live balance
                                    match client.get(format!("{}/balance", executor_url)).send().await {
                                        Ok(resp) => {
                                            match resp.json::<serde_json::Value>().await {
                                                Ok(b) => {
                                                    let bal = b.get("betToken").and_then(|v| v.as_str()).unwrap_or("?");
                                                    let nat = b.get("native").and_then(|v| v.as_str()).unwrap_or("?");
                                                    let wallet = b.get("wallet").and_then(|v| v.as_str()).unwrap_or("?");
                                                    msg.push_str(&format!("💰 <b>{} USDT</b> ({} MATIC)\n", bal, &nat[..nat.len().min(6)]));
                                                    msg.push_str(&format!("🔑 <code>{}</code>\n", wallet));
                                                }
                                                Err(_) => {
                                                    match client.get(format!("{}/health", executor_url)).send().await {
                                                        Ok(r) => {
                                                            if let Ok(h) = r.json::<ExecutorHealthResponse>().await {
                                                                msg.push_str(&format!("💰 <b>{} USDT</b>\n🔑 <code>{}</code>\n",
                                                                    h.balance.as_deref().unwrap_or("?"),
                                                                    h.wallet.as_deref().unwrap_or("?")));
                                                            }
                                                        }
                                                        Err(_) => msg.push_str("❌ Executor offline\n"),
                                                    }
                                                }
                                            }
                                        }
                                        Err(_) => msg.push_str("❌ Executor offline\n"),
                                    }

                                    // Subgraph summary
                                    if let Ok(resp) = client.get(format!("{}/my-bets", executor_url)).send().await {
                                        if let Ok(mb) = resp.json::<serde_json::Value>().await {
                                            let total = mb.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                                            let won = mb.get("won").and_then(|v| v.as_u64()).unwrap_or(0);
                                            let lost = mb.get("lost").and_then(|v| v.as_u64()).unwrap_or(0);
                                            let redeemable = mb.get("redeemable").and_then(|v| v.as_u64()).unwrap_or(0);
                                            msg.push_str(&format!(
                                                "\n📋 Azuro bety: {} total | ✅{} ❌{} | 💰 Claim: {}\n",
                                                total, won, lost, redeemable
                                            ));
                                        }
                                    }

                                    // Local tracked
                                    if !active_bets.is_empty() {
                                        msg.push_str(&format!("\n🎰 <b>Pending sázky ({})</b>\n", active_bets.len()));
                                        let total_at_risk: f64 = active_bets.iter().map(|b| b.amount_usd).sum();
                                        for b in &active_bets {
                                            msg.push_str(&format!("  \u{2022} {} @ {:.2} ${:.2}\n", b.value_team, b.odds, b.amount_usd));
                                        }
                                        msg.push_str(&format!("  Ve hře: <b>${:.2}</b>\n", total_at_risk));
                                    } else {
                                        msg.push_str("\n🎰 Žádné lokálně sledované sázky\n");
                                    }

                                    let daily_pnl = daily_returned - daily_wagered;
                                    let (pnl_sign, pnl_emoji) = if daily_pnl >= 0.0 { ("+", "📈") } else { ("", "📉") };
                                    msg.push_str(&format!("\n{} <b>Daily P/L: {}{:.2} USDT</b>\n", pnl_emoji, pnl_sign, daily_pnl));
                                    msg.push_str(&format!("Vsazeno: ${:.2} | Vráceno: ${:.2}\n", daily_wagered, daily_returned));
                                    let daily_loss = daily_wagered - daily_returned;
                                    msg.push_str(&format!("Loss limit: ${:.2} / ${:.0}\n", if daily_loss > 0.0 { daily_loss } else { 0.0 }, DAILY_LOSS_LIMIT_USD));
                                    msg.push_str(&format!("Auto-bets dnes: {}\n", auto_bet_count));

                                    let _ = tg_send_message(&client, &token, chat_id, &msg).await;

                                // === YES reply: place bet ===
                                } else if let Some((mut aid, amount, parsed_opposite_side)) = parse_bet_reply(text) {
                                    let opposite_side = force_opposite_side || parsed_opposite_side;
                                    // aid=0 means "latest alert"
                                    if aid == 0 {
                                        if let Some(reply_text) = reply_text_owned.as_deref() {
                                            if let Some(extracted) = extract_alert_id_from_text(reply_text) {
                                                aid = extracted;
                                            }
                                        }
                                        if aid == 0 {
                                            aid = alert_counter;
                                        }
                                    }
                                    info!("✅ Parsed BET reply -> alert_id={} amount=${:.2} opposite_side={}", aid, amount, opposite_side);
                                    if let Some(anomaly) = alert_map.get(&aid) {
                                        let alert_age_secs = (Utc::now() - anomaly.detected_at).num_seconds();
                                        if alert_age_secs > MANUAL_ALERT_MAX_AGE_SECS {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "🛑 <b>MANUAL BET BLOCKED</b>\n\nAlert #{} je starý {}s (max {}s).\nPošli čerstvý YES/OPP na nový alert.",
                                                    aid, alert_age_secs, MANUAL_ALERT_MAX_AGE_SECS
                                                )
                                            ).await;
                                            continue;
                                        }

                                        if BLOCK_GENERIC_ESPORTS_BETS && anomaly.match_key.starts_with("esports::") {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "🛑 <b>MANUAL BET BLOCKED</b>\n\nAlert #{} má generic esports:: key (neověřená sport semantika).",
                                                    aid
                                                )
                                            ).await;
                                            continue;
                                        }

                                        // Check we have execution data
                                        let condition_id = match &anomaly.condition_id {
                                            Some(c) => c.clone(),
                                            None => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("⚠️ Alert #{} nemá condition_id — nelze automaticky vsadit.", aid)
                                                ).await;
                                                continue;
                                            }
                                        };
                                        let selected_side = if opposite_side {
                                            if anomaly.value_side == 1 { 2 } else { 1 }
                                        } else {
                                            anomaly.value_side
                                        };

                                        let selected_outcome_id = if selected_side == 1 {
                                            anomaly.outcome1_id.clone()
                                        } else {
                                            anomaly.outcome2_id.clone()
                                        };

                                        let outcome_id = match selected_outcome_id {
                                            Some(o) => o,
                                            None => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("⚠️ Alert #{} nemá outcome_id pro vybranou stranu — nelze automaticky vsadit.", aid)
                                                ).await;
                                                continue;
                                            }
                                        };

                                        let azuro_odds = if selected_side == 1 { anomaly.azuro_w1 } else { anomaly.azuro_w2 };
                                        let value_team = if selected_side == 1 { &anomaly.team1 } else { &anomaly.team2 };

                                        if azuro_odds > MANUAL_BET_MAX_ODDS {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "🛑 <b>MANUAL BET BLOCKED</b>\n\nAlert #{}\n{} @ {:.2}\nMax manual odds cap: {:.2}",
                                                    aid, value_team, azuro_odds, MANUAL_BET_MAX_ODDS
                                                )
                                            ).await;
                                            continue;
                                        }

                                        // Acknowledge
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!(
                                                "⏳ <b>Placing bet #{}</b>\n\
                                                 {} @ {:.2} | ${:.2}\n\
                                                 Condition: {}\n\
                                                 Outcome: {}\n\
                                                 Posílám do executoru...",
                                                aid, value_team, azuro_odds, amount,
                                                condition_id, outcome_id
                                            )
                                        ).await;

                                        // POST to executor
                                        let min_odds = (azuro_odds * min_odds_factor_for_match(&anomaly.match_key) * 1e12) as u64;
                                        let amount_raw = (amount * 1e6) as u64; // USDT 6 decimals

                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "team1": anomaly.team1,
                                            "team2": anomaly.team2,
                                            "valueTeam": value_team,
                                        });

                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                match resp.json::<ExecutorBetResponse>().await {
                                                    Ok(br) => {
                                                        let is_rejected = br.state.as_deref()
                                                            .map(|s| s == "Rejected" || s == "Failed" || s == "Cancelled")
                                                            .unwrap_or(false);
                                                        if let Some(err) = &br.error {
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>BET FAILED #{}</b>\n\nError: {}", aid, err)
                                                            ).await;
                                                        } else if is_rejected {
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>BET REJECTED #{}</b>\n\nState: {}\nCondition may be resolved or odds moved.",
                                                                    aid, br.state.as_deref().unwrap_or("?"))
                                                            ).await;
                                                            // === LEDGER: REJECTED (bet-command) ===
                                                            ledger_write("REJECTED", &serde_json::json!({
                                                                "alert_id": aid,
                                                                "match_key": anomaly.match_key,
                                                                "value_team": value_team,
                                                                "state": br.state,
                                                                "path": "bet_command"
                                                            }));
                                                        } else {
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let state = br.state.as_deref().unwrap_or("?");
                                                            let token_id_opt = sanitize_token_id(br.token_id.clone());
                                                            let graph_bet_id_opt = br.graph_bet_id.clone();

                                                            let is_dry_run = state == "DRY-RUN" || bet_id.starts_with("dry-");

                                                            // Don't track dry-run bets as active
                                                            if !is_dry_run {
                                                                active_bets.push(ActiveBet {
                                                                    alert_id: aid,
                                                                    bet_id: bet_id.to_string(),
                                                                    match_key: anomaly.match_key.clone(),
                                                                    team1: anomaly.team1.clone(),
                                                                    team2: anomaly.team2.clone(),
                                                                    value_team: value_team.to_string(),
                                                                    amount_usd: amount,
                                                                    odds: azuro_odds,
                                                                    placed_at: Utc::now().to_rfc3339(),
                                                                    condition_id: condition_id.clone(),
                                                                    outcome_id: outcome_id.clone(),
                                                                    graph_bet_id: graph_bet_id_opt.clone(),
                                                                    token_id: token_id_opt.clone(),
                                                                });

                                                                let token_to_write = token_id_opt.as_deref().unwrap_or("?");
                                                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                    .create(true).append(true)
                                                                    .open(pending_claims_path) {
                                                                    use std::io::Write;
                                                                    let _ = writeln!(f, "{}|{}|{}|{}|{}|{}|{}",
                                                                        token_to_write,
                                                                        bet_id, anomaly.match_key,
                                                                        value_team, amount, azuro_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                                // === LEDGER: BET PLACED (bet-command) ===
                                                                ledger_write("PLACED", &serde_json::json!({
                                                                    "alert_id": aid, "bet_id": bet_id,
                                                                    "match_key": anomaly.match_key,
                                                                    "team1": anomaly.team1, "team2": anomaly.team2,
                                                                    "value_team": value_team,
                                                                    "amount_usd": amount, "odds": azuro_odds,
                                                                    "condition_id": condition_id,
                                                                    "outcome_id": outcome_id,
                                                                    "token_id": token_id_opt,
                                                                    "graph_bet_id": graph_bet_id_opt,
                                                                    "path": "bet_command",
                                                                    "flags": {
                                                                        "FF_EXPOSURE_CAPS": FF_EXPOSURE_CAPS,
                                                                        "FF_REBET_ENABLED": FF_REBET_ENABLED,
                                                                        "FF_CROSS_VALIDATION": FF_CROSS_VALIDATION,
                                                                        "FF_CASHOUT_ENABLED": FF_CASHOUT_ENABLED,
                                                                        "FF_INFLIGHT_CAP": FF_INFLIGHT_CAP,
                                                                        "FF_PER_SPORT_CAP": FF_PER_SPORT_CAP,
                                                                        "FF_RESYNC_FREEZE": FF_RESYNC_FREEZE,
                                                                    }
                                                                }));
                                                            }

                                                            let msg = if is_dry_run {
                                                                format!(
                                                                    "🧪 <b>DRY-RUN #{}</b> (SIMULACE)\n\n\
                                                                     {} @ {:.2} | ${:.2}\n\n\
                                                                     ⚠️ Bet NEBYL odeslán on-chain!\n\
                                                                     Executor běží bez PRIVATE_KEY.\n\
                                                                     Pro reálné bety nastav v terminálu:\n\
                                                                     <code>$env:PRIVATE_KEY=\"0x...\"</code>\n\
                                                                     a restartuj executor.",
                                                                    aid, value_team, azuro_odds, amount
                                                                )
                                                            } else {
                                                                format!(
                                                                    "✅ <b>BET PLACED #{}</b>\n\n\
                                                                     {} @ {:.2} | ${:.2}\n\
                                                                     Bet ID: <code>{}</code>\n\
                                                                     State: {}\n\n\
                                                                     Auto-cashout aktivní (≥{}% profit).",
                                                                    aid, value_team, azuro_odds, amount,
                                                                    bet_id, state, CASHOUT_MIN_PROFIT_PCT
                                                                )
                                                            };

                                                            let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            &format!("❌ Executor bet response error: {}", e)
                                                        ).await;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("❌ Executor nedostupný: {}\nSpusť: cd executor && node index.js", e)
                                                ).await;
                                            }
                                        }
                                    } else {
                                        warn!("⚠️ YES parsed but alert #{} not found (alert_map size={})", aid, alert_map.len());
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!("⚠️ Alert #{} nenalezen. Možná expiroval (max 50 v paměti).", aid)
                                        ).await;
                                    }

                                // === NO reply: skip ===
                                } else if let Some(aid) = parse_no_reply(text) {
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        &format!("⏭️ Alert #{} přeskočen.", aid)
                                    ).await;

                                // Legacy NO/SKIP without number
                                } else if text.eq_ignore_ascii_case("NO") || text.eq_ignore_ascii_case("SKIP") {
                                    let _ = tg_send_message(&client, &token, chat_id, "⏭️ Skipped.").await;

                                } else if text.starts_with("/") {
                                    // Unknown command — ignore
                                }
                                // else: ignore non-command messages
                            }
                        }
                    }
                    Err(e) => {
                        if Utc::now().timestamp() % 60 == 0 {
                            warn!("getUpdates err: {}", e);
                        }
                    }
                }
            }
        }
    }
}
