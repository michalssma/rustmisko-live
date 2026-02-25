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
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, HashMap};
use std::time::Duration;
use tracing::{info, warn, error};
use tracing_subscriber::{EnvFilter, fmt};
use std::path::Path;

// ====================================================================
// Config
// ====================================================================

const POLL_INTERVAL_SECS: u64 = 10;  // Fast polling for score edge detection!
/// Minimum edge % to trigger alert (all tiers)
const MIN_EDGE_PCT: f64 = 5.0;
/// Don't re-alert same match within this window
const ALERT_COOLDOWN_SECS: i64 = 1800; // 30 min
/// Auto-cashout check interval
const CASHOUT_CHECK_SECS: u64 = 30;
/// Minimum profit % to auto-cashout
const CASHOUT_MIN_PROFIT_PCT: f64 = 3.0;
/// Minimum score-edge % to trigger alert
const MIN_SCORE_EDGE_PCT: f64 = 8.0;
/// Score edge cooldown per match (seconds)
const SCORE_EDGE_COOLDOWN_SECS: i64 = 300; // 5 min per match
/// === AUTO-BET CONFIG ===
/// Auto-bet ON: automaticky vsadí na HIGH confidence score edges
const AUTO_BET_ENABLED: bool = true;
/// Minimum edge % for auto-bet (only HIGH confidence)
/// TIGHTENED: was 12%, now 15% — only bet on strong edges
const AUTO_BET_MIN_EDGE_PCT: f64 = 15.0;
/// Fixed stake per auto-bet in USD
const AUTO_BET_STAKE_USD: f64 = 2.0;
/// Maximum number of auto-bets per session (safety limit)
const AUTO_BET_MAX_PER_SESSION: u32 = 10;
/// Minimum Azuro odds to auto-bet (skip heavy favorites)
const AUTO_BET_MIN_ODDS: f64 = 1.15;
/// === AUTO-CLAIM CONFIG ===
/// Interval for checking settled bets and claiming payouts
const CLAIM_CHECK_SECS: u64 = 60;
/// Maximum odds for auto-bet (skip extreme underdogs)
const AUTO_BET_MAX_ODDS: f64 = 3.50;
/// Portfolio status report interval (seconds) — every 30 min
const PORTFOLIO_REPORT_SECS: u64 = 1800;

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
    odds_team1: f64,
    odds_team2: f64,
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
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    message_id: i64,
    chat: TgChat,
    text: Option<String>,
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

async fn tg_send_message(client: &reqwest::Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
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
    }
    Ok(())
}

async fn tg_get_updates(client: &reqwest::Client, token: &str, offset: i64) -> Result<TgUpdatesResponse> {
    let url = format!(
        "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout=5&allowed_updates=[\"message\"]",
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
    outcome_id: Option<String>,
    chain: Option<String>,
    azuro_url: Option<String>,
}

/// CS2 score → estimated win probability for the LEADING team
/// Detects whether scores are round-level (0-13) or map-level (0-2)
/// and returns expected match win probability.
///
/// Round scores (max > 3): within a single map
///   - Leading by 3+ rounds → team controlling the map
///   - Leading by 6+ → map almost decided
///   - Leading by 8+ → map virtually won
///
/// Map scores (max <= 3): Bo3 map count
///   - 1-0 → ~68% match win
///   - 2-0 → match won (don't bet)
fn score_to_win_prob(leading_score: i32, losing_score: i32) -> Option<f64> {
    let diff = leading_score - losing_score;
    if diff <= 0 { return None; }

    let max_score = leading_score.max(losing_score);

    if max_score > 3 {
        // ROUND scores within a map (CS2 MR12: first to 13)
        // The leading team needs fewer rounds to win this map.
        // Model: differential maps to map-win probability,
        // then map-win probability gives match advantage.
        let rounds_to_win = 13 - leading_score; // rounds leading team needs
        let rounds_opponent = 13 - losing_score; // rounds opponent needs

        if rounds_to_win <= 0 {
            return None; // Map already won, too late
        }

        // Rough map-win probability based on round differential
        let map_win_prob = match diff {
            1..=2 => 0.58,   // Slight lead
            3..=4 => 0.70,   // Solid lead
            5..=6 => 0.82,   // Dominant
            7..=8 => 0.90,   // Near-certain map win
            _ => 0.95,       // 9+ round lead = map locked
        };

        // Map win → match impact (assumes this map matters)
        // If they win this map: they either take 1-0 or 2-1 lead
        // Approximate: match_prob ≈ 50% + (map_win_prob - 50%) * 0.55
        let match_prob = 0.50 + (map_win_prob - 0.50) * 0.55;

        // Only flag if there's a STRONG round lead
        // TIGHTENED: was diff >= 3, now diff >= 5
        // At 4-1 in CS2, CT side often starts strong then collapses on T side.
        // Comebacks at 4-1 happen ~35-38% of the time — NOT safe enough for auto-bet.
        // At 7-2 (diff=5), map win prob is genuinely 82% → match prob 67.6%.
        if diff >= 5 {
            Some(match_prob)
        } else {
            None // Too close to call from rounds alone
        }
    } else {
        // MAP scores (Bo3/Bo5 format)
        match (leading_score, losing_score) {
            (1, 0) => Some(0.68),  // Won first map → ~68%
            (2, 0) => None,        // Already won → too late
            (2, 1) => None,        // Already won
            _ => None,
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
    if diff < 5 || total < 10 { return None; }

    match diff {
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
    if total < 15 { return None; }

    // Early game (< 30 total) — leads are volatile
    if total < 30 {
        return match diff {
            1..=4  => None,            // Too close early
            5..=9  => Some(0.60),
            10..=14 => Some(0.67),
            _ => Some(0.75),           // 15+ early
        };
    }

    // Mid game (30-80 total)
    if total < 80 {
        return match diff {
            1..=4  => None,            // Small lead, high variance
            5..=9  => Some(0.62),
            10..=14 => Some(0.72),
            15..=19 => Some(0.80),
            _ => Some(0.87),           // 20+ mid
        };
    }

    // Late game (80+ total) — leads are decisive
    match diff {
        1..=4  => None,
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
) -> Vec<ScoreEdge> {
    let now = Utc::now();
    let mut edges = Vec::new();

    // Build live score map
    let live_map: HashMap<&str, &LiveItem> = state.live.iter()
        .map(|l| (l.match_key.as_str(), l))
        .collect();

    // Build Azuro odds map (only azuro_ bookmakers, match_winner)
    let mut azuro_by_match: HashMap<&str, &OddsPayload> = HashMap::new();
    // Build map winner odds map: match_key → Vec<MapWinnerOdds>
    let mut map_winners_by_match: HashMap<&str, Vec<MapWinnerOdds>> = HashMap::new();
    for item in &state.odds {
        if !item.payload.bookmaker.starts_with("azuro_") {
            continue;
        }
        let market = item.payload.market.as_deref().unwrap_or("match_winner");
        if market == "match_winner" {
            azuro_by_match.entry(item.match_key.as_str())
                .or_insert(&item.payload);
        } else if market.starts_with("map") && market.ends_with("_winner") {
            // map1_winner, map2_winner, map3_winner
            map_winners_by_match.entry(item.match_key.as_str())
                .or_default()
                .push(MapWinnerOdds {
                    market: market.to_string(),
                    odds_team1: item.payload.odds_team1,
                    odds_team2: item.payload.odds_team2,
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

        // Cooldown: don't re-alert same match too quickly
        if let Some(last_alert) = tracker.edge_cooldown.get(*match_key) {
            if (now - *last_alert).num_seconds() < SCORE_EDGE_COOLDOWN_SECS {
                continue;
            }
        }

        // Determine which team is leading
        if s1 == s2 {
            continue; // Tied → no directional edge
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

        // === STEP 1: Check MAP WINNER edges FIRST (highest priority) ===
        if max_score > 3 && diff >= 5 {
            // This is a round-level score within a CS2 map
            if let Some(map_odds_list) = map_winners_by_match.get(odds_lookup_key) {
                // Map win probability direct (NOT converted to match prob)
                let map_win_prob = match diff {
                    5..=6 => 0.82,
                    7..=8 => 0.90,
                    _ => 0.95,  // 9+
                };

                for mw in map_odds_list {
                    let mw_implied = if leading_side == 1 {
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
                    let mw_outcome_id = if leading_side == 1 {
                        mw.outcome1_id.clone()
                    } else {
                        mw.outcome2_id.clone()
                    };

                    let leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
                    info!("🗺️ MAP WINNER EDGE [PRIORITY]: {} leads {}-{}, {} implied={:.1}%, map_prob={:.1}%, edge={:.1}% — BLOCKING match_winner",
                        leading_team, s1, s2, mw.market, mw_implied * 100.0, map_win_prob * 100.0, mw_edge);

                    tracker.edge_cooldown.insert(match_key.to_string(), now);
                    has_map_winner_edge = true;

                    edges.push(ScoreEdge {
                        match_key: format!("{}::{}", match_key, mw.market),
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score1: s1,
                        score2: s2,
                        prev_score1: prev_s1,
                        prev_score2: prev_s2,
                        leading_side,
                        azuro_w1: mw.odds_team1,
                        azuro_w2: mw.odds_team2,
                        azuro_bookmaker: format!("{} [{}]", mw.bookmaker, mw.market),
                        azuro_implied_pct: mw_implied * 100.0,
                        score_implied_pct: map_win_prob * 100.0,
                        edge_pct: mw_edge,
                        confidence: mw_confidence,
                        game_id: None,
                        condition_id: mw.condition_id.clone(),
                        outcome_id: mw_outcome_id,
                        chain: mw.chain.clone(),
                        azuro_url: mw.url.clone(),
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
        let azuro = match azuro_by_match.get(odds_lookup_key) {
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
        let azuro_implied = if leading_side == 1 {
            1.0 / azuro.odds_team1
        } else {
            1.0 / azuro.odds_team2
        };

        // EDGE = expected - azuro_implied
        let edge = (expected_prob - azuro_implied) * 100.0;

        if edge < MIN_SCORE_EDGE_PCT {
            info!("  ⏭️ {} {}-{}: edge={:.1}% < min {}% (prob={:.0}% az={:.0}%)",
                match_key, s1, s2, edge, MIN_SCORE_EDGE_PCT, expected_prob*100.0, azuro_implied*100.0);
            continue;
        }

        // Confidence based on edge size
        let confidence = if edge >= 15.0 { "HIGH" } else { "MEDIUM" };

        let leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
        info!("⚡ MATCH WINNER EDGE [FALLBACK]: {} leads {}-{}, Azuro implied {:.1}%, expected {:.1}%, edge {:.1}% (no map_winner odds available)",
            leading_team, s1, s2, azuro_implied * 100.0, expected_prob * 100.0, edge);

        tracker.edge_cooldown.insert(match_key.to_string(), now);

        let outcome_id = if leading_side == 1 {
            azuro.outcome1_id.clone()
        } else {
            azuro.outcome2_id.clone()
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
            azuro_w1: azuro.odds_team1,
            azuro_w2: azuro.odds_team2,
            azuro_bookmaker: azuro.bookmaker.clone(),
            azuro_implied_pct: azuro_implied * 100.0,
            score_implied_pct: expected_prob * 100.0,
            edge_pct: edge,
            confidence,
            game_id: azuro.game_id.clone(),
            condition_id: azuro.condition_id.clone(),
            outcome_id,
            chain: azuro.chain.clone(),
            azuro_url: azuro.url.clone(),
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

    format!(
        "⚡ <b>#{}</b> {} <b>SCORE EDGE</b> [{}]\n\
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
         Reply: <code>{} YES $5</code> / <code>{} NO</code>",
        alert_id, conf_emoji, e.confidence,
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
        alert_id, alert_id
    )
}

// ====================================================================
// Odds comparison logic
// ====================================================================

struct OddsAnomaly {
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
    // Levenshtein-like: if short and differ by 1-2 chars, might be typo
    if na.len() >= 3 && nb.len() >= 3 {
        let shorter = na.len().min(nb.len());
        let common = na.chars().zip(nb.chars()).filter(|(a, b)| a == b).count();
        if common as f64 / shorter as f64 > 0.75 { return true; }
    }
    false
}

/// Detect if odds from two sources have team1/team2 swapped
/// Returns (market_w1_aligned, market_w2_aligned, is_swapped)
fn align_teams(azuro: &OddsPayload, market: &OddsPayload) -> (f64, f64, bool) {
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

    if swap_score > normal_score {
        // Teams are swapped — flip market odds
        (market.odds_team2, market.odds_team1, true)
    } else {
        (market.odds_team1, market.odds_team2, false)
    }
}

fn find_odds_anomalies(state: &StateResponse) -> Vec<OddsAnomaly> {
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
        let azuro_items: Vec<&&StateOddsItem> = items.iter().filter(|i| i.payload.bookmaker.starts_with("azuro_")).collect();
        // Include hltv-featured (20bet, ggbet, etc.) as market reference!
        let market_items: Vec<&&StateOddsItem> = items.iter().filter(|i| !i.payload.bookmaker.starts_with("azuro_")).collect();

        if azuro_items.is_empty() || market_items.is_empty() {
            continue;
        }

        let azuro = &azuro_items[0].payload;
        let is_live = live_keys.contains_key(match_key.as_str());
        let live_score = live_keys.get(match_key.as_str()).map(|l| {
            format!("{}-{}", l.payload.score1, l.payload.score2)
        });

        // For each market source, align teams and compute discrepancy
        let mut total_m_w1 = 0.0_f64;
        let mut total_m_w2 = 0.0_f64;
        let mut any_swapped = false;
        let mut market_count = 0;

        for mi in &market_items {
            let (mw1, mw2, swapped) = align_teams(azuro, &mi.payload);
            total_m_w1 += mw1;
            total_m_w2 += mw2;
            if swapped { any_swapped = true; }
            market_count += 1;
        }

        let avg_w1 = total_m_w1 / market_count as f64;
        let avg_w2 = total_m_w2 / market_count as f64;

        let market_bookie = market_items.iter().map(|i| i.payload.bookmaker.as_str()).collect::<Vec<_>>().join("+");

        let disc_w1 = (azuro.odds_team1 / avg_w1 - 1.0) * 100.0;
        let disc_w2 = (azuro.odds_team2 / avg_w2 - 1.0) * 100.0;

        // === Confidence scoring ===
        let mut reasons: Vec<String> = Vec::new();
        let mut penalty = 0;

        // PENALTY: match is live but Azuro is prematch-only
        if is_live {
            reasons.push("LIVE zápas — Azuro odds mohou být prematch (stale!)".into());
            penalty += 3;
        }

        // PENALTY: teams were swapped
        if any_swapped {
            reasons.push(format!("Team order PROHOZENÝ (azuro: {} vs {}, trh: {} vs {})",
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

        if disc_w1 > MIN_EDGE_PCT {
            anomalies.push(OddsAnomaly {
                match_key: match_key.clone(),
                team1: azuro.team1.clone(),
                team2: azuro.team2.clone(),
                azuro_w1: azuro.odds_team1,
                azuro_w2: azuro.odds_team2,
                azuro_bookmaker: azuro.bookmaker.clone(),
                azuro_url: azuro.url.clone(),
                market_w1: avg_w1,
                market_w2: avg_w2,
                market_bookmaker: market_bookie.clone(),
                value_side: 1,
                discrepancy_pct: disc_w1,
                confidence,
                confidence_reasons: reasons.clone(),
                teams_swapped: any_swapped,
                is_live,
                live_score: live_score.clone(),
                game_id: azuro.game_id.clone(),
                condition_id: azuro.condition_id.clone(),
                outcome_id: azuro.outcome1_id.clone(),
                chain: azuro.chain.clone(),
            });
        }
        if disc_w2 > MIN_EDGE_PCT {
            anomalies.push(OddsAnomaly {
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
                confidence_reasons: reasons.clone(),
                teams_swapped: any_swapped,
                is_live,
                live_score: live_score.clone(),
                game_id: azuro.game_id.clone(),
                condition_id: azuro.condition_id.clone(),
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
        "\n⚠️ Týmy PROHOZENÉ mezi zdroji (opraveno)"
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

    format!(
        "🎯 <b>#{}</b> {} <b>ODDS ANOMALY</b> [{}]\n\
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
         Reply: <code>{} YES $5</code> / <code>{} NO</code>",
        alert_id, conf_emoji, a.confidence,
        a.team1, a.team2, live_line, swap_warn,
        a.azuro_bookmaker,
        a.team1, a.azuro_w1, a.team2, a.azuro_w2,
        a.market_bookmaker,
        a.team1, a.market_w1, a.team2, a.market_w2,
        value_team, a.discrepancy_pct,
        azuro_odds, market_odds, reasons_text, url_line,
        exec_ready,
        value_team, azuro_odds,
        alert_id, alert_id
    )
}

fn format_opportunity_alert(opp: &Opportunity) -> String {
    let emoji = match opp.opp_type.as_str() {
        "arb_cross_book" => "💰",
        "score_momentum" => "📈",
        "tight_spread_underdog" => "🎲",
        _ => "❓",
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
        opp.score,
        opp.signal,
        opp.edge_pct, opp.odds,
        opp.bookmaker,
        opp.confidence * 100.0
    )
}

// ====================================================================
// Main loop
// ====================================================================

/// Parse reply like "3 YES $5", "3 YES 5", "3 YES", "YES $5", "YES" → (alert_id, amount)
/// If no alert_id given, returns 0 (caller uses latest alert)
fn parse_yes_reply(text: &str) -> Option<(u32, f64)> {
    let text = text.trim();
    let parts: Vec<&str> = text.splitn(4, char::is_whitespace).collect();
    if parts.is_empty() { return None; }

    // Format 1: "{id} YES [$]{amount}" e.g. "3 YES $5"
    // Format 2: "{id} YES" e.g. "3 YES" → default $5
    // Format 3: "YES [$]{amount}" e.g. "YES $5" → use latest alert (id=0)
    // Format 4: "YES" → use latest alert, default $5

    if let Ok(id) = parts[0].parse::<u32>() {
        // Starts with number → Format 1 or 2
        if parts.len() < 2 { return None; }
        if !parts[1].eq_ignore_ascii_case("YES") { return None; }
        let amount = if parts.len() >= 3 {
            parts[2].trim_start_matches('$').trim().parse::<f64>().unwrap_or(5.0)
        } else {
            5.0
        };
        Some((id, amount))
    } else if parts[0].eq_ignore_ascii_case("YES") {
        // Starts with YES → Format 3 or 4 (id=0 means "latest")
        let amount = if parts.len() >= 2 {
            parts[1].trim_start_matches('$').trim().parse::<f64>().unwrap_or(5.0)
        } else {
            5.0
        };
        Some((0, amount))
    } else {
        None
    }
}

/// Parse reply like "3 NO" → alert_id
fn parse_no_reply(text: &str) -> Option<u32> {
    let text = text.trim();
    let parts: Vec<&str> = text.splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 { return None; }
    let id: u32 = parts[0].parse().ok()?;
    if parts[1].eq_ignore_ascii_case("NO") || parts[1].eq_ignore_ascii_case("SKIP") {
        Some(id)
    } else {
        None
    }
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
    let mut active_bets: Vec<ActiveBet> = Vec::new();
    let mut score_tracker = ScoreTracker::new();
    let mut auto_bet_count: u32 = 0;

    // === DEDUP: track already-bet match keys + condition IDs (persisted across restarts) ===
    let bet_history_path = "data/bet_history.txt";
    let mut already_bet_matches: HashSet<String> = HashSet::new();
    let mut already_bet_conditions: HashSet<String> = HashSet::new();
    // Load from file on startup
    if Path::new(bet_history_path).exists() {
        if let Ok(contents) = std::fs::read_to_string(bet_history_path) {
            for line in contents.lines() {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 2 {
                    already_bet_matches.insert(parts[0].to_string());
                    already_bet_conditions.insert(parts[1].to_string());
                }
            }
            info!("📋 Loaded {} previous bets from history (dedup protection)", already_bet_matches.len());
        }
    }

    // === PENDING CLAIMS: persist token IDs for bets waiting to be claimed ===
    let pending_claims_path = "data/pending_claims.txt";
    // Format per line: tokenId|betId|matchKey|valueTeam|amountUsd|odds|timestamp
    // Load on startup → add to active_bets for auto-claim monitoring
    if Path::new(pending_claims_path).exists() {
        if let Ok(contents) = std::fs::read_to_string(pending_claims_path) {
            for line in contents.lines() {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 6 {
                    let token_id_raw = parts[0].to_string();
                    let bet_id = parts[1].to_string();
                    let match_key = parts[2].to_string();
                    let value_team = parts[3].to_string();
                    let amount_usd: f64 = parts[4].parse().unwrap_or(2.0);
                    let odds: f64 = parts[5].parse().unwrap_or(1.5);
                    // "?" means tokenId not yet discovered — set to None so PATH B will discover it
                    let token_id = if token_id_raw == "?" || token_id_raw.is_empty() {
                        None
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
                    format!("✅ Executor ONLINE\n   Wallet: <code>{}</code>\n   Balance: {} USDT\n   Allowance: {}", wallet, balance, allowance)
                }
                Err(_) => "⚠️ Executor odpověděl, ale nevalidní JSON".to_string(),
            }
        }
        Err(_) => "❌ Executor OFFLINE — sázky nebudou fungovat!\n   Spusť: cd executor && node index.js".to_string(),
    };

    // Startup message
    let auto_bet_info = if AUTO_BET_ENABLED {
        format!("🤖 <b>AUTO-BET: ON</b>\n   \
                 Edge ≥{:.0}% + HIGH → auto-sázka ${:.0}\n   \
                 Max {}/session | Odds {:.2}-{:.2}\n\
                 💰 <b>AUTO-CLAIM: ON</b> (každých {}s)\n   \
                 Výhry a refundy se automaticky vybírají!",
                AUTO_BET_MIN_EDGE_PCT, AUTO_BET_STAKE_USD,
                AUTO_BET_MAX_PER_SESSION, AUTO_BET_MIN_ODDS, AUTO_BET_MAX_ODDS,
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
    // Bets that have been settled and claimed (to avoid re-processing)
    let mut settled_bet_ids: HashSet<String> = HashSet::new();
    // Running profit/loss tracker
    let mut total_wagered: f64 = 0.0;
    let mut total_returned: f64 = 0.0;
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

                // 1. Check /state for cross-bookmaker odds anomalies
                match client.get(format!("{}/state", feed_hub_url)).send().await {
                    Ok(resp) => {
                        match resp.json::<StateResponse>().await {
                            Ok(state) => {
                                // === 1. SCORE EDGE detection (primary strategy!) ===
                                let score_edges = find_score_edges(&state, &mut score_tracker);
                                for edge in &score_edges {
                                    let alert_key = format!("score:{}:{}:{}-{}", edge.match_key, edge.leading_side, edge.score1, edge.score2);
                                    if already_alerted.contains(&alert_key) {
                                        continue;
                                    }

                                    alert_counter += 1;
                                    let aid = alert_counter;

                                    // Store as OddsAnomaly for YES/BET compatibility
                                    let anomaly = OddsAnomaly {
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
                                        outcome_id: edge.outcome_id.clone(),
                                        chain: edge.chain.clone(),
                                    };

                                    let azuro_odds = if edge.leading_side == 1 { edge.azuro_w1 } else { edge.azuro_w2 };
                                    let leading_team = if edge.leading_side == 1 { &edge.team1 } else { &edge.team2 };

                                    // === AUTO-BET: place bet automatically on high-confidence edges ===
                                    let cond_id_str = anomaly.condition_id.as_deref().unwrap_or("").to_string();
                                    let match_key_for_bet = edge.match_key.clone();
                                    let already_bet_this = already_bet_matches.contains(&match_key_for_bet)
                                        || (!cond_id_str.is_empty() && already_bet_conditions.contains(&cond_id_str));

                                    if already_bet_this {
                                        info!("🚫 DEDUP: Already bet on {} (cond={}), skipping auto-bet",
                                            match_key_for_bet, cond_id_str);
                                    }

                                    let should_auto_bet = AUTO_BET_ENABLED
                                        && edge.confidence == "HIGH"
                                        && edge.edge_pct >= AUTO_BET_MIN_EDGE_PCT
                                        && azuro_odds >= AUTO_BET_MIN_ODDS
                                        && azuro_odds <= AUTO_BET_MAX_ODDS
                                        && auto_bet_count < AUTO_BET_MAX_PER_SESSION
                                        && anomaly.condition_id.is_some()
                                        && anomaly.outcome_id.is_some()
                                        && !already_bet_this;

                                    if should_auto_bet {
                                        // AUTO-BET!
                                        let condition_id = anomaly.condition_id.as_ref().unwrap().clone();
                                        let outcome_id = anomaly.outcome_id.as_ref().unwrap().clone();
                                        let stake = AUTO_BET_STAKE_USD;

                                        info!("🤖 AUTO-BET #{}: {} @ {:.2} ${:.2} edge={:.1}%",
                                            aid, leading_team, azuro_odds, stake, edge.edge_pct);

                                        // Send alert WITH auto-bet notice
                                        let msg = format!(
                                            "🤖 <b>#{} AUTO-BET</b> 🟢 HIGH\n\
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
                                            edge.team1, edge.team2,
                                            edge.score1, edge.score2, edge.prev_score1, edge.prev_score2,
                                            leading_team,
                                            edge.edge_pct,
                                            azuro_odds,
                                            stake
                                        );
                                        let _ = tg_send_message(&client, &token, chat_id, &msg).await;

                                        // Place the bet
                                        let min_odds = (azuro_odds * 0.95 * 1e12) as u64;
                                        let amount_raw = (stake * 1e6) as u64;
                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "team1": edge.team1,
                                            "team2": edge.team2,
                                        });

                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                match resp.json::<ExecutorBetResponse>().await {
                                                    Ok(br) => {
                                                        if let Some(err) = &br.error {
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>AUTO-BET #{} FAILED</b>\n\nError: {}", aid, err)
                                                            ).await;
                                                        } else {
                                                            auto_bet_count += 1;
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let bet_state = br.state.as_deref().unwrap_or("?");

                                                            // === DEDUP: record bet to prevent duplicates ===
                                                            already_bet_matches.insert(match_key_for_bet.clone());
                                                            already_bet_conditions.insert(cond_id_str.clone());
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
                                                                    graph_bet_id: None,
                                                                    token_id: None,
                                                                });
                                                                // Persist pending claim (tokenId discovered later via /bet/:id)
                                                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                    .create(true).append(true)
                                                                    .open(pending_claims_path) {
                                                                    use std::io::Write;
                                                                    let _ = writeln!(f, "?|{}|{}|{}|{}|{}|{}",
                                                                        bet_id, edge.match_key,
                                                                        leading_team, stake, azuro_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                            }

                                                            let result_msg = if is_dry_run {
                                                                format!("🧪 <b>AUTO-BET #{} DRY-RUN</b>\n{} @ {:.2} ${:.2}\n⚠️ Nebyl odeslán on-chain.", aid, leading_team, azuro_odds, stake)
                                                            } else {
                                                                format!(
                                                                    "✅ <b>AUTO-BET #{} PLACED!</b>\n\
                                                                     {} @ {:.2} | ${:.2}\n\
                                                                     Bet ID: <code>{}</code>\n\
                                                                     State: {}\n\
                                                                     Auto-bets session: {}/{}",
                                                                    aid, leading_team, azuro_odds, stake,
                                                                    bet_id, bet_state,
                                                                    auto_bet_count, AUTO_BET_MAX_PER_SESSION
                                                                )
                                                            };
                                                            let _ = tg_send_message(&client, &token, chat_id, &result_msg).await;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            &format!("❌ Auto-bet #{} response error: {}", aid, e)
                                                        ).await;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("❌ Executor offline pro auto-bet #{}: {}", aid, e)
                                                ).await;
                                            }
                                        }
                                    } else {
                                        // Manual alert (MEDIUM confidence or auto-bet disabled)
                                        let msg = format_score_edge_alert(edge, aid);
                                        if let Err(e) = tg_send_message(&client, &token, chat_id, &msg).await {
                                            error!("Failed to send score edge alert: {}", e);
                                        }
                                    }

                                    info!("⚡ Score Edge #{} sent: {} {}-{} side={} edge={:.1}%",
                                        aid, edge.match_key, edge.score1, edge.score2, edge.leading_side, edge.edge_pct);
                                    sent_alerts.push(SentAlert {
                                        match_key: alert_key,
                                        sent_at: Utc::now(),
                                    });
                                    alert_map.insert(aid, anomaly);
                                }

                                // === 2. Cross-book odds anomaly (secondary strategy) ===
                                let anomalies = find_odds_anomalies(&state);
                                let mut actually_sent = score_edges.len();
                                let total_anomalies = anomalies.len();
                                for anomaly in anomalies {
                                    let alert_key = format!("{}:{}:{}", anomaly.match_key, anomaly.value_side, anomaly.azuro_bookmaker);
                                    if already_alerted.contains(&alert_key) {
                                        continue;
                                    }

                                    alert_counter += 1;
                                    let aid = alert_counter;

                                    let msg = format_anomaly_alert(&anomaly, aid);
                                    if let Err(e) = tg_send_message(&client, &token, chat_id, &msg).await {
                                        error!("Failed to send alert: {}", e);
                                    } else {
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

            // === AUTO-CASHOUT check ===
            _ = cashout_ticker.tick() => {
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
                                    if let Some(tid) = discovered_tid {
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
                    let check_body = serde_json::json!({
                        "tokenId": token_id,
                    });
                    let cashout_check = match client.post(format!("{}/check-cashout", executor_url))
                        .json(&check_body).send().await {
                        Ok(r) => r.json::<CashoutCheckResponse>().await.ok(),
                        Err(_) => None,
                    };

                    if let Some(check) = cashout_check {
                        if check.available.unwrap_or(false) {
                            if let Some(odds_str) = &check.cashout_odds {
                                let cashout_odds: f64 = odds_str.parse().unwrap_or(0.0);
                                // Only cashout if profitable (cashout_odds > bet_odds means we can lock profit)
                                let profit_pct = if bet.odds > 0.0 {
                                    (cashout_odds / bet.odds - 1.0) * 100.0
                                } else { 0.0 };

                                if profit_pct >= CASHOUT_MIN_PROFIT_PCT {
                                    info!("Auto-cashout #{}: odds {:.3} → cashout {:.3} (+{:.1}%)",
                                        bet.alert_id, bet.odds, cashout_odds, profit_pct);

                                    // Execute cashout
                                    let cashout_body = serde_json::json!({
                                        "tokenId": token_id,
                                    });
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
                if active_bets.is_empty() { continue; }

                let mut bets_to_remove: Vec<String> = Vec::new();
                let mut tokens_to_claim: Vec<String> = Vec::new();
                let mut claim_details: Vec<(u32, String, String, String, f64, f64, String)> = Vec::new(); // (alert_id, team1, team2, value_team, amount, odds, result)

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
                                // payout = 0 and not pending = LOST
                                total_wagered += bet.amount_usd;
                                let loss_msg = format!(
                                    "❌ <b>PROHRA</b>\n\n\
                                     {} vs {}\n\
                                     Sázka: <b>{}</b> @ {:.2} — ${:.2}\n\
                                     Výsledek: <b>PROHRA</b> — -${:.2}",
                                    bet.team1, bet.team2,
                                    bet.value_team, bet.odds, bet.amount_usd,
                                    bet.amount_usd
                                );
                                let _ = tg_send_message(&client, &token, chat_id, &loss_msg).await;
                                settled_bet_ids.insert(bet.bet_id.clone());
                                bets_to_remove.push(bet.bet_id.clone());
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
                        if let Some(tid) = discovered_tid {
                            bet.token_id = Some(tid.clone());
                            // Update pending_claims file with real tokenId
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true).append(true)
                                .open(pending_claims_path) {
                                use std::io::Write;
                                let _ = writeln!(f, "{}|{}|{}|{}|{}|{}|{}",
                                    tid, bet.bet_id, bet.match_key,
                                    bet.value_team, bet.amount_usd, bet.odds,
                                    Utc::now().to_rfc3339());
                            }
                            info!("🔍 Discovered tokenId {} for bet {}", tid, bet.bet_id);
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

                    // Track wagered amount
                    total_wagered += bet.amount_usd;

                    // If we have a token_id, try to claim payout
                    if let Some(tid) = &bet.token_id {
                        match effective_result.as_str() {
                            "Won" | "Canceled" => {
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
                    info!("💰 Claiming {} settled bets: {:?}", tokens_to_claim.len(), tokens_to_claim);

                    let claim_body = serde_json::json!({
                        "tokenIds": tokens_to_claim,
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

                                    // Build detailed notification
                                    let mut msg = String::from("💰 <b>AUTO-CLAIM úspěšný!</b>\n\n");
                                    for (aid, _t1, _t2, vt, amt, odds, res) in &claim_details {
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

                                    let pnl = total_returned - total_wagered;
                                    let pnl_sign = if pnl >= 0.0 { "+" } else { "" };

                                    msg.push_str(&format!(
                                        "\n💵 Vyplaceno: <b>${:.2}</b>\n\
                                         📤 TX: <code>{}</code>\n\
                                         💰 <b>Nový zůstatek: {} USDT</b>\n\n\
                                         📊 Session P/L: <b>{}{:.2} USDT</b>\n\
                                         (vsazeno: ${:.2}, vráceno: ${:.2})",
                                        total_payout, tx, new_balance,
                                        pnl_sign, pnl,
                                        total_wagered, total_returned
                                    ));

                                    let _ = tg_send_message(&client, &token, chat_id, &msg).await;
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
                }

                // Remove settled bets from active list
                active_bets.retain(|b| !bets_to_remove.contains(&b.bet_id));

                // Rewrite pending_claims file with remaining active bets only
                if !bets_to_remove.is_empty() {
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
            }

            // === PORTFOLIO STATUS REPORT (every 30 min) ===
            _ = portfolio_ticker.tick() => {
                let mut msg = String::from("📊 <b>PORTFOLIO STATUS</b>\n\n");
                let uptime_mins = (Utc::now() - session_start).num_minutes();
                msg.push_str(&format!("⏱️ Uptime: {}h {}min\n\n", uptime_mins / 60, uptime_mins % 60));

                // Get wallet balance from executor
                match client.get(format!("{}/health", executor_url)).send().await {
                    Ok(resp) => {
                        if let Ok(h) = resp.json::<ExecutorHealthResponse>().await {
                            let balance = h.balance.as_deref().unwrap_or("?");
                            msg.push_str(&format!("💰 <b>Wallet: {} USDT</b>\n", balance));
                        }
                    }
                    Err(_) => msg.push_str("💰 Wallet: ❌ executor offline\n"),
                }

                // Active bets summary
                if active_bets.is_empty() {
                    msg.push_str("🎰 Aktivní sázky: 0\n");
                } else {
                    msg.push_str(&format!("🎰 Aktivní sázky: <b>{}</b>\n", active_bets.len()));
                    for b in &active_bets {
                        msg.push_str(&format!("  • {} @ {:.2} ${:.2}\n", b.value_team, b.odds, b.amount_usd));
                    }
                }

                // Session P&L
                let pnl = total_returned - total_wagered;
                let (pnl_sign, pnl_emoji) = if pnl >= 0.0 { ("+", "📈") } else { ("", "📉") };
                msg.push_str(&format!("\n{} Session P/L: <b>{}{:.2} USDT</b>\n", pnl_emoji, pnl_sign, pnl));
                msg.push_str(&format!("   Vsazeno: ${:.2} | Vráceno: ${:.2}\n", total_wagered, total_returned));
                msg.push_str(&format!("   Auto-bets: {}/{}\n", auto_bet_count, AUTO_BET_MAX_PER_SESSION));

                // Feed-hub live info
                match client.get(format!("{}/state", feed_hub_url)).send().await {
                    Ok(resp) => {
                        if let Ok(state) = resp.json::<StateResponse>().await {
                            let azuro_count = state.odds.iter().filter(|o| o.payload.bookmaker.starts_with("azuro_")).count();
                            let map_winner_count = state.odds.iter().filter(|o| {
                                o.payload.market.as_deref().map(|m| m.starts_with("map")).unwrap_or(false)
                            }).count();
                            let tennis_count = state.odds.iter().filter(|o| {
                                o.payload.sport.as_deref() == Some("tennis")
                            }).count();
                            msg.push_str(&format!(
                                "\n📡 Live: {} zápasů | Azuro: {} odds ({} map, {} tennis)\n",
                                state.live_items, azuro_count, map_winner_count, tennis_count
                            ));
                        }
                    }
                    Err(_) => {}
                }

                let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                info!("📊 Portfolio report sent");
            }

            // === Check Telegram for user replies ===
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                match tg_get_updates(&client, &token, update_offset).await {
                    Ok(updates) => {
                        for u in &updates.result {
                            update_offset = u.update_id + 1;
                            if let Some(msg) = &u.message {
                                if msg.chat.id != chat_id { continue; }
                                let text = msg.text.as_deref().unwrap_or("").trim();

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
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format_anomaly_alert(top, alert_counter)).await;
                                                        }
                                                    }
                                                }
                                                Err(_) => { let _ = tg_send_message(&client, &token, chat_id, "❌ /state parse error").await; }
                                            }
                                        }
                                        Err(e) => { let _ = tg_send_message(&client, &token, chat_id, &format!("❌ Feed Hub offline: {}", e)).await; }
                                    }

                                } else if text == "/bets" {
                                    if active_bets.is_empty() {
                                        let _ = tg_send_message(&client, &token, chat_id, "📭 Žádné aktivní sázky.").await;
                                    } else {
                                        let bets_text = active_bets.iter().map(|b| {
                                            format!("• #{} {} ${:.2} @ {:.2} ({})",
                                                b.alert_id, b.value_team, b.amount_usd, b.odds, b.match_key)
                                        }).collect::<Vec<_>>().join("\n");
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!("🎰 <b>Aktivní sázky ({})</b>\n\n{}", active_bets.len(), bets_text)
                                        ).await;
                                    }

                                } else if text == "/help" {
                                    let _ = tg_send_message(&client, &token, chat_id,
                                        "🤖 <b>RustMisko Alert Bot v4</b>\n\n\
                                         CS2 + Tennis | Match + Map Winner | Auto-bet + Auto-claim.\n\n\
                                         <b>Commands:</b>\n\
                                         /status — systém + executor + wallet\n\
                                         /portfolio — peněženka + P/L + sázky\n\
                                         /odds — aktuální anomálie\n\
                                         /bets — aktivní sázky\n\
                                         /help — tato zpráva\n\n\
                                         <b>Na alert odpověz:</b>\n\
                                         <code>3 YES $5</code> — sázka $5 na alert #3\n\
                                         <code>3 NO</code> — skip alert #3\n\n\
                                         Auto-bet: edge ≥15% HIGH → auto $2\n\
                                         Auto-claim: výhry + refundy automaticky.\n\
                                         Portfolio report: každých 30 min."
                                    ).await;

                                } else if text == "/portfolio" {
                                    // On-demand portfolio report
                                    let mut msg = String::from("📊 <b>PORTFOLIO</b>\n\n");
                                    let uptime_mins = (Utc::now() - session_start).num_minutes();
                                    msg.push_str(&format!("⏱️ Session: {}h {}min\n\n", uptime_mins / 60, uptime_mins % 60));

                                    match client.get(format!("{}/health", executor_url)).send().await {
                                        Ok(resp) => {
                                            if let Ok(h) = resp.json::<ExecutorHealthResponse>().await {
                                                let balance = h.balance.as_deref().unwrap_or("?");
                                                let wallet = h.wallet.as_deref().unwrap_or("?");
                                                msg.push_str(&format!("💰 <b>{} USDT</b>\n", balance));
                                                msg.push_str(&format!("🔑 <code>{}</code>\n", wallet));
                                            }
                                        }
                                        Err(_) => msg.push_str("❌ Executor offline\n"),
                                    }

                                    if !active_bets.is_empty() {
                                        msg.push_str(&format!("\n🎰 <b>Aktivní sázky ({})</b>\n", active_bets.len()));
                                        let total_at_risk: f64 = active_bets.iter().map(|b| b.amount_usd).sum();
                                        for b in &active_bets {
                                            msg.push_str(&format!("  • {} @ {:.2} ${:.2}\n", b.value_team, b.odds, b.amount_usd));
                                        }
                                        msg.push_str(&format!("  Celkem ve hře: <b>${:.2}</b>\n", total_at_risk));
                                    } else {
                                        msg.push_str("\n🎰 Žádné aktivní sázky\n");
                                    }

                                    let pnl = total_returned - total_wagered;
                                    let (pnl_sign, pnl_emoji) = if pnl >= 0.0 { ("+", "📈") } else { ("", "📉") };
                                    msg.push_str(&format!("\n{} <b>P/L: {}{:.2} USDT</b>\n", pnl_emoji, pnl_sign, pnl));
                                    msg.push_str(&format!("Vsazeno: ${:.2} | Vráceno: ${:.2}\n", total_wagered, total_returned));
                                    msg.push_str(&format!("Auto-bets: {}/{}\n", auto_bet_count, AUTO_BET_MAX_PER_SESSION));

                                    let _ = tg_send_message(&client, &token, chat_id, &msg).await;

                                // === YES reply: place bet ===
                                } else if let Some((mut aid, amount)) = parse_yes_reply(text) {
                                    // aid=0 means "latest alert"
                                    if aid == 0 {
                                        aid = alert_counter;
                                    }
                                    if let Some(anomaly) = alert_map.get(&aid) {
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
                                        let outcome_id = match &anomaly.outcome_id {
                                            Some(o) => o.clone(),
                                            None => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!("⚠️ Alert #{} nemá outcome_id — nelze automaticky vsadit.", aid)
                                                ).await;
                                                continue;
                                            }
                                        };

                                        let azuro_odds = if anomaly.value_side == 1 { anomaly.azuro_w1 } else { anomaly.azuro_w2 };
                                        let value_team = if anomaly.value_side == 1 { &anomaly.team1 } else { &anomaly.team2 };

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
                                        let min_odds = (azuro_odds * 0.95 * 1e12) as u64; // 5% slippage, raw format
                                        let amount_raw = (amount * 1e6) as u64; // USDT 6 decimals

                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "team1": anomaly.team1,
                                            "team2": anomaly.team2,
                                        });

                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                match resp.json::<ExecutorBetResponse>().await {
                                                    Ok(br) => {
                                                        if let Some(err) = &br.error {
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("❌ <b>BET FAILED #{}</b>\n\nError: {}", aid, err)
                                                            ).await;
                                                        } else {
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let state = br.state.as_deref().unwrap_or("?");

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
                                                                    graph_bet_id: None,
                                                                    token_id: None,
                                                                });
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
