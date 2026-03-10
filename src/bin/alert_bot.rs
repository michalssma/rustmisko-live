#![recursion_limit = "256"]

//! Telegram Alert Bot pro CS2 odds anomálie
//!
//! Standalone binary — polluje feed-hub /opportunities endpoint,
//! detekuje odds discrepancy mezi Azuro a trhem, posílá Telegram alerty.
//! Miša odpoví YES $X / NO a bot umístí sázku přes Azuro executor sidecar.
//! Auto-cashout monitoruje aktivní sázky a cashoutuje při profitu.
//!
//! Spuštění:
//!   $env:TELEGRAM_BOT_TOKEN="<token>"
//!   $env:TELEGRAM_CHAT_ID="6458129071"
//!   $env:FEED_HUB_URL="http://127.0.0.1:8081"
//!   $env:EXECUTOR_URL="http://127.0.0.1:3030"  # Node.js sidecar
//!   cargo run --bin alert_bot

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn, error, debug};
use tracing_subscriber::{EnvFilter, fmt};
use std::path::Path;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ====================================================================
// Config
// ====================================================================

const POLL_INTERVAL_SECS: u64 = 2;  // 2s — near-instant detection of Tipsport score changes!
/// Minimum edge % to trigger alert (all tiers)
const MIN_EDGE_PCT: f64 = 8.0;
/// Don't re-alert same match+score+side within this window
const ALERT_COOLDOWN_SECS: i64 = 90; // reduced spam — 90s between re-alerts on same match
/// Manual offers: max 1 nabídka na stejný match_key za tento interval
const MANUAL_MATCH_COOLDOWN_SECS: i64 = 180;
/// Auto-cashout check interval
const CASHOUT_CHECK_SECS: u64 = 30;
/// Minimum profit % to auto-cashout
const CASHOUT_MIN_PROFIT_PCT: f64 = 3.0;
/// Minimum score-edge % to trigger alert
/// Relaxed 6% → 5%: score is fact-based evidence, safe to lower threshold
const MIN_SCORE_EDGE_PCT: f64 = 5.0;
/// Score edge cooldown per match (seconds)
const SCORE_EDGE_COOLDOWN_SECS: i64 = 60; // 60s — reduced spam, still catches score changes
/// === AUTO-BET CONFIG ===
const AUTO_BET_ENABLED: bool = true;
/// Base stake per auto-bet in USD
const AUTO_BET_STAKE_USD: f64 = 3.0;
/// Tennis/basketball score-edge: activated at $0.50 (was paper-trading $0.00)
/// Data shows tennis anomaly 1.50-1.70 is profitable; score-edge with 30% min is even stricter
const AUTO_BET_STAKE_LOW_USD: f64 = 0.50;
/// Minimum Azuro odds to auto-bet (skip heavy favorites, prevents massive risk/reward leakage)
/// Raised 1.40→1.70: at 59% WR break-even is 1/0.59=1.695 — below 1.70 is systematically -EV
const AUTO_BET_MIN_ODDS: f64 = 1.70;
/// Maximum odds for auto-bet (skip extreme underdogs)
/// Relaxed 2.50→3.00: score-edge is fact-based, safe to bet slightly wider
const AUTO_BET_MAX_ODDS: f64 = 3.00;
/// CS2 map_winner exception: allow higher odds (score-based edge is more reliable on maps)
const AUTO_BET_MAX_ODDS_CS2_MAP: f64 = 3.00;
/// Lowered 2.5→1.5: map-level bets are correlated within same series, limiting max multiplier reduces double-exposure risk (KRU map2+map3 = $2.88 at 2.5x)
const SCORE_EDGE_STAKE_MAX_MULT: f64 = 1.5;
const ESPORTS_MAPLEVEL_MATCH_EDGE_MIN_PCT: f64 = 16.0;
const ESPORTS_MAPLEVEL_MATCH_MAX_ODDS: f64 = 1.65;
/// Manual/Reaction default stake in USD
const MANUAL_BET_DEFAULT_USD: f64 = 3.0;
/// Manual/Reaction max odds cap (risk guard)
const MANUAL_BET_MAX_ODDS: f64 = 2.00;
/// Manual/Reaction alert must be fresh (prevents betting stale/reset markets)
const MANUAL_ALERT_MAX_AGE_SECS: i64 = 25;
/// Block betting on generic esports keys unless they are promoted to a concrete
/// sport family from hard runtime data.
/// Recent settle audit (2026-03-10) shows explicit cs2:: score-edge is profitable,
/// while unresolved generic esports:: auto-bets are the main current loss bucket.
/// Keep alerts and manual review, but only auto-execute generic esports after a
/// high-confidence promotion coming from resolved_sport / live payload sport.
const BLOCK_GENERIC_ESPORTS_BETS: bool = true;
/// Retry settings — jittered backoff for live market condition pauses
/// GPT audit: max 2 retry for ConditionNotRunning; 3rd attempt is wasted latency
const AUTO_BET_RETRY_MAX: usize = 2;
/// Jitter base delays per retry attempt — actual = base + rand(0..base/2)
const AUTO_BET_RETRY_DELAYS_MS: [u64; 2] = [80, 200];
/// Min-odds fallback step for one rescue retry (e.g. 0.84 -> 0.76)
/// Increased 0.06→0.08 to give rescue-retry more room (52x MinOddsReject in 24h audit)
const MIN_ODDS_FALLBACK_STEP: f64 = 0.08;
/// Retry condition-paused only for reasonably fresh conditions
const RETRY_CONDITION_MAX_AGE_MS: u64 = 1200;
/// Small pause before retrying after execution ID remap
const REMAP_RETRY_DELAY_MS: u64 = 120;
/// Signal TTL — reject bet if decision is older than this (seconds)
const SIGNAL_TTL_SECS: u64 = 3;
/// Persistent dedup history lookback (hours) — older entries are ignored on startup.
/// Prevents "blocked forever" behavior on recurring match keys.
const DEDUP_HISTORY_LOOKBACK_HOURS: i64 = 8;
/// === PRE-FLIGHT GATING (condition-state pivot, 2026-02-28) ===
/// Max age since last GQL sighting of this condition as Active
/// If condition_age_ms > this, DROP before sending to executor
/// ROLLBACK: set to 999_999 to effectively disable pre-flight gate
/// Raised 2000→4000: GQL poll cycle is 3s, 2s was causing valid conditions to be dropped
/// (condition fresh at t=0, bet decision at t=2.1s → age=2100ms > 2000ms → DROP = false negative)
const CONDITION_MAX_AGE_MS: u64 = 4000;
/// Base chain poll cadence is much slower than Polygon WS/GQL cadence.
/// Tightened 120s→30s: still allows Base bets, but cuts truly stale conditions.
const CONDITION_MAX_AGE_MS_BASE: u64 = 30_000;
/// Max total pipeline time for live bets; drop if exceeded (condition likely paused)
/// ROLLBACK: set to 999_999 to effectively disable pipeline budget
const PIPELINE_BUDGET_MS: u64 = 1200;
/// Suspended market odds thresholds — market odds outside this range → skip
/// ROLLBACK: set MIN to 0.0 and MAX to 999_999.0 to disable
const SUSPENDED_MARKET_MIN_ODDS: f64 = 1.05;
const SUSPENDED_MARKET_MAX_ODDS: f64 = 50.0;
/// Slippage guard factors (minOdds = displayed_odds * factor)
/// Relaxed 0.88→0.84 after 24h audit (52x MinOddsReject, 40 tennis, 7 esports, 4 basket, 1 football).
/// Live markets can move 12-18% between 3s poll cycles — 0.84 allows 16% slippage on first try.
const MIN_ODDS_FACTOR_DEFAULT: f64 = 0.84;
/// Tennis moves fastest around points; 0.82 allows 18% slippage on first try.
/// With fallback step 0.08, rescue retry hits 0.74 — generous but still capped by AUTO_BET_MIN_ODDS=1.15.
const MIN_ODDS_FACTOR_TENNIS: f64 = 0.82;
/// Basketball live also has fast score swings; use intermediate factor.
const MIN_ODDS_FACTOR_BASKETBALL: f64 = 0.83;
/// Prefer auto-bet only when anomaly is confirmed by at least N market sources
/// Restored 1→2: single-source bets (e.g. Tipsport only) showed higher loss rate in production data
const AUTO_BET_MIN_MARKET_SOURCES: usize = 2;
/// Ignore stale odds snapshots older than this threshold
const MAX_ODDS_AGE_SECS: i64 = 20;
/// Maximum concurrent pending bets (inflight guard)
const MAX_CONCURRENT_PENDING: usize = 8;
/// Loss streak cooldown: consecutive LOST count to trigger pause
const LOSS_STREAK_PAUSE_THRESHOLD: usize = 4;
/// Loss streak pause duration (seconds)
const LOSS_STREAK_PAUSE_SECS: u64 = 180;
/// Minimum bankroll to allow auto-bet (skip if below)
const MIN_BANKROLL_USD: f64 = 10.0;
/// Periodic ledger-based recovery cadence for unresolved accepted bets.
const LEDGER_RECONCILE_EVERY_CLAIM_TICKS: u32 = 5;
/// Unresolved accepted bets older than this should be surfaced explicitly.
const UNRESOLVED_ACCEPTED_STALE_HOURS: i64 = 12;

fn condition_max_age_limit_ms(chain: Option<&str>, azuro_bookmaker: &str) -> u64 {
    let chain_l = chain.unwrap_or("").to_lowercase();
    let bookmaker_l = azuro_bookmaker.to_lowercase();
    if chain_l == "base" || bookmaker_l.contains("azuro_base") {
        CONDITION_MAX_AGE_MS_BASE
    } else {
        CONDITION_MAX_AGE_MS
    }
}
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
// WS STATE GATE — real-time condition state from Azuro V3 streams
// ====================================================================

/// Azuro V3 WebSocket streams endpoint (production) — same as azuro_poller shadow
const WS_GATE_URL: &str = "wss://streams.onchainfeed.org/v1/streams/feed";
/// WS update is considered stale after this many ms
/// Keep this strict: stale WS effectively behaves like no WS (and we prefer skipping over executor rejects)
const WS_STALE_MS: u64 = 500;
/// Reconnect backoff sequence (ms)
const WS_GATE_BACKOFF_MS: &[u64] = &[500, 1_000, 2_000, 5_000, 15_000];
/// Subscribe throttle: don't re-send SubscribeConditions more than once per N seconds
/// Lowered 5→2: reduces NoData race window (bet arrives before WS subscribe response)
/// At 5s throttle, a bet on a new condition always sees NoData on first attempt
const WS_SUBSCRIBE_THROTTLE_SECS: u64 = 2;
/// Maximum conditions to track before GC of stale entries
const WS_MAX_TRACKED_CONDITIONS: usize = 500;

// ── WS types ──

/// Cached condition state from Azuro WS stream
#[derive(Debug, Clone)]
struct WsConditionEntry {
    /// "Active", "Paused", "Resolved", "Canceled", "Created"
    state: String,
    /// When this entry was last updated (monotonic)
    updated_at: std::time::Instant,
}

/// Thread-safe shared WS condition cache (condition_id → entry)
type WsConditionCache = Arc<RwLock<HashMap<String, WsConditionEntry>>>;

/// WS subscribe message format (matches Azuro V3 protocol)
#[derive(Serialize)]
struct WsGateSubscribeMsg {
    event: String,
    conditions: Vec<String>,
    environment: String,
}

/// WS incoming message (generic — we match on event field)
#[derive(Deserialize, Debug)]
struct WsGateIncoming {
    event: Option<String>,
    id: Option<String>,
    data: Option<serde_json::Value>,
}

/// Result of checking WS cache for a condition before betting
#[derive(Debug)]
enum WsGateResult {
    /// WS confirms condition is Active + fresh → proceed
    Active { age_ms: u64 },
    /// WS says condition is NOT Active → DROP immediately
    NotActive { state: String, age_ms: u64 },
    /// WS data is stale (older than WS_STALE_MS) → fallback to GQL
    Stale { age_ms: u64 },
    /// No WS data for this condition → fallback to GQL
    NoData,
    /// WS gate disabled (kill-switch off) → fallback to GQL
    Disabled,
}

/// Check WS condition cache for pre-flight gate decision
fn ws_gate_check(cache: &HashMap<String, WsConditionEntry>, condition_id: &str, gate_enabled: bool) -> WsGateResult {
    if !gate_enabled {
        return WsGateResult::Disabled;
    }
    match cache.get(condition_id) {
        Some(entry) => {
            let age_ms = entry.updated_at.elapsed().as_millis() as u64;
            if age_ms > WS_STALE_MS {
                WsGateResult::Stale { age_ms }
            } else if entry.state == "Active" {
                WsGateResult::Active { age_ms }
            } else {
                WsGateResult::NotActive { state: entry.state.clone(), age_ms }
            }
        }
        None => WsGateResult::NoData,
    }
}

/// Async WS condition gate daemon.
/// Maintains live connection to Azuro WS stream, receives condition state updates,
/// populates shared cache that the main loop reads for pre-flight gating.
///
/// Subscribes to condition IDs received via `sub_rx` channel.
async fn run_ws_gate(
    cache: WsConditionCache,
    mut sub_rx: mpsc::Receiver<Vec<String>>,
) {
    let mut backoff_idx: usize = 0;
    // Accumulate all condition IDs we should be subscribed to
    let mut all_subscribed: HashSet<String> = HashSet::new();
    // Pending IDs that arrived while disconnected
    let mut pending_subscribe: Vec<String> = Vec::new();

    loop {
        info!("[WS-GATE] Connecting to {}", WS_GATE_URL);

        let ws_stream = match tokio_tungstenite::connect_async(WS_GATE_URL).await {
            Ok((stream, resp)) => {
                info!("[WS-GATE] Connected! HTTP {} (subscribed: {})", resp.status(), all_subscribed.len());
                backoff_idx = 0;
                stream
            }
            Err(e) => {
                let delay = WS_GATE_BACKOFF_MS.get(backoff_idx).copied().unwrap_or(15_000);
                warn!("[WS-GATE] Connect failed: {} — retry in {}ms", e, delay);
                backoff_idx = (backoff_idx + 1).min(WS_GATE_BACKOFF_MS.len() - 1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
        };

        let (mut ws_sink, mut ws_read) = ws_stream.split();

        // On reconnect: clear old subscribed set, re-subscribe to everything
        let mut needs_resub = !all_subscribed.is_empty();
        let mut resub_ids: Vec<String> = all_subscribed.iter().cloned().collect();
        // Also add any pending from while disconnected
        for id in pending_subscribe.drain(..) {
            if all_subscribed.insert(id.clone()) {
                resub_ids.push(id);
            }
        }
        let mut last_subscribe_ts = std::time::Instant::now()
            .checked_sub(Duration::from_secs(WS_SUBSCRIBE_THROTTLE_SECS + 1))
            .unwrap_or_else(std::time::Instant::now);

        let disconnect_reason: String;
        loop {
            // ── Handle new subscribe requests from main loop ──
            // Drain channel (non-blocking)
            loop {
                match sub_rx.try_recv() {
                    Ok(new_ids) => {
                        let mut fresh: Vec<String> = Vec::new();
                        for id in new_ids {
                            if all_subscribed.insert(id.clone()) {
                                fresh.push(id);
                            }
                        }
                        if !fresh.is_empty() {
                            resub_ids.extend(fresh);
                            needs_resub = true;
                        }
                    }
                    Err(_) => break,
                }
            }

            // ── Send SubscribeConditions if we have new IDs and throttle allows ──
            if needs_resub && !resub_ids.is_empty()
                && last_subscribe_ts.elapsed() >= Duration::from_secs(WS_SUBSCRIBE_THROTTLE_SECS)
            {
                let msg = serde_json::to_string(&WsGateSubscribeMsg {
                    event: "SubscribeConditions".to_string(),
                    conditions: resub_ids.clone(),
                    environment: "polygon".to_string(),
                }).unwrap();
                if let Err(e) = ws_sink.send(WsMessage::Text(msg.into())).await {
                    disconnect_reason = format!("subscribe send error: {}", e);
                    break;
                }
                info!("[WS-GATE] Subscribed {} conditions (total tracked: {})",
                    resub_ids.len(), all_subscribed.len());
                resub_ids.clear();
                needs_resub = false;
                last_subscribe_ts = std::time::Instant::now();
            }

            // ── Read next WS message (with 3s timeout for responsiveness) ──
            let msg = tokio::time::timeout(Duration::from_secs(3), ws_read.next()).await;

            match msg {
                Ok(Some(Ok(WsMessage::Text(txt)))) => {
                    match serde_json::from_str::<WsGateIncoming>(&txt) {
                        Ok(incoming) => {
                            let event = incoming.event.as_deref().unwrap_or("?");
                            match event {
                                "ConditionUpdated" => {
                                    let cid = incoming.id.as_deref().unwrap_or("?").to_string();
                                    let state_str = incoming.data.as_ref()
                                        .and_then(|d| d.get("state"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?")
                                        .to_string();

                                    // Update cache
                                    let mut cache_w = cache.write().await;
                                    cache_w.insert(cid.clone(), WsConditionEntry {
                                        state: state_str.clone(),
                                        updated_at: std::time::Instant::now(),
                                    });

                                    // GC: if cache too large, remove oldest entries
                                    if cache_w.len() > WS_MAX_TRACKED_CONDITIONS {
                                        let mut entries: Vec<(String, std::time::Instant)> =
                                            cache_w.iter().map(|(k, v)| (k.clone(), v.updated_at)).collect();
                                        entries.sort_by_key(|e| e.1);
                                        let to_remove = cache_w.len() - WS_MAX_TRACKED_CONDITIONS / 2;
                                        for (k, _) in entries.into_iter().take(to_remove) {
                                            cache_w.remove(&k);
                                        }
                                        debug!("[WS-GATE] GC: removed {} stale entries", to_remove);
                                    }
                                    drop(cache_w);

                                    debug!("[WS-GATE] ConditionUpdated cid={} state={}", &cid[..cid.len().min(16)], state_str);
                                }
                                "SubscribedToConditions" => {
                                    let count = incoming.data.as_ref()
                                        .and_then(|d| d.as_array())
                                        .map(|a| a.len())
                                        .unwrap_or(0);
                                    info!("[WS-GATE] Server confirmed subscription: {} condition IDs", count);
                                }
                                _ => {
                                    debug!("[WS-GATE] event={}", event);
                                }
                            }
                        }
                        Err(e) => {
                            debug!("[WS-GATE] JSON parse error: {} raw={}", e, &txt[..txt.len().min(200)]);
                        }
                    }
                }
                Ok(Some(Ok(WsMessage::Ping(payload)))) => {
                    let _ = ws_sink.send(WsMessage::Pong(payload)).await;
                }
                Ok(Some(Ok(WsMessage::Close(frame)))) => {
                    disconnect_reason = format!("server close: {:?}",
                        frame.map(|f| format!("{} {}", f.code, f.reason)));
                    break;
                }
                Ok(Some(Err(e))) => {
                    disconnect_reason = format!("read error: {}", e);
                    break;
                }
                Ok(None) => {
                    disconnect_reason = "stream ended (None)".to_string();
                    break;
                }
                Err(_) => {
                    // Timeout — normal, loop back to check subscribe + read
                    continue;
                }
                _ => {}
            }
        }

        // Disconnected — reconnect with backoff
        let delay = WS_GATE_BACKOFF_MS.get(backoff_idx).copied().unwrap_or(15_000);
        warn!("[WS-GATE] Disconnected: {} — reconnecting in {}ms (cache: {} entries)",
            disconnect_reason, delay, all_subscribed.len());
        backoff_idx = (backoff_idx + 1).min(WS_GATE_BACKOFF_MS.len() - 1);
        // On reconnect, we need to re-subscribe to all tracked conditions
        resub_ids = all_subscribed.iter().cloned().collect();
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

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
/// Phase 1: CS2 match_winner from round scores (maps 1-0 / 1-1 + round lead)
const FF_CS2_MATCH_FROM_ROUNDS: bool = true;
/// Phase 1: Football anomaly DISABLED — production data: 40% WR, PnL -$4.54 (n=10)
const FF_FOOTBALL_ANOMALY_GOALDIFF2: bool = false;
/// Phase 2: Tennis game-level model (paper trade only until 50+ bets)
const FF_TENNIS_GAME_MODEL: bool = false;
/// Phase 4: Basketball live bets (OFF until live_score kalibrace)
const FF_BASKETBALL_LIVE: bool = false;
/// Regime-based stake sizing (Kelly/3 for StrongEdge, $0.50 for FalseFavorite)
const FF_REGIME_STAKE: bool = true;
/// Dota-2 score-edge dry-run (alert-only, no auto-bet)
const FF_DOTA2_EDGE_DRY_RUN: bool = true;
/// Dota-2 score-edge live rollout
const FF_DOTA2_EDGE_LIVE: bool = false;
/// Valorant score-edge dry-run (alert-only, no auto-bet)
const FF_VALORANT_EDGE_DRY_RUN: bool = false;
/// Valorant score-edge live rollout
const FF_VALORANT_EDGE_LIVE: bool = false;
/// StrongEdge Kelly/3 stake floor
const STRONG_EDGE_STAKE_MIN: f64 = 1.50;
/// StrongEdge Kelly/3 stake cap
const STRONG_EDGE_STAKE_MAX: f64 = 5.00;
/// FalseFavorite test stake
const FALSE_FAVORITE_STAKE: f64 = 0.50;

fn sport_score_edge_live_enabled(sport: &str) -> bool {
    match sport {
        "dota-2" => FF_DOTA2_EDGE_LIVE,
        "valorant" => FF_VALORANT_EDGE_LIVE,
        _ => true,
    }
}

fn sport_score_edge_dry_run_enabled(sport: &str) -> bool {
    match sport {
        "dota-2" => FF_DOTA2_EDGE_DRY_RUN,
        "valorant" => FF_VALORANT_EDGE_DRY_RUN,
        _ => false,
    }
}

/// Sport-specific auto-bet configuration (v3 — relaxed thresholds for score-edge)
/// Returns: (auto_bet_allowed, min_edge_pct, stake_multiplier, preferred_market)
/// preferred_market: "map_winner" | "match_winner"
fn get_sport_config(sport: &str) -> (bool, f64, f64, &'static str) {
    match sport {
        // Esports: prefer map_winner, but allow match_winner fallback when map market is missing.
        // Raised 28→38%: production data shows edge<40% bucket WR=42% vs need=51% → -EV. Only edge 40-50% was profitable.
        "cs2" | "valorant" | "dota-2" | "league-of-legends" | "lol"
            => (true, 38.0, 1.0, "match_or_map"),
        // Generic esports: same 38% threshold, blocked regardless by BLOCK_GENERIC_ESPORTS_BETS
        "esports"
            => (true, 38.0, 1.0, "match_or_map"),
        // Tennis: match_winner — our tennis_model uses set+game state
        // Raised 30→38%: production data (131W/125L) shows edge<40% is -EV across all sports
        "tennis"
            => (true, 38.0, 1.0, "match_winner"),
        // Basketball: match_winner — point spread model; +$4.49 P&L so keep but raise threshold
        "basketball"
            => (true, 38.0, 1.0, "match_winner"),
        // Football: DISABLED — production P&L -$18.59, WR 37%, 38 bets. No strategy worked.
        // Re-enable when we have a football-specific model with >52% WR at avg odds.
        "football"
            => (false, 38.0, 1.0, "match_winner"),
        // New sports: alerts enabled, conservative edge thresholds
        "volleyball" | "ice-hockey" | "baseball" | "cricket" | "boxing"
            => (true, 30.0, 1.0, "match_winner"),
        // Unknown sport: alerts only
        _
            => (false, 0.0, 0.0, "none"),
    }
}

/// Dynamic football edge threshold based on match minute.
/// Late game: odds adjust slowly → lower threshold is safe.
/// Returns adjusted min_edge_pct for football specifically.
fn dynamic_football_min_edge(detailed_score: Option<&str>) -> f64 {
    let ds = detailed_score.unwrap_or("");
    if let Some(minute) = parse_football_minute_static(ds) {
        match minute {
            0..=45 => 28.0,    // First half football remains too noisy for aggressive live edge betting
            46..=60 => 26.0,   // Early second half: require materially larger model-vs-market gap
            61..=75 => 24.0,   // Only strong late-game edges are allowed through
            76..=85 => 22.0,   // Very late game can relax slightly, but not below observed safe band
            _ => 22.0,         // 86+: keep late-game selective instead of auto-trusting the clock
        }
    } else {
        28.0 // No minute info: treat football as high uncertainty and keep it alert-first
    }
}

/// Static version of parse_football_minute for use in dynamic_football_min_edge
fn parse_football_minute_static(detailed: &str) -> Option<i64> {
    // Pattern 1: "NN.min"
    if let Some(min_idx) = detailed.find(".min") {
        let before = &detailed[..min_idx];
        let digits: String = before.chars().rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars().rev().collect();
        if let Ok(min) = digits.parse::<i64>() {
            return Some(min);
        }
    }
    // Pattern 2: "<Nmin"
    if let Some(lt_idx) = detailed.find('<') {
        let after = &detailed[lt_idx + 1..];
        let digits: String = after.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(min) = digits.parse::<i64>() {
            return Some(min);
        }
    }
    // Pattern 3: "N.pol" half markers
    for (half, minute_est) in [("1.pol", 25i64), ("2.pol", 65i64)] {
        if let Some(idx) = detailed.find(half) {
            if idx == 0 { return Some(minute_est); }
            let prev_char = detailed.as_bytes()[idx - 1];
            if prev_char != b':' && !prev_char.is_ascii_digit() {
                return Some(minute_est);
            }
        }
    }
    None
}

fn min_odds_factor_for_match(match_key: &str) -> f64 {
    let sport = match_key.split("::").next().unwrap_or("");
    match sport {
        "tennis" => MIN_ODDS_FACTOR_TENNIS,
        "basketball" => MIN_ODDS_FACTOR_BASKETBALL,
        _ => MIN_ODDS_FACTOR_DEFAULT,
    }
}

fn min_odds_factor_with_fallback(match_key: &str, fallback_applied: bool) -> f64 {
    let base = min_odds_factor_for_match(match_key);
    if fallback_applied {
        (base - MIN_ODDS_FALLBACK_STEP).max(0.80)
    } else {
        base
    }
}

/// Compute on-chain minOdds (in 1e12 format) with a floor of 1.01.
/// Without this floor, low Azuro odds (e.g. 1.18 * 0.83 = 0.98) produce sub-1.0
/// minOdds which provides ZERO slippage protection since odds can never be < 1.0.
fn compute_min_odds_raw(azuro_odds: f64, factor: f64) -> (u64, f64) {
    let display = (azuro_odds * factor).max(1.01);
    let raw = (display * 1e12) as u64;
    (raw, display)
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

/// Prematch odds anomaly auto-bet: RE-ENABLED with SCORE-CONFIRMED gate
/// Only bets when live score supports the anomaly direction (leading team = value side)
const AUTO_BET_ODDS_ANOMALY_ENABLED: bool = true;
const AUTO_BET_ODDS_ANOMALY_STAKE_BASE_USD: f64 = 0.50;
const AUTO_BET_ODDS_ANOMALY_REF_ODDS: f64 = 1.25;
const AUTO_BET_ODDS_ANOMALY_STAKE_FLOOR: f64 = 0.50;
const AUTO_BET_ODDS_ANOMALY_STAKE_CAP: f64 = 1.00;
/// Anomaly ODDS WINDOW: production data (256 settled bets) shows:
/// anomaly 1.45-1.70 = 70% WR (need 61%) → +EV sweet spot
/// anomaly <1.45 = 63% WR (need 69%) → -EV (short prices eat margin)
/// anomaly >1.70 = historically -EV, maintained as hard cap
const ANOMALY_MIN_ODDS: f64 = 1.45;
const ANOMALY_MAX_ODDS: f64 = 1.70;
/// Anomaly gets only 30% of daily budget (score-edge gets the rest)
const ANOMALY_DAILY_LIMIT_MULT: f64 = 0.30;
/// Minimum discrepancy for anomaly AUTO-BET (higher than alert threshold MIN_EDGE_PCT=8%)
/// Raised 15→22→28: tennis anomaly at 22-28% showed negative EV in production;
/// 28%+ threshold filters to only highest-confidence anomaly signals
const ANOMALY_MIN_DISC_AUTOBET: f64 = 28.0;
/// Tennis anomaly can be slightly looser than the global threshold because it still
/// requires score confirmation, a set lead, and low odds. This mainly unblocks
/// borderline 24-27% night candidates that are currently alert-only.
const TENNIS_ANOMALY_MIN_DISC_AUTOBET: f64 = 24.0;
/// Exception to favorit-only anomaly policy: allow tennis underdog only when the
/// discrepancy is extreme, odds stay in the safe anomaly band, and later score gates confirm it.
const TENNIS_UNDERDOG_ANOMALY_MIN_DISC: f64 = 40.0;
/// Tennis underdog override must beat the favorite anomaly by a meaningful margin,
/// otherwise we keep the original favorite-only bias.
const TENNIS_UNDERDOG_OVERRIDE_MARGIN_PCT: f64 = 10.0;

/// Odds-proportional anomaly stake: safe low odds → higher stake, risky high odds → lower
/// Formula: base × (ref_odds / azuro_odds)^1.5
fn anomaly_stake_for_odds(azuro_odds: f64) -> f64 {
    let scale = (AUTO_BET_ODDS_ANOMALY_REF_ODDS / azuro_odds).powf(1.5);
    (AUTO_BET_ODDS_ANOMALY_STAKE_BASE_USD * scale)
        .max(AUTO_BET_ODDS_ANOMALY_STAKE_FLOOR)
        .min(AUTO_BET_ODDS_ANOMALY_STAKE_CAP)
}

fn anomaly_min_disc_autobet(sport: &str) -> f64 {
    match sport {
        "tennis" => TENNIS_ANOMALY_MIN_DISC_AUTOBET,
        _ => ANOMALY_MIN_DISC_AUTOBET,
    }
}

fn allow_underdog_anomaly_override(match_key: &str, azuro_odds: f64, discrepancy_pct: f64) -> bool {
    let sport = match_key.split("::").next().unwrap_or("");
    sport == "tennis"
        && azuro_odds <= ANOMALY_MAX_ODDS
        && discrepancy_pct >= TENNIS_UNDERDOG_ANOMALY_MIN_DISC
}

fn prefer_underdog_anomaly_override(
    match_key: &str,
    underdog_odds: f64,
    underdog_disc_pct: f64,
    favorite_disc_pct: f64,
) -> bool {
    allow_underdog_anomaly_override(match_key, underdog_odds, underdog_disc_pct)
        && underdog_disc_pct >= favorite_disc_pct + TENNIS_UNDERDOG_OVERRIDE_MARGIN_PCT
}

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
        (0.05, 0.10, 0.15, 0.60, 0.60)  // micro: 5% bet, 10% cond, 15% match, 60% daily, 60% inflight
    } else if bankroll < 500.0 {
        (0.03, 0.08, 0.12, 0.20, 0.42)  // small
    } else if bankroll < 1500.0 {
        (0.02, 0.06, 0.10, 0.15, 0.37)  // medium
    } else {
        (0.015, 0.05, 0.08, 0.10, 0.32) // large
    }
}

fn score_edge_max_odds(market_key: &str, sport: &str, cs2_map_confidence: Option<&'static str>) -> f64 {
    let is_map_winner = market_key.starts_with("map") && market_key.ends_with("_winner");
    match sport {
        // Football score-edge produced multiple guarded, high-edge candidates in the 2.21-2.34 band.
        // Keep the cap conservative, but wide enough to not miss the safest late-score windows.
        "football" => 2.35,
        "tennis" => 2.10,
        "basketball" => 2.00,
        "cs2" => {
            if let Some(tier) = cs2_map_confidence {
                // Let HIGH (3.00) and ULTRA (5.00) pass through — score-edge with 80%+ win prob
                // is high certainty at these odds. MEDIUM (2.00) and LOW (1.60) are self-limiting.
                cs2_dynamic_max_odds(tier)
            } else if is_map_winner {
                AUTO_BET_MAX_ODDS_CS2_MAP
            } else {
                2.25
            }
        }
        "esports" | "valorant" | "dota-2" | "league-of-legends" | "lol" => {
            if let Some(tier) = cs2_map_confidence {
                cs2_dynamic_max_odds(tier)
            } else if is_map_winner {
                AUTO_BET_MAX_ODDS_CS2_MAP
            } else {
                2.15
            }
        }
        _ => {
            if is_map_winner {
                AUTO_BET_MAX_ODDS_CS2_MAP.min(2.25)
            } else {
                2.20
            }
        }
    }
}

fn score_edge_min_odds(sport: &str, market_key: &str) -> f64 {
    let is_map_winner = market_key.starts_with("map") && market_key.ends_with("_winner");
    match sport {
        // Football was missing guarded late-game entries purely because the global floor of 1.70 was too high.
        // 1.55 still avoids ultra-short prices while allowing strong 1.58-1.69 score edges through.
        "football" => 1.55,
        // Production observations: esports score-edge is healthier in the 1.50-1.70 band.
        // Keep match_winner a touch stricter than map_winner, because map edges are backed by round-level state.
        "cs2" | "esports" | "valorant" | "dota-2" | "league-of-legends" | "lol" => {
            if is_map_winner { 1.50 } else { 1.55 }
        }
        _ => AUTO_BET_MIN_ODDS,
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
    stake_path: &str,         // "score_edge" | "anomaly" (path-aware daily budget)
    azuro_odds: f64,          // REAL EDGE GUARD: odds check pro exponenciální sizing
    limit_override: f64,      // runtime daily limit — DAILY_LOSS_LIMIT_USD or /limit override
) -> f64 {
    // Effective daily limit: min(hard_limit, tier-based cap)
    // Uses SOD bankroll so the limit doesn't shrink as you lose bets during the day
    // If limit_override > DAILY_LOSS_LIMIT_USD the user explicitly raised it via /limit — skip tier cap
    let effective_daily_limit = if !FF_EXPOSURE_CAPS || limit_override > DAILY_LOSS_LIMIT_USD {
        limit_override
    } else {
        let (_, _, _, daily_loss_frac, _) = get_exposure_caps(sod_bankroll);
        limit_override.min(sod_bankroll * daily_loss_frac)
    };

    let path_daily_limit = if stake_path == "anomaly" {
        effective_daily_limit * ANOMALY_DAILY_LIMIT_MULT
    } else {
        effective_daily_limit
    };

    if !FF_EXPOSURE_CAPS {
        let base = calculated_stake * cross_val_multiplier;
        return base.min((path_daily_limit - daily_net_loss).max(0.0));
    }

    let (per_bet_frac, per_cond_frac, per_match_frac, _, inflight_frac) = get_exposure_caps(bankroll);
    let per_bet_cap = bankroll * per_bet_frac;
    let per_cond_cap = bankroll * per_cond_frac;
    let per_match_cap = bankroll * per_match_frac;

    let cond_room = (per_cond_cap - condition_exposure).max(0.0);
    let match_room = (per_match_cap - match_exposure).max(0.0);
    let daily_room = (path_daily_limit - daily_net_loss).max(0.0);

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

    // Apply REAL EDGE multiplier k base stake pokud jsou kurzy v sweet-spotu (1.80+) a máme cross match.
    // Base stake je násoben exponenciálně s tím, jak je kurz zajímavější, až do 1.75x
    let mut real_edge_multiplier = cross_val_multiplier;
    if cross_val_multiplier > 1.0 && azuro_odds >= 1.80 {
        // Exponenciální škálování od 1.80 do 2.50
        // max bonus u 2.50 je +75% ke staku (1.25 -> 2.18x) nebo jen fix +50%:
        real_edge_multiplier *= 1.5;
    }
    
    let boosted_stake = calculated_stake * real_edge_multiplier;

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
            "📊 TRIM_STAKE: raw={:.2} boosted={:.2} final={:.2} | caps: bet={:.2} cond={:.2} match={:.2} daily={:.2} inflight={:.2} sport={:.2} | sod_br={:.2} cur_br={:.2} eff_daily_lim={:.2} path_daily_lim={:.2} path={}",
            calculated_stake, boosted_stake, final_stake,
            per_bet_cap, cond_room, match_room, daily_room, inflight_room, sport_room,
            sod_bankroll, bankroll, effective_daily_limit, path_daily_limit, stake_path
        );
    }

    // IMPORTANT: When a potentially valid bet (>= $0.50) gets trimmed to $0,
    // log a full cap breakdown at INFO so we can root-cause “SCORE placed=0”.
    if calculated_stake >= 0.50 && final_stake < 0.50 {
        tracing::info!(
            "🛡️ TRIM_TO_ZERO: raw={:.2} boosted={:.2} final={:.2} | rooms: bet={:.2} cond={:.2} match={:.2} daily={:.2} inflight={:.2} sport={:.2} | inflight_total={:.2} sport_exp={:.2} cond_exp={:.2} match_exp={:.2} daily_loss={:.2} | sod_br={:.2} cur_br={:.2} eff_daily_lim={:.2} path_daily_lim={:.2} sport={} path={}",
            calculated_stake,
            boosted_stake,
            final_stake,
            per_bet_cap,
            cond_room,
            match_room,
            daily_room,
            inflight_room,
            sport_room,
            inflight_total,
            sport_exposure,
            condition_exposure,
            match_exposure,
            daily_net_loss,
            sod_bankroll,
            bankroll,
            effective_daily_limit,
            path_daily_limit,
            sport,
            stake_path
        );
    }

    if final_stake < 0.50 { 0.0 } else { final_stake }
}

/// Cross-validation result for HLTV vs Chance score comparison.
/// Returns (skip: bool, stake_multiplier: f64)
///   skip=false always — NEVER hard-skip, use reduced stake instead
///   stake_multiplier: 1.25 (agree), 0.5 (mismatch=hedged), 1.0 (single source)
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
                // Mismatch → HEDGED bet at 0.5x stake (not hard skip)
                // Scraper fluke is common; full skip leaves money on table
                info!("CROSS-VAL mismatch: HLTV={:?} vs Chance={:?} → 0.5x stake (was: HARD SKIP)", h, c);
                (false, 0.5)
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

#[derive(Debug, Clone)]
struct BackwardScoreState {
    score1: i32,
    score2: i32,
    seen_count: u8,
    first_seen_at: chrono::DateTime<Utc>,
}

impl BackwardScoreState {
    fn new(score1: i32, score2: i32) -> Self {
        Self {
            score1,
            score2,
            seen_count: 1,
            first_seen_at: Utc::now(),
        }
    }

    fn observe(&mut self, score1: i32, score2: i32) -> bool {
        if self.score1 == score1 && self.score2 == score2 {
            self.seen_count = self.seen_count.saturating_add(1);
        } else {
            self.score1 = score1;
            self.score2 = score2;
            self.seen_count = 1;
            self.first_seen_at = Utc::now();
        }

        self.seen_count >= 2
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
    #[serde(default)]
    sport: Option<String>,
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
    if token.trim().is_empty() || chat_id == 0 {
        return Ok(0);
    }
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });
    let resp = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Telegram sendMessage request failed: {}", e);
            return Ok(0);
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("Telegram sendMessage failed: {} — {}", status, body);
        // Non-fatal: keep bot running even if Telegram is misconfigured.
        return Ok(0);
    }
    let resp_json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("Telegram sendMessage JSON parse failed: {}", e);
            return Ok(0);
        }
    };
    let msg_id = resp_json["result"]["message_id"].as_i64().unwrap_or(0);
    Ok(msg_id)
}

async fn tg_get_updates(client: &reqwest::Client, token: &str, offset: i64) -> Result<TgUpdatesResponse> {
    if token.trim().is_empty() {
        return Ok(TgUpdatesResponse { ok: false, result: vec![] });
    }
    let url = format!(
        "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout=5&allowed_updates=[\"message\",\"message_reaction\"]",
        token, offset
    );
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("getUpdates request failed: {}", e);
            return Ok(TgUpdatesResponse { ok: false, result: vec![] });
        }
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Non-fatal: invalid token (401) or forbidden (403) should not crash the bot.
        // Auto-bet logic can still run; Telegram features will be effectively disabled.
        warn!("getUpdates HTTP {}: {}", status, body);
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Ok(TgUpdatesResponse { ok: false, result: vec![] });
        }
        return Ok(TgUpdatesResponse { ok: false, result: vec![] });
    }
    let parsed: TgUpdatesResponse = serde_json::from_str(&body)
        .with_context(|| format!("Failed to parse getUpdates: {}", &body[..body.len().min(200)]))?;
    Ok(parsed)
}

async fn tg_get_me(client: &reqwest::Client, token: &str) -> Result<i64> {
    if token.trim().is_empty() {
        return Ok(0);
    }
    let url = format!("https://api.telegram.org/bot{}/getMe", token);
    let resp: serde_json::Value = match client.get(&url).send().await {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Telegram getMe JSON parse failed: {}", e);
                return Ok(0);
            }
        },
        Err(e) => {
            warn!("Telegram getMe request failed: {}", e);
            return Ok(0);
        }
    };
    if resp["ok"].as_bool() == Some(false) {
        warn!("Telegram getMe failed: {}", resp.to_string());
        return Ok(0);
    }
    Ok(resp["result"]["id"].as_i64().unwrap_or(0))
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
    /// match_key → transient lower score waiting for second confirmation
    backward_scores: HashMap<String, BackwardScoreState>,
}

impl ScoreTracker {
    fn new() -> Self {
        Self {
            prev_scores: HashMap::new(),
            edge_cooldown: HashMap::new(),
            backward_scores: HashMap::new(),
        }
    }

    /// Clean entries older than 30 min (match ended)
    fn cleanup(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::seconds(1800);
        self.prev_scores.retain(|_, (_, _, ts)| *ts > cutoff);
        self.edge_cooldown.retain(|_, ts| *ts > cutoff);
        self.backward_scores.retain(|_, state| state.first_seen_at > cutoff);
    }
}

/// Score edge alert — Azuro odds haven't adjusted to live score
struct ScoreEdge {
    match_key: String,
    /// Exact Azuro market used for execution (match_winner, map1_winner, map2_winner, map3_winner...)
    market_key: String,
    /// Sport prefix used for Azuro odds lookup (may differ from match_key prefix for generic esports:: keys)
    resolved_sport: Option<String>,
    /// Classified concrete esports family when generic esports:: key is disambiguated.
    esports_family: Option<&'static str>,
    /// Classifier confidence for esports family routing: high / medium / low / unknown / n-a.
    esports_confidence: &'static str,
    /// Short machine-readable reason for classifier decision.
    esports_reason: &'static str,
    team1: String,
    team2: String,
    /// Current live score
    score1: i32,
    score2: i32,
    /// Raw live status string (e.g. "76'", "2. pol", "Half-time")
    live_status: String,
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
    /// Detailed score string from live feed (for esports anomaly guard)
    detailed_score: Option<String>,
}

// ====================================================================
// CS2 ROUND SCORE PARSER — extract current round from Chance detailed_score
// ====================================================================

/// Parse CS2 round score from Dust2.us detailed_score string.
/// Example: "R:9-3 M:0-0" → Some((9, 3)) for round score
/// Example: "R:6-8 M:1-0" → Some((6, 8)) for round score
fn parse_dust2_round_score(detailed: &str) -> Option<(i32, i32)> {
    // Look for "R:X-Y" pattern
    if let Some(r_pos) = detailed.find("R:") {
        let after = &detailed[r_pos + 2..];
        let parts: Vec<&str> = after.split_whitespace().next()?.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(a), Ok(b)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>()) {
                if a >= 0 && a <= 30 && b >= 0 && b <= 30 {
                    return Some((a, b));
                }
            }
        }
    }
    None
}

/// Parse CS2 map score from Dust2.us detailed_score string.
/// Example: "R:9-3 M:1-0" → Some((1, 0)) for map score
fn parse_dust2_map_score(detailed: &str) -> Option<(i32, i32)> {
    if let Some(m_pos) = detailed.find("M:") {
        let after = &detailed[m_pos + 2..];
        let parts: Vec<&str> = after.split_whitespace().next().unwrap_or(after).split('-').collect();
        if parts.len() == 2 {
            if let (Ok(a), Ok(b)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>()) {
                if a >= 0 && a <= 5 && b >= 0 && b <= 5 {
                    return Some((a, b));
                }
            }
        }
    }
    None
}

/// Universal CS2/esports round score extractor — tries all formats.
/// Returns Some((round1, round2)) from any source format.
fn parse_esports_round_score(detailed: &str) -> Option<(i32, i32)> {
    // Try Dust2 format first (most precise)
    if let Some(rs) = parse_dust2_round_score(detailed) {
        return Some(rs);
    }
    // Then Chance/Tipsport format
    parse_cs2_round_score(detailed)
}

/// Universal CS2/esports map score extractor — tries all formats.
/// Returns Some((map1, map2)) from any source format.
fn parse_esports_map_score(detailed: &str, score1: i32, score2: i32) -> (i32, i32) {
    // Try Dust2 "M:X-Y" format first
    if let Some(ms) = parse_dust2_map_score(detailed) { return ms; }

    // Chance/Tipsport format carries completed map scores inside detailed_score.
    // Example: "Lepší ze 3 | 3.mapa - 13:6, 9:13, 7:12" → completed maps = 1-1.
    let completed_maps = parse_cs2_completed_maps(detailed);
    if !completed_maps.is_empty() || parse_cs2_current_map(detailed).is_some() {
        let mut team1_maps = 0;
        let mut team2_maps = 0;
        for (m1, m2) in completed_maps {
            if m1 > m2 {
                team1_maps += 1;
            } else if m2 > m1 {
                team2_maps += 1;
            }
        }
        return (team1_maps, team2_maps);
    }

    // Fallback: use live score1/score2 only when the feed itself is already map-level.
    (score1, score2)
}

fn normalize_cs2_live_score_for_edge(
    score1: i32,
    score2: i32,
    detailed: Option<&str>,
    esports_family: Option<&str>,
) -> (i32, i32) {
    if esports_family != Some("cs2") {
        return (score1, score2);
    }

    let Some(detail) = detailed else {
        return (score1, score2);
    };

    let Some((round1, round2)) = parse_esports_round_score(detail) else {
        return (score1, score2);
    };

    if score1 == round1 && score2 == round2 {
        return (score1, score2);
    }

    let raw_looks_map_level = score1.max(score2) <= 1;
    let round_has_progress = round1 > 0 || round2 > 0;

    // Chance/Tipsport often reports fresh CS2 rounds as raw 0-0 / 0-1 while
    // detailed_score already carries the real round score (e.g. R:2-2 M:0-0).
    // Trust the detailed round context in these low-score openings so we don't
    // manufacture fake backward jumps on every new map start.
    if raw_looks_map_level && round_has_progress {
        return (round1, round2);
    }

    // Prefer round score whenever payload score is map-level or otherwise inconsistent
    // with the richer round context from detailed_score.
    if round1.max(round2) > 3 && (score1.max(score2) <= 3 || (score1 != round1 || score2 != round2)) {
        return (round1, round2);
    }

    (score1, score2)
}

fn has_cs2_round_context_override(
    raw_score1: i32,
    raw_score2: i32,
    detailed: Option<&str>,
    esports_family: Option<&str>,
) -> bool {
    if esports_family != Some("cs2") {
        return false;
    }

    raw_score1.max(raw_score2) <= 3
        && detailed
            .and_then(parse_esports_round_score)
            .map(|(r1, r2)| {
                (r1.max(r2) > 3) || (raw_score1.max(raw_score2) <= 1 && (r1 > 0 || r2 > 0))
            })
            .unwrap_or(false)
}

fn is_status_pregame_like(status: &str) -> bool {
    let lower = status.to_lowercase();
    lower.contains("za ")
        || lower.contains("za okam")
        || lower.contains("minut")
        || lower.contains("minute")
        || lower.contains("start")
}

/// Returns true when the match is CS2 or strongly resembles CS2 by context.
/// Used to apply CS2 score hold guards to `esports::` matches that Chance/Tipsport
/// sends without resolving `sport = cs2`. Czech "mapa" appears ONLY in CS2 BO3 feeds.
fn is_cs2_like(esports_family: Option<&str>, detailed: Option<&str>) -> bool {
    esports_family == Some("cs2") || detailed.unwrap_or("").to_lowercase().contains("mapa")
}

fn is_cs2_reset_hold_state(
    prev_score1: i32,
    prev_score2: i32,
    score1: i32,
    score2: i32,
    detailed: Option<&str>,
    live_status: &str,
    esports_family: Option<&str>,
) -> bool {
    if !is_cs2_like(esports_family, detailed) {
        return false;
    }

    if score1 != 0 || score2 != 0 {
        return false;
    }

    if prev_score1.max(prev_score2) > 0 {
        return true;
    }

    let detail = detailed.unwrap_or("").trim();
    let pregame_or_blank = detail.is_empty()
        || detail == "R:0-0 M:0-0"
        || is_status_pregame_like(live_status);

    if pregame_or_blank {
        return prev_score1.max(prev_score2) > 0;
    }

    prev_score1.max(prev_score2) > 3
}

fn is_cs2_map_rollover_hold_state(
    prev_score1: i32,
    prev_score2: i32,
    score1: i32,
    score2: i32,
    detailed: Option<&str>,
    esports_family: Option<&str>,
) -> bool {
    if !is_cs2_like(esports_family, detailed) {
        return false;
    }

    let detail = detailed.unwrap_or("");
    let has_cs2_detail = detail.contains("mapa") || detail.contains("R:") || detail.contains("M:") || detail.contains('(');

    // Case 1: high round-score map → 1-0 map score (asymmetric winner)
    let case_asymmetric = prev_score1.max(prev_score2) >= 8
        && score1.max(score2) <= 1
        && score1 != score2
        && (has_cs2_detail || detail.trim().is_empty());

    // Case 2: early round-score in map 3 → 1-1 map score (both teams won 1 map)
    // feed sends e.g. 1-4 rounds → then 1-1 map score when context updates
    let case_map3_start = score1 == 1 && score2 == 1
        && prev_score1.max(prev_score2) > 1
        && (prev_score1 != 1 || prev_score2 != 1)
        && (score1 < prev_score1 || score2 < prev_score2)
        && has_cs2_detail;

    case_asymmetric || case_map3_start
}

fn is_cs2_round_rewind_hold_state(
    prev_score1: i32,
    prev_score2: i32,
    score1: i32,
    score2: i32,
    detailed: Option<&str>,
    esports_family: Option<&str>,
) -> bool {
    if !is_cs2_like(esports_family, detailed) {
        return false;
    }

    if prev_score1.max(prev_score2) <= 3 || score1.max(score2) <= 3 {
        return false;
    }

    let drop1 = prev_score1 - score1;
    let drop2 = prev_score2 - score2;
    if drop1 <= 0 && drop2 <= 0 {
        return false;
    }

    let detail = detailed.unwrap_or("");
    let has_cs2_detail = detail.contains("mapa") || detail.contains("R:") || detail.contains("M:");
    let strong_rewind = drop1.max(drop2) >= 6 || (drop1 >= 3 && drop2 >= 3);

    has_cs2_detail && strong_rewind
}

fn is_cs2_legit_map_rollover(
    prev_score1: i32,
    prev_score2: i32,
    score1: i32,
    score2: i32,
    raw_score1: i32,
    raw_score2: i32,
    detailed: Option<&str>,
    esports_family: Option<&str>,
) -> bool {
    if !is_cs2_like(esports_family, detailed) {
        return false;
    }

    if prev_score1.max(prev_score2) <= 3 {
        return false;
    }

    let detail = detailed.unwrap_or("");
    let has_map_rollover_detail = detail.contains("mapa")
        || detail.contains("M:")
        || (detail.contains('(') && detail.contains(':'));
    let explicit_next_map = parse_cs2_current_map(detail)
        .map(|map_no| map_no > 1)
        .unwrap_or(false)
        && !parse_cs2_completed_maps(detail).is_empty();
    let dust2_map_progress = parse_dust2_map_score(detail)
        .map(|(m1, m2)| m1 > 0 || m2 > 0)
        .unwrap_or(false);
    let current_round_like = score1.max(score2) <= 12 || (score1.max(score2) <= 15 && prev_score1.max(prev_score2) >= 13);
    let raw_round_like = raw_score1.max(raw_score2) <= 2
        || (raw_score1.max(raw_score2) <= 12 && score1.max(score2) <= 12)
        || (raw_score1.max(raw_score2) >= 10 && score1.max(score2) <= 12);
    let score_dropped = score1 < prev_score1 || score2 < prev_score2;
    let previous_map_finished = prev_score1.max(prev_score2) >= 10;
    let plausible_rollover_score = score_dropped
        && current_round_like
        && raw_round_like
        && previous_map_finished;

    plausible_rollover_score && (has_map_rollover_detail || explicit_next_map || dust2_map_progress)
}

fn is_cs2_backward_score_pending_state(
    prev_score1: i32,
    prev_score2: i32,
    score1: i32,
    score2: i32,
    detailed: Option<&str>,
    esports_family: Option<&str>,
) -> bool {
    if !is_cs2_like(esports_family, detailed) {
        return false;
    }

    if score1 <= 0 && score2 <= 0 {
        return false;
    }

    if prev_score1 <= 0 && prev_score2 <= 0 {
        return false;
    }

    let detail = detailed.unwrap_or("");
    let has_cs2_detail = detail.contains("mapa") || detail.contains("R:") || detail.contains("M:");
    if !has_cs2_detail {
        return false;
    }

    let drop1 = (prev_score1 - score1).max(0);
    let drop2 = (prev_score2 - score2).max(0);
    let total_drop = drop1 + drop2;

    total_drop > 0 && total_drop <= 2
}

/// Esports anomaly guard: is the match "in-progress" enough to trust odds anomaly?
/// Returns true if match has progressed past early-game noise zone.
/// Logic: map_diff ≥ 1 (someone won a map) OR round_total ≥ 5 (current map in progress).
/// This blocks: fresh match starts (0-0, round 0-0) where Azuro oracle hasn't adjusted.
fn esports_anomaly_guard(score1: i32, score2: i32, detailed: Option<&str>) -> bool {
    // Map score check: if someone is leading in maps, odds are meaningful
    let map_diff = (score1 - score2).abs();
    if map_diff >= 1 { return true; }

    // Round score check: if enough rounds played, market has had time to react
    if let Some(det) = detailed {
        if let Some((r1, r2)) = parse_esports_round_score(det) {
            let total_rounds = r1 + r2;
            return total_rounds >= 5; // ~2.5 min into map minimum
        }
    }

    // No detailed score and maps are 0-0 → match just started → skip
    false
}

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

fn cs2_round_edge_max_odds_override(
    sport: &str,
    market_key: &str,
    cs2_map_confidence: Option<&'static str>,
    edge_pct: f64,
    score1: i32,
    score2: i32,
) -> Option<f64> {
    if sport != "cs2" {
        return None;
    }
    let is_map_winner = market_key.starts_with("map") && market_key.ends_with("_winner");
    let is_match_winner = market_key == "match_winner";
    if !is_map_winner && !is_match_winner {
        return None;
    }
    if score1.max(score2) <= 3 {
        return None;
    }

    if is_match_winner {
        let round_diff = (score1 - score2).abs();
        // Stale GQL odds problem: when one team leads 0-4+ in a map, GQL still
        // shows inflated odds (3.5+) because queries lag behind scoreboard.
        // With 40%+ edge AND big score diff, we KNOW the odds are wrong — allow wider band.
        return if edge_pct >= 40.0 && round_diff >= 4 {
            Some(5.00)
        } else if edge_pct >= 55.0 {
            Some(4.00)
        } else if edge_pct >= 45.0 {
            Some(3.50)
        } else if edge_pct >= 32.0 {
            Some(2.90)
        } else {
            None
        };
    }

    match cs2_map_confidence {
        Some("ULTRA") if edge_pct >= 40.0 => Some(3.60),
        Some("HIGH") if edge_pct >= 40.0 => Some(3.60),
        Some("HIGH") if edge_pct >= 32.0 => Some(3.45),
        _ => None,
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

fn scoped_condition_key(base_match_key: &str, condition_id: &str) -> String {
    format!("{}|{}", base_match_key, condition_id)
}

fn normalize_team_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
}

fn canonical_team_name(name: &str) -> String {
    let mut normalized = normalize_team_name(name);

    if normalized.starts_with("team") && normalized.len() > 8 {
        normalized = normalized.trim_start_matches("team").to_string();
    }

    let aliases = [
        ("bbteam", "betboom"),
        ("betboomteam", "betboom"),
        ("submarineyellow", "yellowsubmarine"),
        ("yellowsubmarine", "yellowsubmarine"),
        ("teamquazar", "quazar"),
        ("quazarmpkbkcislan", "quazar"),
        ("rustecmpkbkcislan", "rustec"),
        ("teamnemesis", "nemesis"),
        ("europeannemesis", "nemesis"),
        ("fengdacsasia", "fengda"),
        ("lynnvisioncsasia", "lynnvision"),
        ("depocsasia", "depo"),
    ];
    for (from, to) in aliases {
        normalized = normalized.replace(from, to);
    }

    for suffix in ["mpkbkcislan", "cislan", "csasia", "academy", "esports"] {
        if normalized.ends_with(suffix) && normalized.len() > suffix.len() + 3 {
            normalized.truncate(normalized.len() - suffix.len());
        }
    }

    normalized
}

fn match_key_team_parts(match_key: &str) -> Option<(String, String)> {
    let tail = match_key.split("::").nth(1)?;
    let (left, right) = tail.split_once("_vs_")?;
    Some((canonical_team_name(left), canonical_team_name(right)))
}

fn team_matches_match_key_part(team_name: &str, key_part: &str) -> bool {
    let team = canonical_team_name(team_name);
    !team.is_empty() && !key_part.is_empty() && (team == key_part || team.contains(key_part) || key_part.contains(&team))
}

fn live_item_matches_match_key(item: &LiveItem) -> bool {
    let Some((key_team1, key_team2)) = match_key_team_parts(&item.match_key) else {
        return true;
    };

    let direct = team_matches_match_key_part(&item.payload.team1, &key_team1)
        && team_matches_match_key_part(&item.payload.team2, &key_team2);
    let swapped = team_matches_match_key_part(&item.payload.team1, &key_team2)
        && team_matches_match_key_part(&item.payload.team2, &key_team1);

    direct || swapped
}

fn live_item_priority(item: &LiveItem) -> i32 {
    let mut score = match item.source.as_str() {
        "dust2" => 60,
        s if s.starts_with("hltv") => 55,
        "chance" => 40,
        "fortuna" => 30,
        "tipsport" => 20,
        _ => 10,
    };

    match item.payload.sport.as_deref() {
        Some("cs2") => score += 20,
        Some("dota-2") | Some("valorant") | Some("lol") | Some("league-of-legends") => score += 10,
        Some("esports") => score += 5,
        _ => {}
    }

    if item.payload.detailed_score.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
        score += 10;
    }
    if live_item_matches_match_key(item) {
        score += 35;
    } else {
        score -= 45;
    }

    score
}

/// Check if a single team name loosely matches another (substring or equality after normalization)
fn team_name_matches_single(live_name: &str, azuro_name: &str) -> bool {
    let a = canonical_team_name(live_name);
    let b = canonical_team_name(azuro_name);
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
    let a1n = canonical_team_name(a1);
    let a2n = canonical_team_name(a2);
    let b1n = canonical_team_name(b1);
    let b2n = canonical_team_name(b2);

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

#[derive(Debug, Clone, Copy)]
struct EsportsClassification {
    family: Option<&'static str>,
    confidence: &'static str,
    reason: &'static str,
}

const ESPORTS_CS2_MARKERS: &[&str] = &[
    "3dmax", "themongolz", "mongolz", "ursa", "enceacademy", "hotu", "eyeballers",
    "9ine", "oddik", "mibr", "nrg", "sharks", "redcanids", "fluxo", "imperialesports",
];
const ESPORTS_DOTA2_MARKERS: &[&str] = &[
    "yellowsubmarine", "runeeaters", "gaiminggladiators", "xtremegaming", "parivision",
    "tundraesports", "teamfalcons",
];
const ESPORTS_VALORANT_MARKERS: &[&str] = &[
    "acend", "zerotenacity", "gentlemates", "karminecorpvalorant",
];
const ESPORTS_LOL_MARKERS: &[&str] = &[
    "topesports", "teamwe", "fearx", "geng", "nongshimredforce", "ktrolster",
];

fn canonicalize_esports_family(sport: &str) -> Option<&'static str> {
    match sport {
        "cs2" => Some("cs2"),
        "dota-2" => Some("dota-2"),
        "valorant" => Some("valorant"),
        "league-of-legends" | "lol" => Some("league-of-legends"),
        _ => None,
    }
}

fn detect_marker_family(blob: &str) -> Option<&'static str> {
    let families = [
        ("cs2", ESPORTS_CS2_MARKERS),
        ("dota-2", ESPORTS_DOTA2_MARKERS),
        ("valorant", ESPORTS_VALORANT_MARKERS),
        ("league-of-legends", ESPORTS_LOL_MARKERS),
    ];
    let mut matched_family: Option<&'static str> = None;
    for (family, markers) in families {
        if markers.iter().any(|marker| blob.contains(marker)) {
            if matched_family.is_some() {
                return None;
            }
            matched_family = Some(family);
        }
    }
    matched_family
}

fn classify_esports_family(
    match_key: &str,
    live_sport: Option<&str>,
    resolved_sport: Option<&str>,
    team1: &str,
    team2: &str,
) -> EsportsClassification {
    let sport_raw = match_key.split("::").next().unwrap_or("");
    if let Some(family) = canonicalize_esports_family(sport_raw) {
        return EsportsClassification {
            family: Some(family),
            confidence: "high",
            reason: "match_key_prefix",
        };
    }
    if let Some(family) = live_sport.and_then(canonicalize_esports_family) {
        return EsportsClassification {
            family: Some(family),
            confidence: "high",
            reason: "live_payload_sport",
        };
    }
    if let Some(family) = resolved_sport.and_then(canonicalize_esports_family) {
        return EsportsClassification {
            family: Some(family),
            confidence: "high",
            reason: "resolved_sport",
        };
    }
    if sport_raw != "esports" {
        return EsportsClassification {
            family: None,
            confidence: "n-a",
            reason: "non_esports",
        };
    }

    let blob = format!(
        "{} {} {}",
        normalize_team_name(match_key),
        normalize_team_name(team1),
        normalize_team_name(team2)
    );
    if let Some(family) = detect_marker_family(&blob) {
        return EsportsClassification {
            family: Some(family),
            confidence: "medium",
            reason: "team_marker",
        };
    }

    EsportsClassification {
        family: None,
        confidence: "unknown",
        reason: "unclassified",
    }
}

fn generic_esports_auto_bet_allowed(
    match_key: &str,
    resolved_sport: Option<&str>,
    esports_family: Option<&str>,
    esports_confidence: &str,
    esports_reason: &str,
) -> bool {
    if !BLOCK_GENERIC_ESPORTS_BETS || !match_key.starts_with("esports::") {
        return true;
    }

    let family = match esports_family {
        Some(family) => family,
        None => return false,
    };

    if esports_confidence != "high" {
        return false;
    }

    match esports_reason {
        "live_payload_sport" => true,
        "resolved_sport" => resolved_sport
            .and_then(canonicalize_esports_family)
            .is_some_and(|resolved| resolved == family),
        _ => false,
    }
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

fn has_recent_azuro_market_for_live(
    match_key: &str,
    live: &LiveItem,
    now: DateTime<Utc>,
    azuro_by_match: &HashMap<&str, &StateOddsItem>,
    map_winners_by_match: &HashMap<&str, Vec<MapWinnerOdds>>,
) -> bool {
    let esports_alts_list: &[&str] = &["cs2", "dota-2", "league-of-legends", "valorant", "basketball", "football", "mma"];
    let exact_lookup_keys: Vec<String> = if match_key.starts_with("esports::") {
        let tail = &match_key["esports::".len()..];
        let mut keys = Vec::with_capacity(esports_alts_list.len() + 1);
        keys.push(match_key.to_string());
        for alt in esports_alts_list {
            keys.push(format!("{}::{}", alt, tail));
        }
        keys
    } else {
        vec![match_key.to_string()]
    };

    for key in &exact_lookup_keys {
        if let Some(item) = azuro_by_match.get(key.as_str()) {
            if is_recent_seen_at(&item.seen_at, now) {
                return true;
            }
        }
        if let Some(list) = map_winners_by_match.get(key.as_str()) {
            if list.iter().any(|mw| is_recent_seen_at(&mw.seen_at, now)) {
                return true;
            }
        }
    }

    azuro_by_match.values().any(|item| {
        is_recent_seen_at(&item.seen_at, now)
            && teams_match_loose(
                &live.payload.team1,
                &live.payload.team2,
                &item.payload.team1,
                &item.payload.team2,
            )
    }) || map_winners_by_match.values().any(|list| {
        list.iter().any(|mw| {
            is_recent_seen_at(&mw.seen_at, now)
                && teams_match_loose(
                    &live.payload.team1,
                    &live.payload.team2,
                    &mw.team1,
                    &mw.team2,
                )
        })
    })
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

/// CS2 round score → MATCH win probability (Bo3).
/// Combines: P(win current map) × Bo3 transition probabilities.
///
/// Bo3 transitions (team that wins next map → match result):
///   maps 0-0: win → 1-0 (P_match=0.58) / lose → 0-1 (P_match=0.42)
///   maps 1-0: win → 2-0 (P_match=1.00) / lose → 1-1 (P_match=0.50)
///   maps 0-1: win → 1-1 (P_match=0.50) / lose → 0-2 (P_match=0.00)
///   maps 1-1: win → 2-1 (P_match=1.00) / lose → 1-2 (P_match=0.00)
///
/// Source: HLTV Bo3 first-map-winner stats → 58-62%, using 0.58 conservative.
fn cs2_round_to_match_prob(
    map_lead: i32,    // maps won by the round-leading team
    map_lose: i32,    // maps won by the opponent
    round_lead: i32,  // rounds won by leading team (current map)
    round_lose: i32,  // rounds won by opponent (current map)
) -> Option<f64> {
    let diff = round_lead - round_lose;
    let total = round_lead + round_lose;
    if diff <= 0 { return None; }

    let map_prob = cs2_map_win_prob(diff, total);

    // Bo3 transition: P(match) given current map outcome
    let (p_after_win, p_after_lose) = match (map_lead, map_lose) {
        (0, 0) => (0.58, 0.42),  // → 1-0 or 0-1
        (1, 0) => (1.00, 0.50),  // → 2-0 WIN or 1-1
        (0, 1) => (0.50, 0.00),  // → 1-1 or 0-2 LOSS
        (1, 1) => (1.00, 0.00),  // → 2-1 WIN or 1-2 LOSS
        _      => return None,   // match already decided
    };

    let match_prob = map_prob * p_after_win + (1.0 - map_prob) * p_after_lose;

    // Minimum useful threshold: 55% (below = NoBet / prefer map_winner)
    if match_prob < 0.55 { return None; }
    Some(match_prob)
}

/// Regime classification based on true_p.
/// Returns: ("StrongEdge" | "FalseFavorite" | "NoBet", true_p)
fn classify_regime(true_p: f64, azuro_odds: f64) -> &'static str {
    // Quarantine: odds ≥ 2.0 with low true_p is steam-roller territory
    if azuro_odds >= 2.0 && true_p < 0.75 {
        return "NoBet";
    }
    if true_p >= 0.70 {
        "StrongEdge"
    } else if true_p >= 0.55 {
        "FalseFavorite"
    } else {
        "NoBet"
    }
}

/// Kelly/3 stake sizing for regime-based betting.
/// f = (true_p × odds - 1) / (odds - 1)   (Kelly criterion)
/// stake = bankroll × f / 3                (fractional Kelly for safety)
/// Guardrails: FLOOR $1.50, CAP $5.00 for StrongEdge; $0.50 for FalseFavorite.
fn compute_regime_stake(true_p: f64, azuro_odds: f64, bankroll: f64) -> f64 {
    let regime = classify_regime(true_p, azuro_odds);
    match regime {
        "StrongEdge" => {
            let kelly_f = (true_p * azuro_odds - 1.0) / (azuro_odds - 1.0);
            if kelly_f <= 0.0 { return FALSE_FAVORITE_STAKE; } // edge evaporated
            let stake = bankroll * kelly_f / 3.0;
            stake.max(STRONG_EDGE_STAKE_MIN).min(STRONG_EDGE_STAKE_MAX)
        }
        "FalseFavorite" => FALSE_FAVORITE_STAKE,
        _ => 0.0, // NoBet
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
        // Round-level leads predict MAP wins, NOT MATCH wins.
        // Round edges should ONLY generate map_winner bets (STEP 1).
        // Returning None ensures match_winner fallback never triggers.
        return None;
    } else {
        // MAP scores (Bo3/Bo5 format)
        // (1, 0) RE-ENABLED at conservative 58%: map pick advantage is real
        // but small. With sanitized data + min_edge 12%, this only triggers
        // when Azuro odds are genuinely stale (implied < 46%).
        match (leading_score, losing_score) {
            (1, 0) => Some(0.58),  // Won 1 map → ~58% (conservative)
            (2, 0) => None,        // Already won → too late
            (2, 1) => None,        // Already won
            _ => None,
        }
    }
}

/// LoL / Valorant map score → win probability (Bo3/Bo5).
/// Separate from CS2 because the map-pick dynamics differ slightly.
///   LoL Bo3: teams ban/pick champions per game, map pick less relevant
///   Valorant Bo3: map veto similar to CS2
/// Conservative: (1,0) → 58% (same as CS2 for now)
fn map_score_to_win_prob(leading: i32, losing: i32) -> Option<f64> {
    if leading <= losing { return None; }
    match (leading, losing) {
        (1, 0) => Some(0.58),  // Won 1 map/game in Bo3
        (2, 0) => None,        // Already won
        (2, 1) => None,        // Already won
        (3, 0) | (3, 1) | (3, 2) => None, // Bo5 won
        _ => None,
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

fn football_minute_from_context(status: Option<&str>, detailed_score: Option<&str>) -> Option<i32> {
    detailed_score
        .and_then(|detail| parse_football_minute_static(detail).map(|minute| minute as i32))
        .or_else(|| status.and_then(parse_football_minute))
}

fn football_auto_bet_guard(goal_diff: i32, minute: Option<i32>) -> bool {
    let minute = match minute {
        Some(minute) => minute,
        None => return false,
    };

    match goal_diff {
        diff if diff >= 3 => minute >= 55,
        2 => minute >= 70,
        1 => false,
        _ => false,
    }
}

fn blocked_score_edge_reason_codes(
    auto_bet_enabled: bool,
    sport_auto_allowed: bool,
    sport_live_enabled: bool,
    is_preferred_market: bool,
    sport_guard_ok: bool,
    within_daily_limit: bool,
    safe_mode: bool,
    confidence_high: bool,
    edge_pct: f64,
    sport_min_edge: f64,
    azuro_odds: f64,
    effective_min_odds: f64,
    effective_max_odds: f64,
    has_condition_id: bool,
    has_outcome_id: bool,
    generic_esports_blocked: bool,
    condition_blacklisted: bool,
    match_blacklisted: bool,
    already_bet_this: bool,
    rebet_ok: bool,
    stake: f64,
    bankroll_ok: bool,
    pending_ok: bool,
    streak_ok: bool,
) -> Vec<&'static str> {
    let mut reasons = Vec::new();

    if !auto_bet_enabled {
        reasons.push("AutoBetDisabled");
    }
    if !sport_auto_allowed {
        reasons.push("SportAutoDisabled");
    }
    if !sport_live_enabled {
        reasons.push("SportLiveDisabled");
    }
    if !is_preferred_market {
        reasons.push("MarketNotPreferred");
    }
    if !sport_guard_ok {
        reasons.push("SportGuardBlocked");
    }
    if !within_daily_limit {
        reasons.push("DailyLossLimit");
    }
    if safe_mode {
        reasons.push("SafeMode");
    }
    if !confidence_high {
        reasons.push("ConfidenceNotHigh");
    }
    if edge_pct < sport_min_edge {
        reasons.push("EdgeBelowMin");
    }
    if azuro_odds < effective_min_odds {
        reasons.push("OddsBelowMin");
    }
    if azuro_odds > effective_max_odds {
        reasons.push("OddsAboveMax");
    }
    if !has_condition_id {
        reasons.push("MissingConditionId");
    }
    if !has_outcome_id {
        reasons.push("MissingOutcomeId");
    }
    if generic_esports_blocked {
        reasons.push("GenericEsportsBlocked");
    }
    if condition_blacklisted {
        reasons.push("ConditionBlacklisted");
    }
    if match_blacklisted {
        reasons.push("MatchBlacklisted");
    }
    if already_bet_this && !rebet_ok {
        reasons.push("DedupBlocked");
    }
    if stake < 0.50 {
        reasons.push("StakeTrimmedBelowMin");
    }
    if !bankroll_ok {
        reasons.push("BankrollTooLow");
    }
    if !pending_ok {
        reasons.push("PendingCap");
    }
    if !streak_ok {
        reasons.push("LossStreakPause");
    }

    reasons
}

fn effective_score_edge_sport<'a>(
    match_key: &'a str,
    resolved_sport: Option<&'a str>,
    esports_family: Option<&'static str>,
) -> &'a str {
    let sport_raw = match_key.split("::").next().unwrap_or("?");
    if sport_raw != "esports" {
        return sport_raw;
    }
    if let Some(family) = esports_family {
        return family;
    }
    match resolved_sport {
        Some("cs2") | Some("dota-2") | Some("league-of-legends") | Some("valorant") => {
            resolved_sport.unwrap_or(sport_raw)
        }
        _ => sport_raw,
    }
}

fn should_audit_esports_score_decision(
    match_key: &str,
    esports_family: Option<&str>,
    esports_confidence: &str,
) -> bool {
    if match_key.starts_with("cs2::") {
        return true;
    }

    match_key.starts_with("esports::")
        && matches!(esports_confidence, "high" | "medium")
        && matches!(
            esports_family,
            Some("cs2") | Some("dota-2") | Some("valorant") | Some("league-of-legends")
        )
}

fn append_ledger_audit_event(event: &str, data: &serde_json::Value) {
    let mut entry = data.clone();
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("ts".to_string(), serde_json::json!(Utc::now().to_rfc3339()));
        obj.insert("event".to_string(), serde_json::json!(event));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("data/ledger.jsonl")
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", entry);
    }
}

/// Football goal score → estimated match win probability for the LEADING team.
///
/// Minute is critical. A 2-goal lead at 20' is not the same product as a 2-goal
/// lead at 78'. We therefore keep early-game fair probabilities much lower and
/// only let the clock push them up late.
fn football_score_to_win_prob(leading: i32, losing: i32, minute: Option<i32>) -> Option<f64> {
    if leading <= losing { return None; }
    let diff = leading - losing;
    let total = leading + losing;

    // Only bet when there's clear goal advantage
    if diff < 1 { return None; }

    let minute = minute.unwrap_or(55);

    match diff {
        1 => {
            let base: f64 = match minute {
                0..=30 => 0.56,
                31..=45 => 0.60,
                46..=60 => 0.64,
                61..=75 => 0.68,
                76..=85 => 0.73,
                _ => 0.79,
            };
            Some(if total >= 3 { (base + 0.02).min(0.82) } else { base })
        }
        2 => {
            let base: f64 = match minute {
                0..=30 => 0.70,
                31..=45 => 0.75,
                46..=60 => 0.81,
                61..=75 => 0.86,
                76..=85 => 0.91,
                _ => 0.95,
            };
            Some(if total >= 4 { (base + 0.02).min(0.96) } else { base })
        }
        _ => Some(match minute {
            0..=30 => 0.82,
            31..=45 => 0.87,
            46..=60 => 0.91,
            61..=75 => 0.95,
            76..=85 => 0.97,
            _ => 0.985,
        }),
    }
}

fn parse_football_minute(status: &str) -> Option<i32> {
    let lower = status.to_lowercase();
    if lower.contains("poločas") || lower.contains("half") {
        return Some(45);
    }

    let mut digits = String::new();
    let mut started = false;
    for ch in status.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            started = true;
        } else if started {
            break;
        }
    }

    if digits.is_empty() {
        None
    } else {
        digits.parse::<i32>().ok()
    }
}

fn score_edge_stake_multiplier(edge: &ScoreEdge, sport: &str, azuro_odds: f64) -> f64 {
    let edge_mult: f64 = if edge.edge_pct >= 25.0 {
        1.8
    } else if edge.edge_pct >= 20.0 {
        1.5
    } else if edge.edge_pct >= 16.0 {
        1.3
    } else {
        1.0
    };

    let phase_mult: f64 = match sport {
        "football" => {
            let goal_diff = (edge.score1 - edge.score2).abs();
            let minute = football_minute_from_context(Some(&edge.live_status), edge.detailed_score.as_deref());
            if goal_diff >= 3 {
                if minute.map_or(false, |m| m >= 70) { 1.30 } else { 1.10 }
            } else if goal_diff == 2 {
                if minute.map_or(false, |m| m >= 75) {
                    1.20
                } else if minute.map_or(false, |m| m >= 60) {
                    1.05
                } else {
                    0.90
                }
            } else if goal_diff >= 1 {
                if minute.map_or(false, |m| m >= 80) {
                    1.15
                } else {
                    0.95
                }
            } else {
                1.0
            }
        }
        "cs2" | "esports" | "lol" | "dota-2" | "valorant" | "league-of-legends" => {
            if let Some(ref detailed) = edge.detailed_score {
                if let Some((r1, r2)) = parse_esports_round_score(detailed) {
                    let total_rounds = r1 + r2;
                    if total_rounds >= 20 {
                        1.6
                    } else if total_rounds >= 16 {
                        1.45
                    } else if total_rounds >= 12 {
                        1.25
                    } else {
                        1.0
                    }
                } else {
                    1.0
                }
            } else {
                1.0
            }
        }
        _ => 1.0,
    };

    let mut mult: f64 = (edge_mult * phase_mult).min(SCORE_EDGE_STAKE_MAX_MULT);
    if azuro_odds >= 2.1 {
        mult = mult.min(1.2);
    } else if azuro_odds >= 1.8 {
        mult = mult.min(1.4);
    }
    mult
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
            1..=3  => None,            // Too close early
            4..=6  => Some(0.56),
            7..=9  => Some(0.60),
            10..=14 => Some(0.67),
            _ => Some(0.75),           // 15+ early
        };
    }

    // Mid game (30-80 total) — leads start to matter
    if total < 80 {
        return match diff {
            1..=2  => None,            // Small lead, high variance
            3..=5  => Some(0.57),
            6..=9  => Some(0.63),
            10..=14 => Some(0.72),
            15..=19 => Some(0.80),
            _ => Some(0.87),           // 20+ mid
        };
    }

    // Late game (80-140 total) — leads are more decisive
    if total < 140 {
        return match diff {
            1..=2  => None,
            3..=4  => Some(0.60),
            5..=7  => Some(0.66),
            8..=12 => Some(0.76),
            13..=17 => Some(0.85),
            _ => Some(0.91),           // 18+ late
        };
    }

    // Very late game (140+ total, ~4th quarter) — leads are decisive
    match diff {
        1..=2  => None,
        3..=4  => Some(0.63),
        5..=7  => Some(0.70),
        8..=12 => Some(0.82),
        13..=17 => Some(0.90),
        _ => Some(0.95),               // 18+ very late
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
    let mut live_map: HashMap<&str, &LiveItem> = HashMap::new();
    let mut live_map_priority: HashMap<&str, i32> = HashMap::new();
    for live in &state.live {
        let priority = live_item_priority(live);
        if priority < 0 {
            continue;
        }
        let key = live.match_key.as_str();
        let should_replace = live_map_priority
            .get(key)
            .map(|current| priority > *current)
            .unwrap_or(true);
        if should_replace {
            live_map.insert(key, live);
            live_map_priority.insert(key, priority);
        }
    }

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
        let live_esports_class = classify_esports_family(
            match_key,
            live.payload.sport.as_deref(),
            None,
            &live.payload.team1,
            &live.payload.team2,
        );
        let raw_s1 = live.payload.score1;
        let raw_s2 = live.payload.score2;
        let (s1, s2) = normalize_cs2_live_score_for_edge(
            raw_s1,
            raw_s2,
            live.payload.detailed_score.as_deref(),
            live_esports_class.family,
        );
        let cs2_round_context_override = has_cs2_round_context_override(
            raw_s1,
            raw_s2,
            live.payload.detailed_score.as_deref(),
            live_esports_class.family,
        );

        // Check if score changed from previous poll
        let prev = tracker.prev_scores.get(*match_key).cloned();

        let is_first_sight = prev.is_none();
        let (prev_s1, prev_s2, prev_seen_at) = match prev {
            Some((ps1, ps2, pts)) => (ps1, ps2, pts),
            None => (s1, s2, now), // First time: use current score as "previous" for edge calc
        };

        let cs2_reset_hold_state = is_cs2_reset_hold_state(
            prev_s1,
            prev_s2,
            s1,
            s2,
            live.payload.detailed_score.as_deref(),
            &live.payload.status,
            live_esports_class.family,
        );
        let cs2_round_rewind_hold_state = is_cs2_round_rewind_hold_state(
            prev_s1,
            prev_s2,
            s1,
            s2,
            live.payload.detailed_score.as_deref(),
            live_esports_class.family,
        );
        let cs2_map_rollover_hold_state = is_cs2_map_rollover_hold_state(
            prev_s1,
            prev_s2,
            s1,
            s2,
            live.payload.detailed_score.as_deref(),
            live_esports_class.family,
        );
        if cs2_reset_hold_state || cs2_round_rewind_hold_state || cs2_map_rollover_hold_state {
            tracker.backward_scores.remove(*match_key);
            let suppress_duplicate = tracker.edge_cooldown.get(*match_key)
                .map(|ts| (now - *ts).num_seconds() < 45)
                .unwrap_or(false);
            if !suppress_duplicate {
                let hold_reason = if cs2_round_rewind_hold_state {
                    "ScoreRoundRewind"
                } else if cs2_map_rollover_hold_state {
                    "ScoreMapRolloverPending"
                } else {
                    "ScoreContextReset"
                };
                info!(
                    "  ⏸️ {} CS2 hold {}-{} -> {}-{} ({})",
                    match_key, prev_s1, prev_s2, s1, s2, hold_reason
                );
                if should_audit_esports_score_decision(
                    match_key,
                    live_esports_class.family,
                    live_esports_class.confidence,
                ) {
                    append_ledger_audit_event("ESPORTS_SCORE_DECISION_AUDIT", &serde_json::json!({
                        "match_key": match_key,
                        "path": "score_model",
                        "decision": "hold_candidate",
                        "reason_code": hold_reason,
                        "reason_codes": [hold_reason],
                        "resolved_sport": live_esports_class.family.or(live.payload.sport.as_deref()),
                        "esports_family": live_esports_class.family,
                        "sport_confidence": live_esports_class.confidence,
                        "sport_reason": live_esports_class.reason,
                        "team1": live.payload.team1,
                        "team2": live.payload.team2,
                        "raw_score1": raw_s1,
                        "raw_score2": raw_s2,
                        "prev_score1": prev_s1,
                        "prev_score2": prev_s2,
                        "score1": s1,
                        "score2": s2,
                        "live_status": live.payload.status,
                        "detailed_score": live.payload.detailed_score,
                    }));
                }
            }
            tracker.edge_cooldown.insert(match_key.to_string(), now);
            continue;
        }

        let cs2_legit_map_rollover = is_cs2_legit_map_rollover(
            prev_s1,
            prev_s2,
            s1,
            s2,
            raw_s1,
            raw_s2,
            live.payload.detailed_score.as_deref(),
            live_esports_class.family,
        );

        let score_changed = s1 != prev_s1 || s2 != prev_s2;
        // Guard against score-mode switches / parser glitches:
        // examples: 19-17 -> 1-0, 1-2 -> 0-0. These are often round->map or source resets.
        let backward_score_jump = score_changed
            && s1 <= prev_s1
            && s2 <= prev_s2
            && (s1 < prev_s1 || s2 < prev_s2)
            && !cs2_round_context_override
            && !cs2_legit_map_rollover;

        if !backward_score_jump {
            tracker.backward_scores.remove(*match_key);
        }

        if backward_score_jump {
            if is_cs2_backward_score_pending_state(
                prev_s1,
                prev_s2,
                s1,
                s2,
                live.payload.detailed_score.as_deref(),
                live_esports_class.family,
            ) {
                let confirmed = tracker
                    .backward_scores
                    .entry(match_key.to_string())
                    .or_insert_with(|| BackwardScoreState::new(s1, s2))
                    .observe(s1, s2);

                let hold_reason = if confirmed {
                    tracker.prev_scores.insert(match_key.to_string(), (s1, s2, now));
                    tracker.backward_scores.remove(*match_key);
                    "ScoreBackwardConfirmed"
                } else {
                    "ScoreBackwardPending"
                };

                info!(
                    "  ⏸️ {} CS2 backward hold {}-{} -> {}-{} ({})",
                    match_key, prev_s1, prev_s2, s1, s2, hold_reason
                );
                if should_audit_esports_score_decision(
                    match_key,
                    live_esports_class.family,
                    live_esports_class.confidence,
                ) {
                    append_ledger_audit_event("ESPORTS_SCORE_DECISION_AUDIT", &serde_json::json!({
                        "match_key": match_key,
                        "path": "score_model",
                        "decision": "hold_candidate",
                        "reason_code": hold_reason,
                        "reason_codes": [hold_reason],
                        "resolved_sport": live_esports_class.family.or(live.payload.sport.as_deref()),
                        "esports_family": live_esports_class.family,
                        "sport_confidence": live_esports_class.confidence,
                        "sport_reason": live_esports_class.reason,
                        "team1": live.payload.team1,
                        "team2": live.payload.team2,
                        "raw_score1": raw_s1,
                        "raw_score2": raw_s2,
                        "prev_score1": prev_s1,
                        "prev_score2": prev_s2,
                        "score1": s1,
                        "score2": s2,
                        "live_status": live.payload.status,
                        "detailed_score": live.payload.detailed_score,
                    }));
                }
                tracker.edge_cooldown.insert(match_key.to_string(), now);
                continue;
            }

            info!(
                "  ⏭️ {} score jump backward {}-{} -> {}-{} (source/reset), skipping edge eval",
                match_key, prev_s1, prev_s2, s1, s2
            );
            if should_audit_esports_score_decision(
                match_key,
                live_esports_class.family,
                live_esports_class.confidence,
            ) {
                append_ledger_audit_event("ESPORTS_SCORE_DECISION_AUDIT", &serde_json::json!({
                    "match_key": match_key,
                    "path": "score_model",
                    "decision": "blocked_candidate",
                    "reason_code": "ScoreJumpBackward",
                    "reason_codes": ["ScoreJumpBackward"],
                    "resolved_sport": live_esports_class.family.or(live.payload.sport.as_deref()),
                    "esports_family": live_esports_class.family,
                    "sport_confidence": live_esports_class.confidence,
                    "sport_reason": live_esports_class.reason,
                    "team1": live.payload.team1,
                    "team2": live.payload.team2,
                    "raw_score1": raw_s1,
                    "raw_score2": raw_s2,
                    "prev_score1": prev_s1,
                    "prev_score2": prev_s2,
                    "score1": s1,
                    "score2": s2,
                    "live_status": live.payload.status,
                    "detailed_score": live.payload.detailed_score,
                }));
            }
            tracker.edge_cooldown.insert(match_key.to_string(), now);
            continue;
        }

        tracker.prev_scores.insert(match_key.to_string(), (s1, s2, now));

        // Guard against impossible forward spikes in basketball feed.
        // Example parser/source glitch: 7-7 -> 77-7 in one poll window.
        let elapsed_secs = (now - prev_seen_at).num_seconds().max(1);
        let delta1 = s1 - prev_s1;
        let delta2 = s2 - prev_s2;
        let is_basketball_match = match_key.starts_with("basketball::");
        let forward_basket_spike = score_changed
            && is_basketball_match
            && !is_first_sight
            && delta1 >= 0
            && delta2 >= 0
            && elapsed_secs <= 120
            && (
                delta1.max(delta2) >= 20
                    || (delta1 + delta2) >= 26
                    || ((s1.max(s2) - s1.min(s2)) >= 60 && delta1.max(delta2) >= 15)
            );
        if forward_basket_spike {
            info!(
                "  ⏭️ {} basketball forward spike {}-{} -> {}-{} (Δ={} / {}, {}s), skipping edge eval",
                match_key,
                prev_s1,
                prev_s2,
                s1,
                s2,
                delta1,
                delta2,
                elapsed_secs
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

        if !has_recent_azuro_market_for_live(match_key, live, now, &azuro_by_match, &map_winners_by_match) {
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

        // Football: RE-ENABLED with detailed_score minute guard.
        // Previous bug (Campbelltown loss) was caused by corrupted detailed_score prefix
        // leak — minute parser returned wrong half → wrong fair probability.
        // Now fixed: sanitize_detailed_score() + parse_football_minute() word-boundary fix.
        // Guard: REQUIRE parseable minute from detailed_score (= reliable data).
        let is_lol = match_key.starts_with("league-of-legends::");
        let is_valorant = match_key.starts_with("valorant::");
        if is_football {
            let ds = live.payload.detailed_score.as_deref().unwrap_or("");
            let has_minute = ds.contains(".min") || ds.contains("min.") || ds.contains("poločas") || ds.contains("pol.");
            if !has_minute {
                info!("  ⏭️ {} {}-{}: football score-edge SKIPPED (no minute in detailed_score: '{}')", match_key, s1, s2, ds);
                continue;
            }
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
            let minute = football_minute_from_context(Some(&live.payload.status), live.payload.detailed_score.as_deref());
            match football_score_to_win_prob(leading_maps, losing_maps, minute) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: football score not actionable",
                        match_key, s1, s2);
                    continue;
                }
            }
        } else if is_dota2 {
            // Dota-2 feed can be either kill score or map score.
            // Current Tipsport live feed exposes map score like 0-1 plus "2. mapa" in detailed_score.
            let ds_lower = live.payload.detailed_score.as_deref().unwrap_or("").to_lowercase();
            let looks_like_map_score = leading_maps.max(losing_maps) <= 3 && ds_lower.contains("mapa");
            let dota_prob = if looks_like_map_score {
                map_score_to_win_prob(leading_maps, losing_maps)
            } else {
                dota2_score_to_win_prob(leading_maps, losing_maps)
            };

            match dota_prob {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: dota-2 score not actionable (diff={}, total={}, detailed='{}')",
                        match_key, s1, s2, leading_maps - losing_maps, s1 + s2,
                        live.payload.detailed_score.as_deref().unwrap_or(""));
                    continue;
                }
            }
        } else if is_lol || is_valorant {
            // LoL / Valorant: Bo3/Bo5 MAP scores (same model as CS2 maps)
            // LoL: map (game) scores 0-2 in Bo3, 0-3 in Bo5
            // Valorant: map scores 0-2 in Bo3
            // (1,0) = won 1 map → ~58% (map pick advantage, less than CS2)
            match map_score_to_win_prob(leading_maps, losing_maps) {
                Some(p) => p,
                None => {
                    info!("  ⏭️ {} {}-{}: {} map score not actionable",
                        match_key, s1, s2, if is_lol { "LoL" } else { "Valorant" });
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
                if !matches!(live_esports_class.confidence, "high" | "medium") {
                    info!("  ⚠️  {} is generic esports:: key (not concretely classified) — team names may not be CS2. Score {}-{}",
                        match_key, s1, s2);
                }
            }
            // Phase 1: CS2 match_winner from round scores
            // When live_score is round-level (max > 3), try to compute
            // match win probability using round + map context.
            if FF_CS2_MATCH_FROM_ROUNDS && leading_maps.max(losing_maps) > 3 {
                // Parse map context from detailed_score
                let ds = live.payload.detailed_score.as_deref().unwrap_or("");
                let (ml, mm) = parse_esports_map_score(ds, s1, s2);
                // ml/mm are map scores; s1/s2 are round scores
                // For Dust2 format "R:9-4 M:1-0": ml=1, mm=0
                // For Chance format: map score from live_score parent or parse
                let (map_leader, map_loser) = if ml > mm { (ml, mm) } else if mm > ml { (mm, ml) } else {
                    // If map scores are equal/unknown, try parse from detailed_score Dust2 M:X-Y
                    if let Some(ms) = parse_dust2_map_score(ds) {
                        if ms.0 > ms.1 { (ms.0, ms.1) } else { (ms.1, ms.0) }
                    } else {
                        (0, 0) // maps 0-0
                    }
                };
                match cs2_round_to_match_prob(map_leader, map_loser, leading_maps, losing_maps) {
                    Some(p) => {
                        let regime = classify_regime(p, 1.50); // placeholder odds for logging
                        info!("  🎯 {} CS2 MATCH FROM ROUNDS: maps {}-{}, rounds {}-{} → match_prob={:.1}% regime={}",
                            match_key, map_leader, map_loser, leading_maps, losing_maps, p * 100.0, regime);
                        p
                    }
                    None => {
                        info!("  ⏭️ {} {}-{}: CS2 round score → match_prob below threshold (maps={}-{})",
                            match_key, s1, s2, map_leader, map_loser);
                        if should_audit_esports_score_decision(
                            match_key,
                            live_esports_class.family,
                            live_esports_class.confidence,
                        ) {
                            append_ledger_audit_event("ESPORTS_SCORE_DECISION_AUDIT", &serde_json::json!({
                                "match_key": match_key,
                                "path": "score_model",
                                "decision": "blocked_candidate",
                                "reason_code": "RoundModelBelowThreshold",
                                "reason_codes": ["RoundModelBelowThreshold"],
                                "resolved_sport": live_esports_class.family.or(live.payload.sport.as_deref()),
                                "esports_family": live_esports_class.family,
                                "sport_confidence": live_esports_class.confidence,
                                "sport_reason": live_esports_class.reason,
                                "team1": live.payload.team1,
                                "team2": live.payload.team2,
                                "raw_score1": raw_s1,
                                "raw_score2": raw_s2,
                                "map_score1": map_leader,
                                "map_score2": map_loser,
                                "round_score1": leading_maps,
                                "round_score2": losing_maps,
                                "score1": s1,
                                "score2": s2,
                                "live_status": live.payload.status,
                                "detailed_score": live.payload.detailed_score,
                            }));
                        }
                        continue;
                    }
                }
            } else {
                match score_to_win_prob(leading_maps, losing_maps) {
                    Some(p) => p,
                    None => {
                        info!("  ⏭️ {} {}-{}: score not actionable (diff={}, max={})",
                            match_key, s1, s2, leading_maps - losing_maps,
                            leading_maps.max(losing_maps));
                        continue;
                    }
                }
            }
        };

        // ================================================================
        // CROSS-VALIDATION: Compare HLTV score vs Chance detailed_score
        // Mismatch → 0.5x stake (hedged, NOT hard skip)
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

        // RESYNC OBSERVABILITY: log mismatches but NO hard skip/freeze
        // cv_skip is always false now — mismatch just reduces stake to 0.5x
        if FF_RESYNC_FREEZE && is_cs2_like && cv_stake_mult < 1.0 {
            // Record mismatch for tracking (no blocking)
            let rs = resync_freeze.entry(match_key.to_string()).or_insert_with(ResyncState::new);
            rs.record_mismatch();
            info!("  ⚠️ {} CROSS-VAL MISMATCH (hedged 0.5x): HLTV={}-{} vs Chance={:?} detailed='{}'",
                match_key, s1, s2, chance_round, detailed);
        } else if FF_RESYNC_FREEZE && is_cs2_like && cv_stake_mult > 1.0 {
            // Agreement — clear any previous mismatch state
            if resync_freeze.contains_key(&match_key.to_string()) {
                resync_freeze.remove(&match_key.to_string());
                info!("  ✅ {} RESYNC CLEARED after agreement", match_key);
            }
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
        let resolved_sport_for_odds: &str = odds_lookup_key.split("::").next().unwrap_or("");
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
                            let _leading_team = if leading_side == 1 { &live.payload.team1 } else { &live.payload.team2 };
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

                    let mw_confidence = if mw_edge >= 12.0 { "HIGH" } else { "MEDIUM" };
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

                    let esports_meta = classify_esports_family(
                        match_key,
                        live.payload.sport.as_deref(),
                        Some(resolved_sport_for_odds),
                        &live.payload.team1,
                        &live.payload.team2,
                    );

                    edges.push(ScoreEdge {
                        match_key: match_key.to_string(),
                        market_key: mw.market.clone(),
                        resolved_sport: Some(resolved_sport_for_odds.to_string()),
                        esports_family: esports_meta.family,
                        esports_confidence: esports_meta.confidence,
                        esports_reason: esports_meta.reason,
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score1: s1,
                        score2: s2,
                        live_status: live.payload.status.clone(),
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
                        detailed_score: live.payload.detailed_score.clone(),
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

        // Confidence based on edge size (12% = aligned with sport_min_edge)
        let confidence = if edge >= 12.0 { "HIGH" } else { "MEDIUM" };

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

        let esports_meta = classify_esports_family(
            match_key,
            live.payload.sport.as_deref(),
            Some(resolved_sport_for_odds),
            &live.payload.team1,
            &live.payload.team2,
        );

        edges.push(ScoreEdge {
            match_key: match_key.to_string(),
            market_key: azuro.payload.market.clone().unwrap_or_else(|| "match_winner".to_string()),
            resolved_sport: Some(resolved_sport_for_odds.to_string()),
            esports_family: esports_meta.family,
            esports_confidence: esports_meta.confidence,
            esports_reason: esports_meta.reason,
            team1: live.payload.team1.clone(),
            team2: live.payload.team2.clone(),
            score1: s1,
            score2: s2,
            live_status: live.payload.status.clone(),
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
            detailed_score: live.payload.detailed_score.clone(),
        });
    }

    // Cleanup old entries
    tracker.cleanup();

    edges
}

fn format_score_edge_alert(e: &ScoreEdge, alert_id: u32) -> String {
    let leading_team = if e.leading_side == 1 { &e.team1 } else { &e.team2 };
    let azuro_odds = if e.leading_side == 1 { e.azuro_w1 } else { e.azuro_w2 };
    let market_label = e.market_key.replace('_', " ");

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
        "⚡ <b>#{}</b> {} <b>SCORE EDGE</b>\n\
         🏷️ <b>{}</b> | market: <b>{}</b> | path: <b>score_edge</b> | conf: <b>{}</b>\n\
         🧩 <b>{}</b> vs <b>{}</b>\n\
         🔴 LIVE: <b>{}-{}</b> (předtím {}-{})\n\
         💡 Pick: <b>{}</b> @ <b>{:.2}</b>\n\
         📊 Azuro: {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         🧠 Why: edge <b>{:.1}%</b> | score-implied <b>{:.1}%</b> vs azuro <b>{:.1}%</b>\n\
         🛰 Sources (2): azuro + live_score\n\
         🏦 {}{}\n\
         Reply: <code>{} YES $3</code> / <code>{} OPP $3</code> / <code>{} NO</code>",
        alert_id,
        conf_emoji,
        sport,
        market_label,
        e.confidence,
        e.team1,
        e.team2,
        e.score1,
        e.score2,
        e.prev_score1,
        e.prev_score2,
        leading_team,
        azuro_odds,
        e.team1,
        e.azuro_w1,
        e.team2,
        e.azuro_w2,
        e.edge_pct,
        e.score_implied_pct,
        e.azuro_implied_pct,
        exec_ready,
        url_line,
        alert_id,
        alert_id,
        alert_id,
    )
}

// ====================================================================
// Odds comparison logic
// ====================================================================

#[derive(Clone)]
struct OddsAnomaly {
    detected_at: DateTime<Utc>,
    match_key: String,
    /// Exact Azuro market used for execution (match_winner, map1_winner, map2_winner, map3_winner...)
    market_key: String,
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
    /// Detailed score from bookmaker scraper (round-level for CS2)
    detailed_score: Option<String>,
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
    market_key: String,
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
    /// Strategy path that originated this bet: "score_edge", "anomaly_odds", "bet_command"
    path: String,
}

fn count_pending_slots(active_bets: &[ActiveBet]) -> usize {
    active_bets.iter().filter(|bet| bet.token_id.is_none()).count()
}

fn rewrite_pending_claims_file(active_bets: &[ActiveBet], pending_claims_path: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(pending_claims_path) {
        use std::io::Write;
        for bet in active_bets {
            let tid = bet.token_id.as_deref().unwrap_or("?");
            let _ = writeln!(f, "{}|{}|{}|{}|{}|{}",
                tid, bet.bet_id, bet.match_key,
                bet.value_team, bet.amount_usd, bet.odds);
        }
    }
}

fn should_count_loss_streak(bet: &ActiveBet) -> bool {
    bet.alert_id > 0 && bet.path != "loaded" && bet.placed_at != "loaded"
}

fn record_live_loss_for_streak(
    bet: &ActiveBet,
    consecutive_losses: &mut usize,
    loss_streak_pause_until: &mut Option<std::time::Instant>,
) {
    if !should_count_loss_streak(bet) {
        info!(
            "ℹ️ LOSS STREAK IGNORE: historical/recovered loss {} path={} placed_at={}",
            bet.bet_id, bet.path, bet.placed_at
        );
        return;
    }

    *consecutive_losses += 1;
    if *consecutive_losses >= LOSS_STREAK_PAUSE_THRESHOLD {
        *loss_streak_pause_until = Some(
            std::time::Instant::now() + std::time::Duration::from_secs(LOSS_STREAK_PAUSE_SECS)
        );
        info!(
            "🛑 LOSS STREAK: {} consecutive live losses — pausing auto-bet for {}s",
            *consecutive_losses,
            LOSS_STREAK_PAUSE_SECS
        );
    }
}

fn reconcile_active_bets_with_executor_snapshot(
    active_bets: &mut Vec<ActiveBet>,
    bets_arr: &[serde_json::Value],
    pending_claims_path: &str,
    session_start: DateTime<Utc>,
    inflight_ttl_secs: i64,
) -> f64 {
    let onchain_pending_tids: HashSet<String> = bets_arr.iter()
        .filter(|b| b.get("status").and_then(|v| v.as_str()).unwrap_or("") == "pending")
        .filter_map(|b| b.get("tokenId").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    let onchain_enriched: Vec<(String, String, f64, f64, String, String)> = bets_arr.iter()
        .filter(|b| b.get("status").and_then(|v| v.as_str()).unwrap_or("") == "pending")
        .map(|b| {
            let tid = b.get("tokenId").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let team = b.get("team").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let odds = b.get("odds").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let amount = b.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let match_key = b.get("matchKey").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let bet_id = b.get("betId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            (tid, team, odds, amount, match_key, bet_id)
        })
        .collect();

    let mut needs_rewrite = false;
    let pre_count = active_bets.len();

    active_bets.retain(|b| {
        match &b.token_id {
            Some(tid) => onchain_pending_tids.contains(tid),
            None => true,
        }
    });
    if active_bets.len() != pre_count {
        needs_rewrite = true;
    }

    let before_ttl = active_bets.len();
    let now_utc = Utc::now();
    active_bets.retain(|b| {
        if b.token_id.is_some() { return true; }
        if b.placed_at == "onchain" { return true; }
        if b.placed_at == "loaded" {
            let session_age = (now_utc - session_start).num_seconds();
            if session_age > inflight_ttl_secs {
                info!("⏰ ZOMBIE_TTL: loaded bet {} ({}) expired — session age {}s > TTL {}s",
                    b.bet_id, b.value_team, session_age, inflight_ttl_secs);
                return false;
            }
            return true;
        }
        match chrono::DateTime::parse_from_rfc3339(&b.placed_at) {
            Ok(placed) => {
                let age_secs = (now_utc - placed.with_timezone(&Utc)).num_seconds();
                if age_secs > inflight_ttl_secs {
                    info!("⏰ INFLIGHT_TTL: bet {} ({}) expired after {}s — removing",
                        b.bet_id, b.value_team, age_secs);
                    false
                } else {
                    true
                }
            }
            Err(_) => true,
        }
    });
    if active_bets.len() != before_ttl {
        needs_rewrite = true;
    }

    let local_tids: HashSet<String> = active_bets.iter()
        .filter_map(|b| b.token_id.clone())
        .collect();
    for (tid, team, odds, amount, match_key, bet_id) in &onchain_enriched {
        if !local_tids.contains(tid) && !team.is_empty() {
            active_bets.push(ActiveBet {
                alert_id: 0,
                bet_id: if !bet_id.is_empty() { bet_id.clone() } else { format!("onchain_{}", tid) },
                match_key: match_key.clone(),
                market_key: "unknown".to_string(),
                team1: team.clone(),
                team2: "?".to_string(),
                value_team: team.clone(),
                amount_usd: *amount,
                odds: *odds,
                placed_at: "onchain".to_string(),
                condition_id: String::new(),
                outcome_id: String::new(),
                graph_bet_id: None,
                token_id: Some(tid.clone()),
                path: "onchain".to_string(),
            });
            needs_rewrite = true;
        }
    }

    for b in active_bets.iter_mut() {
        if b.token_id.is_none() {
            if let Some((tid, _, _, _, _, _)) = onchain_enriched.iter()
                .find(|(_, _, _, _, _, bid)| !bid.is_empty() && bid == &b.bet_id)
            {
                b.token_id = Some(tid.clone());
                info!("🔗 RECONCILE: Discovered tokenId {} for bet {} via betId match", tid, b.bet_id);
                needs_rewrite = true;
            } else if let Some((tid, _, _, _, _, _)) = onchain_enriched.iter()
                .find(|(_, t, o, _, mk, _)| {
                    !t.is_empty() && t == &b.value_team
                    && (*o - b.odds).abs() < 0.03
                    && (mk.is_empty() || b.match_key.is_empty() || mk == &b.match_key)
                })
            {
                b.token_id = Some(tid.clone());
                info!("🔗 RECONCILE: Discovered tokenId {} for bet {} ({}) via team+odds+matchKey",
                    tid, b.bet_id, b.value_team);
                needs_rewrite = true;
            }
        }
    }

    let post_count = active_bets.len();
    if pre_count != post_count {
        info!("🔄 RECONCILE: active_bets {} → {} (on-chain pending: {})",
            pre_count, post_count, onchain_pending_tids.len());
    }

    let local_onchain_count = active_bets.iter().filter(|b| b.token_id.is_some()).count();
    if local_onchain_count != onchain_pending_tids.len() {
        warn!("⚠️ INVARIANT MISMATCH: local on-chain verified={} vs on-chain pending={} — investigate!",
            local_onchain_count, onchain_pending_tids.len());
    }

    if needs_rewrite {
        rewrite_pending_claims_file(active_bets, pending_claims_path);
        info!("💾 RECONCILE: pending_claims.txt rewritten ({} entries)", active_bets.len());
    }

    active_bets.iter().map(|b| b.amount_usd).sum()
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
    #[serde(rename = "acceptedOdds")]
    accepted_odds: Option<f64>,
    #[serde(rename = "requestedOdds")]
    requested_odds: Option<f64>,
    #[serde(rename = "minOdds")]
    min_odds: Option<f64>,
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
    #[serde(rename = "tokenIds")]
    token_ids: Option<Vec<String>>,
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
    let mut normal_score = (if teams_match(&a1, &m1) { 1 } else { 0 })
                     + (if teams_match(&a2, &m2) { 1 } else { 0 });
    // Swapped: azuro.t1 ↔ market.t2
    let mut swap_score = (if teams_match(&a1, &m2) { 1 } else { 0 })
                   + (if teams_match(&a2, &m1) { 1 } else { 0 });

    let is_esports = matches!(
        market.sport.as_deref().unwrap_or(""),
        "cs2" | "dota-2" | "league-of-legends" | "valorant" | "esports"
    );

    if is_esports {
        // Shared Opponent Loophole: If one team matches cleanly, we deduct it's the exact same match.
        // E.g., "NAVI" == "Natus Vincere", but Faze matches perfectly -> normal_score=1. Give it 2!
        if normal_score == 1 && swap_score == 0 {
            normal_score = 2;
        } else if normal_score == 0 && swap_score == 1 {
            swap_score = 2;
        }
    }

    let ambiguous = normal_score == swap_score;

    if swap_score > normal_score {
        // Teams are swapped — flip market odds
        (market.odds_team2, market.odds_team1, true, ambiguous)
    } else {
        (market.odds_team1, market.odds_team2, false, ambiguous)
    }
}

fn normalized_market_key(market: Option<&str>) -> String {
    market
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "match_winner".to_string())
}

fn match_prefix_from_match_key(match_key: &str) -> String {
    match_key.split("::").next().unwrap_or("unknown").to_string()
}

#[derive(Debug, Default, Clone)]
struct LedgerRecoveryStats {
    recovered: usize,
    unresolved_total: usize,
    stale_12h: usize,
    stale_24h: usize,
    oldest_age_hours: Option<f64>,
}

fn recover_unresolved_accepts_from_ledger(
    active_bets: &mut Vec<ActiveBet>,
    ledger_settled_ids: &HashSet<String>,
) -> LedgerRecoveryStats {
    let mut stats = LedgerRecoveryStats::default();
    let ledger_path = "data/ledger.jsonl";
    if !Path::new(ledger_path).exists() {
        return stats;
    }

    let Ok(contents) = std::fs::read_to_string(ledger_path) else {
        return stats;
    };

    let mut tracked_bet_ids: HashSet<String> = active_bets.iter()
        .map(|b| b.bet_id.clone())
        .collect();
    let mut unresolved_accepts: HashMap<String, serde_json::Value> = HashMap::new();

    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = entry.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let Some(bet_id) = entry.get("bet_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if bet_id.is_empty() {
            continue;
        }

        match event {
            "ON_CHAIN_ACCEPTED" => {
                unresolved_accepts.insert(bet_id.to_string(), entry);
            }
            "WON" | "LOST" | "CANCELED" | "CLAIMED" | "ON_CHAIN_REJECTED" | "REJECTED" | "BET_FAILED" => {
                unresolved_accepts.remove(bet_id);
            }
            _ => {}
        }
    }

    stats.unresolved_total = unresolved_accepts.len();
    let now = Utc::now();
    for entry in unresolved_accepts.values() {
        if let Some(ts) = entry.get("ts").and_then(|v| v.as_str()) {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts) {
                let age_hours = (now - parsed.with_timezone(&Utc)).num_seconds() as f64 / 3600.0;
                stats.oldest_age_hours = Some(match stats.oldest_age_hours {
                    Some(current) => current.max(age_hours),
                    None => age_hours,
                });
                if age_hours >= UNRESOLVED_ACCEPTED_STALE_HOURS as f64 {
                    stats.stale_12h += 1;
                }
                if age_hours >= 24.0 {
                    stats.stale_24h += 1;
                }
            }
        }
    }

    for (bet_id, entry) in unresolved_accepts {
        if ledger_settled_ids.contains(&bet_id) || tracked_bet_ids.contains(&bet_id) {
            continue;
        }

        let match_key = entry.get("match_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if match_key.is_empty() {
            continue;
        }

        let market_key = entry.get("market_key")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let value_team = entry.get("value_team")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let amount_usd = entry.get("stake")
            .and_then(|v| v.as_f64())
            .or_else(|| entry.get("amount_usd").and_then(|v| v.as_f64()))
            .unwrap_or(0.0);
        let odds = entry.get("odds")
            .and_then(|v| v.as_f64())
            .or_else(|| entry.get("requested_odds").and_then(|v| v.as_f64()))
            .unwrap_or(0.0);
        let token_id = sanitize_token_id(
            entry.get("token_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        );
        let alert_id = entry.get("alert_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let placed_at = entry.get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("ledger_recovery")
            .to_string();
        let condition_id = entry.get("condition_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let outcome_id = entry.get("outcome_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let graph_bet_id = entry.get("graph_bet_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let path = entry.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("ledger_recovery")
            .to_string();

        active_bets.push(ActiveBet {
            alert_id,
            bet_id: bet_id.clone(),
            match_key,
            market_key,
            team1: value_team.clone(),
            team2: "?".to_string(),
            value_team,
            amount_usd,
            odds,
            placed_at,
            condition_id,
            outcome_id,
            graph_bet_id,
            token_id,
            path,
        });
        tracked_bet_ids.insert(bet_id);
        stats.recovered += 1;
    }

    stats
}

fn market_from_match_key(match_key: &str) -> String {
    if let Some((_, suffix)) = match_key.rsplit_once("::") {
        if suffix.ends_with("_winner") {
            return suffix.to_lowercase();
        }
    }
    "match_winner".to_string()
}

fn remap_execution_ids_from_state(
    state: &StateResponse,
    match_key: &str,
    team1: &str,
    team2: &str,
    value_side: u8,
) -> Option<(String, String)> {
    let desired_market = market_from_match_key(match_key);

    for item in &state.odds {
        if item.match_key != match_key {
            continue;
        }
        if !item.payload.bookmaker.starts_with("azuro_") {
            continue;
        }
        let item_market = normalized_market_key(item.payload.market.as_deref());
        if item_market != desired_market {
            continue;
        }

        let cond = match item.payload.condition_id.as_ref() {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };
        let out1 = match item.payload.outcome1_id.as_ref() {
            Some(o) if !o.is_empty() => o,
            _ => continue,
        };
        let out2 = match item.payload.outcome2_id.as_ref() {
            Some(o) if !o.is_empty() => o,
            _ => continue,
        };

        let direct = teams_match(team1, &item.payload.team1) && teams_match(team2, &item.payload.team2);
        let swapped = teams_match(team1, &item.payload.team2) && teams_match(team2, &item.payload.team1);

        if !direct && !swapped {
            continue;
        }

        let mapped_side = if direct {
            value_side
        } else if value_side == 1 {
            2
        } else {
            1
        };

        let out = if mapped_side == 1 { out1 } else { out2 };
        return Some((cond.clone(), out.clone()));
    }

    None
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
        let market_items_all: Vec<&&StateOddsItem> = items.iter()
            .filter(|i| !i.payload.bookmaker.starts_with("azuro_") && is_recent_seen_at(&i.seen_at, now))
            .collect();

        if azuro_items.is_empty() || market_items_all.is_empty() {
            continue;
        }
        let is_live = live_keys.contains_key(match_key.as_str());
        let live_score = live_keys.get(match_key.as_str()).map(|l| {
            format!("{}-{}", l.payload.score1, l.payload.score2)
        });
        let detailed_score = live_keys.get(match_key.as_str())
            .and_then(|l| l.payload.detailed_score.clone());

        // LIVE-ONLY mode: ignore prematch odds anomalies completely.
        if !is_live {
            continue;
        }

        let mut processed_markets: HashSet<String> = HashSet::new();

        for azuro_item in &azuro_items {
            let azuro = &azuro_item.payload;
            let azuro_market = normalized_market_key(azuro.market.as_deref());
            if !processed_markets.insert(azuro_market.clone()) {
                continue;
            }

            let market_items: Vec<&&StateOddsItem> = market_items_all
                .iter()
                .copied()
                .filter(|mi| {
                    let market_key = normalized_market_key(mi.payload.market.as_deref());
                    market_key == azuro_market
                })
                .collect();

            if market_items.is_empty() {
                debug!(
                    "ODDS_ANOMALY market alignment miss: match_key={} azuro_market={} azuro_bookmaker={}",
                    match_key,
                    azuro_market,
                    azuro.bookmaker
                );
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

        // CRITICAL: Suspended/placeholder MARKET odds detection
        // When a bookmaker suspends a market (goal, VAR, red card), they show
        // placeholder odds like 1.01-1.05 / 50-120+. These are NOT real prices.
            let min_market = avg_w1.min(avg_w2);
            let max_market = avg_w1.max(avg_w2);
            if min_market <= SUSPENDED_MARKET_MIN_ODDS || max_market >= SUSPENDED_MARKET_MAX_ODDS {
                reasons.push(format!("⚠️ SUSPENDED MARKET: trh odds {:.2}/{:.2} — placeholder/suspended!", avg_w1, avg_w2));
                penalty += 6; // Guarantees LOW → skip entirely
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
                if side2_ok {
                    if prefer_underdog_anomaly_override(&match_key, azuro.odds_team2, disc_w2, disc_w1) {
                        info!("🎾 ODDS ANOMALY {} UNDERDOG OVERRIDE: {:.1}% disc on {} beats favorite {:.1}% — allowing tennis exception",
                            match_key, disc_w2, azuro.team2, disc_w1);
                        2
                    } else {
                        info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                            match_key, disc_w2, azuro.team2);
                        1
                    }
                } else {
                    1
                }
            } else {
                if side2_ok {
                    if allow_underdog_anomaly_override(&match_key, azuro.odds_team2, disc_w2) {
                        info!("🎾 ODDS ANOMALY {} UNDERDOG OVERRIDE: {:.1}% disc on {} — allowing tennis exception",
                            match_key, disc_w2, azuro.team2);
                        2
                    } else {
                        info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                            match_key, disc_w2, azuro.team2);
                        0 // Favorit nemá edge → skip
                    }
                } else {
                    0 // Favorit nemá edge → skip
                }
            }
        } else {
            // team2 je favorit — jen side2
            if side2_ok {
                if side1_ok {
                    if prefer_underdog_anomaly_override(&match_key, azuro.odds_team1, disc_w1, disc_w2) {
                        info!("🎾 ODDS ANOMALY {} UNDERDOG OVERRIDE: {:.1}% disc on {} beats favorite {:.1}% — allowing tennis exception",
                            match_key, disc_w1, azuro.team1, disc_w2);
                        1
                    } else {
                        info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                            match_key, disc_w1, azuro.team1);
                        2
                    }
                } else {
                    2
                }
            } else {
                if side1_ok {
                    if allow_underdog_anomaly_override(&match_key, azuro.odds_team1, disc_w1) {
                        info!("🎾 ODDS ANOMALY {} UNDERDOG OVERRIDE: {:.1}% disc on {} — allowing tennis exception",
                            match_key, disc_w1, azuro.team1);
                        1
                    } else {
                        info!("⏭️ ODDS ANOMALY {} UNDERDOG-ONLY: {:.1}% disc on underdog {} — SKIPPING (favorit-only mode)",
                            match_key, disc_w1, azuro.team1);
                        0 // Favorit nemá edge → skip
                    }
                } else {
                    0 // Favorit nemá edge → skip
                }
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
                    market_key: azuro.market.clone().unwrap_or_else(|| "match_winner".to_string()),
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
                    live_score: live_score.clone(),
                    detailed_score: detailed_score.clone(),
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
                    market_key: azuro.market.clone().unwrap_or_else(|| "match_winner".to_string()),
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
                    live_score: live_score.clone(),
                    detailed_score: detailed_score.clone(),
                    game_id: azuro.game_id.clone(),
                    condition_id: azuro.condition_id.clone(),
                    outcome1_id: azuro.outcome1_id.clone(),
                    outcome2_id: azuro.outcome2_id.clone(),
                    outcome_id: azuro.outcome2_id.clone(),
                    chain: azuro.chain.clone(),
                });
            }
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
    let market_label = a.market_key.replace('_', " ");

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
    let market_sources: Vec<&str> = a.market_bookmaker
        .split('+')
        .filter(|s| !s.trim().is_empty())
        .collect();
    let source_count = market_sources.len() + 1;
    let source_list = if market_sources.is_empty() {
        a.azuro_bookmaker.clone()
    } else {
        format!("{} + {}", a.azuro_bookmaker, market_sources.join(" + "))
    };

    format!(
        "🎯 <b>#{}</b> {} <b>ODDS ANOMALY</b>\n\
         🏷️ <b>{}</b> | market: <b>{}</b> | path: <b>anomaly_odds</b> | conf: <b>{}</b>\n\
         🧩 <b>{}</b> vs <b>{}</b>{}{}\n\
         💡 Pick: <b>{}</b> @ <b>{:.2}</b>\n\
         📊 Azuro: {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         📊 Trh: {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         🧠 Why: <b>{:.1}%</b> value (azuro {:.2} vs trh {:.2}){}\n\
         🛰 Sources ({}): {}{}\n\
         🏦 {}\n\
         Reply: <code>{} YES $3</code> / <code>{} OPP $3</code> / <code>{} NO</code>",
        alert_id, conf_emoji, sport, market_label, a.confidence,
        a.team1, a.team2, live_line, swap_warn,
        value_team, azuro_odds,
        a.team1, a.azuro_w1, a.team2, a.azuro_w2,
        a.team1, a.market_w1, a.team2, a.market_w2,
        a.discrepancy_pct,
        azuro_odds, market_odds, reasons_text,
        source_count, source_list, url_line,
        exec_ready,
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

fn format_auto_bet_result_message(
    aid: u32,
    path: &str,
    match_key: &str,
    team: &str,
    odds: f64,
    stake: f64,
    bet_id: &str,
    bet_state: &str,
    auto_bet_count: u32,
    is_dry_run: bool,
) -> String {
    let sport = match_key.split("::").next().unwrap_or("?").to_uppercase();
    if is_dry_run {
        format!(
            "🧪 <b>AUTO-BET #{} DRY-RUN</b>\n\
             🏷️ <b>{}</b> | path: <b>{}</b>\n\
             💡 Pick: <b>{}</b> @ <b>{:.2}</b> | stake <b>${:.2}</b>",
            aid, sport, path, team, odds, stake
        )
    } else {
        let header = if bet_state == "Accepted" {
            "✅ <b>AUTO-BET #{aid} CONFIRMED</b>"
        } else if bet_state == "Created" || bet_state == "Pending" {
            "📨 <b>AUTO-BET #{aid} SUBMITTED</b>"
        } else {
            "✅ <b>AUTO-BET #{aid} PLACED</b>"
        };
        format!(
            "{}\n\
             🏷️ <b>{}</b> | path: <b>{}</b>\n\
             💡 Pick: <b>{}</b> @ <b>{:.2}</b> | stake <b>${:.2}</b>\n\
             🧾 Bet ID: <code>{}</code> | state: <b>{}</b>\n\
             📈 Auto-bets dnes: <b>{}</b>",
            header.replace("{aid}", &aid.to_string()),
            sport,
            path,
            team,
            odds,
            stake,
            bet_id,
            bet_state,
            auto_bet_count
        )
    }
}

fn format_auto_bet_failed_message(
    aid: u32,
    path: &str,
    match_key: &str,
    condition_id: &str,
    reason_code: &str,
    err: &str,
    retries: usize,
    rtt_ms: u128,
    pipeline_ms: u128,
    requested_odds: f64,
    min_odds: f64,
) -> String {
    let sport = match_key.split("::").next().unwrap_or("?").to_uppercase();
    format!(
        "❌ <b>AUTO-BET #{} FAILED</b>\n\
         🏷️ <b>{}</b> | path: <b>{}</b>\n\
         🧩 match: <b>{}</b>\n\
         🧠 reason: <b>{}</b>\n\
         🔧 condition: <code>{}</code>\n\
         📊 odds: {:.4} → min {:.4} | retry {}\n\
         ⏱ rtt {}ms | pipeline {}ms\n\
         📝 {}",
        aid,
        sport,
        path,
        match_key,
        reason_code,
        condition_id,
        requested_odds,
        min_odds,
        retries,
        rtt_ms,
        pipeline_ms,
        err,
    )
}

fn format_auto_bet_rejected_message(
    aid: u32,
    path: &str,
    match_key: &str,
    condition_id: &str,
    state: &str,
) -> String {
    let sport = match_key.split("::").next().unwrap_or("?").to_uppercase();
    format!(
        "❌ <b>AUTO-BET #{} REJECTED</b>\n\
         🏷️ <b>{}</b> | path: <b>{}</b>\n\
         🧩 match: <b>{}</b>\n\
         🔧 condition: <code>{}</code>\n\
         🧠 state: <b>{}</b>",
        aid, sport, path, match_key, condition_id, state
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

    let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let feed_hub_url = std::env::var("FEED_HUB_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let executor_url = std::env::var("EXECUTOR_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3030".to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    if token.trim().is_empty() {
        warn!("TELEGRAM_BOT_TOKEN not set — running without Telegram.");
    } else {
        let bot_id = tg_get_me(&client, &token).await.unwrap_or(0);
        if bot_id == 0 {
            warn!("Telegram configured, but getMe failed — continuing without Telegram.");
        } else {
            info!("Telegram bot started, bot_id={}", bot_id);
        }
    }

    // Discover chat_id: either from env or from first message
    let mut chat_id: Option<i64> = std::env::var("TELEGRAM_CHAT_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    let mut update_offset: i64 = 0;
    let mut sent_alerts: Vec<SentAlert> = Vec::new();
    let mut alert_counter: u32 = 0;
    let mut alert_map: HashMap<u32, OddsAnomaly> = HashMap::new();
    let mut msg_id_to_alert_id: HashMap<i64, u32> = HashMap::new();
    // Manual alert throttle per match_key (anti-spam)
    let mut manual_offer_last_sent: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut active_bets: Vec<ActiveBet> = Vec::new();
    // Tokens that are already settled in subgraph but not yet claimable on-chain.
    // These should NOT block new bets via MAX_CONCURRENT_PENDING (no longer risk exposure).
    let mut deferred_claim_tokens: HashSet<String> = HashSet::new();
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

    // === CONDITION FRESHNESS: track when each condition was last seen Active in /state ===
    // Used to measure staleness at bet time — feeds into WS state-feed decision
    let mut condition_last_seen: HashMap<String, std::time::Instant> = HashMap::new();

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
    /// Runtime override pro daily limit — nastaven přes /limit +X, reset na None každý nový den
    let mut daily_limit_override: Option<f64> = None;
    // === LOSS STREAK TRACKING ===
    let mut consecutive_losses: usize = 0;
    let mut loss_streak_pause_until: Option<std::time::Instant> = None;
    // === DASHBOARD CONFIG (read from data/dashboard_config.json) ===
    let mut dashboard_max_stake: Option<f64> = None;         // overrides AUTO_BET_STAKE_USD/.._LOW_USD cap
    let mut dashboard_sport_focus: Vec<String> = vec!["all".to_string()]; // ["all"] = no filter
    let mut dashboard_autobet_enabled: bool = true;          // kill switch from dashboard
    // Load dashboard config at boot
    {
        let cfg_path = "data/dashboard_config.json";
        if let Ok(contents) = std::fs::read_to_string(cfg_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(ms) = v["max_stake"].as_f64() {
                    if ms > 0.0 && ms < 100.0 { dashboard_max_stake = Some(ms); }
                }
                if let Some(arr) = v["sport_focus"].as_array() {
                    let sports: Vec<String> = arr.iter()
                        .filter_map(|s| s.as_str().map(|s| s.to_string()))
                        .collect();
                    if !sports.is_empty() { dashboard_sport_focus = sports; }
                }
                if let Some(ae) = v["autobet_enabled"].as_bool() {
                    dashboard_autobet_enabled = ae;
                }
                info!("📱 Dashboard config loaded: max_stake={:?}, sport_focus={:?}, autobet={}",
                    dashboard_max_stake, dashboard_sport_focus, dashboard_autobet_enabled);
            }
        }
    }
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
                        // Restore limit override if saved
                        if let Some(ov) = v["limit_override"].as_f64() {
                            if ov > DAILY_LOSS_LIMIT_USD {
                                daily_limit_override = Some(ov);
                                info!("📋 Restored limit override: ${:.0}", ov);
                            }
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

    // Reconcile daily P&L from today's ledger so restarts and duplicate claim paths
    // cannot leave daily_pnl.json out of sync with authoritative events.
    if Path::new("data/ledger.jsonl").exists() {
        if let Ok(contents) = std::fs::read_to_string("data/ledger.jsonl") {
            let mut ledger_daily_wagered = 0.0;
            let mut ledger_daily_returned = 0.0;
            let mut today_claimed_tokens: HashSet<String> = HashSet::new();
            let mut today_claimed_txs: HashSet<String> = HashSet::new();
            for line in contents.lines() {
                if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                    if !entry.get("ts").and_then(|v| v.as_str()).map(|s| s.starts_with(&daily_date)).unwrap_or(false) {
                        continue;
                    }
                    match entry.get("event").and_then(|v| v.as_str()).unwrap_or("") {
                        "PLACED" => {
                            let stake = entry.get("amount_usd").and_then(|v| v.as_f64())
                                .or_else(|| entry.get("stake").and_then(|v| v.as_f64()))
                                .unwrap_or(0.0);
                            ledger_daily_wagered += stake;
                        }
                        "EXECUTOR_CLAIM" => {
                            let mut count_this_claim = false;
                            if let Some(token_ids) = entry.get("tokenIds").and_then(|v| v.as_array()) {
                                for token in token_ids {
                                    if let Some(tid) = token.as_str() {
                                        if today_claimed_tokens.insert(tid.to_string()) {
                                            count_this_claim = true;
                                        }
                                    }
                                }
                            }
                            if !count_this_claim {
                                if let Some(tx) = entry.get("txHash").and_then(|v| v.as_str()) {
                                    count_this_claim = today_claimed_txs.insert(tx.to_string());
                                }
                            }
                            if count_this_claim {
                                ledger_daily_returned += entry.get("totalPayoutUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if (ledger_daily_wagered - daily_wagered).abs() > 0.009 || (ledger_daily_returned - daily_returned).abs() > 0.009 {
                daily_wagered = ledger_daily_wagered;
                daily_returned = ledger_daily_returned;
                let _ = std::fs::write("data/daily_pnl.json",
                    serde_json::json!({"date": daily_date, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                info!("📋 Reconciled daily_pnl from ledger: wagered={:.2} returned={:.2}", daily_wagered, daily_returned);
            }
        }
    }

    // === MUTE MANUAL ALERTS (toggle via /nabidka and /nabidkaup) ===
    // When true, only auto-bet confirmations + portfolio + claim messages are sent.
    // Manual "opportunity" alerts (score-edge MEDIUM, odds anomaly manual) are suppressed.
    let mut mute_manual_alerts = false;

    // === WATCHDOG: SAFE MODE ===
    let mut safe_mode = false;
    let mut last_good_data: Option<std::time::Instant> = None;

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

    // === SQLITE LEDGER MIRROR (optional) ===
    // Goal: hard, queryable 48h dataset: edge %, source count, fill result, P&L.
    // Keeps existing JSONL ledger as the source-of-truth.
    fn env_bool(name: &str, default: bool) -> bool {
        match std::env::var(name) {
            Ok(v) => {
                let s = v.trim().to_ascii_lowercase();
                !(s == "0" || s == "false" || s == "off" || s == "no")
            }
            Err(_) => default,
        }
    }

    let sqlite_ledger_tx: Option<mpsc::UnboundedSender<serde_json::Value>> = if env_bool("BET_SQLITE", true) {
        let db_path = std::env::var("BET_SQLITE_PATH").unwrap_or_else(|_| "data/bets.sqlite".to_string());
        let db_path_for_thread = db_path.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<serde_json::Value>();
        std::thread::spawn(move || {
            let conn = match Connection::open(&db_path_for_thread) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[sqlite] open failed: {} err={}", db_path_for_thread, e);
                    return;
                }
            };

            let _ = conn.pragma_update(None, "journal_mode", "WAL");
            let _ = conn.pragma_update(None, "synchronous", "NORMAL");
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS bet_events(\
                    id INTEGER PRIMARY KEY AUTOINCREMENT,\
                    ts TEXT,\
                    event TEXT,\
                    bet_id TEXT,\
                    alert_id INTEGER,\
                    match_key TEXT,\
                    path TEXT,\
                    odds REAL,\
                    stake_usd REAL,\
                    edge_pct REAL,\
                    source_count INTEGER,\
                    on_chain_state TEXT,\
                    payout_usd REAL,\
                    amount_usd REAL,\
                    error TEXT,\
                    raw_json TEXT NOT NULL\
                );\
                CREATE INDEX IF NOT EXISTS idx_bet_events_bet_id ON bet_events(bet_id);\
                CREATE INDEX IF NOT EXISTS idx_bet_events_ts ON bet_events(ts);\
                CREATE TABLE IF NOT EXISTS bets(\
                    bet_id TEXT PRIMARY KEY,\
                    first_ts TEXT,\
                    last_ts TEXT,\
                    match_key TEXT,\
                    path TEXT,\
                    odds REAL,\
                    stake_usd REAL,\
                    edge_pct REAL,\
                    source_count INTEGER,\
                    on_chain_state TEXT,\
                    result TEXT,\
                    payout_usd REAL,\
                    pnl_usd REAL\
                );"
            );

            while let Some(entry) = rx.blocking_recv() {
                let raw_json = entry.to_string();
                let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                let event = entry.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let bet_id = entry.get("bet_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                let alert_id = entry.get("alert_id").and_then(|v| v.as_i64());
                let match_key = entry.get("match_key").and_then(|v| v.as_str()).map(|s| s.to_string());
                let path = entry.get("path").and_then(|v| v.as_str()).map(|s| s.to_string());
                let odds = entry.get("odds").and_then(|v| v.as_f64());
                let stake_usd = entry
                    .get("amount_usd").and_then(|v| v.as_f64())
                    .or_else(|| entry.get("stake").and_then(|v| v.as_f64()));
                let edge_pct = entry.get("edge_pct").and_then(|v| v.as_f64());
                let source_count = entry
                    .get("anomaly_market_source_count").and_then(|v| v.as_i64())
                    .or_else(|| entry.get("market_source_count").and_then(|v| v.as_i64()))
                    .or_else(|| entry.get("source_count").and_then(|v| v.as_i64()));
                let on_chain_state = entry.get("on_chain_state").and_then(|v| v.as_str()).map(|s| s.to_string());
                let payout_usd = entry.get("payout_usd").and_then(|v| v.as_f64());
                let amount_usd = entry.get("amount_usd").and_then(|v| v.as_f64());
                let err = entry
                    .get("error").and_then(|v| v.as_str())
                    .or_else(|| entry.get("reason").and_then(|v| v.as_str()))
                    .map(|s| s.to_string());

                let _ = conn.execute(
                    "INSERT INTO bet_events(\
                        ts,event,bet_id,alert_id,match_key,path,odds,stake_usd,edge_pct,source_count,on_chain_state,payout_usd,amount_usd,error,raw_json\
                    ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                    rusqlite::params![
                        ts,
                        event,
                        bet_id.as_deref(),
                        alert_id,
                        match_key.as_deref(),
                        path.as_deref(),
                        odds,
                        stake_usd,
                        edge_pct,
                        source_count,
                        on_chain_state.as_deref(),
                        payout_usd,
                        amount_usd,
                        err.as_deref(),
                        raw_json,
                    ],
                );

                if let Some(bid) = bet_id.as_deref() {
                    // Ensure summary row exists + bump last_ts.
                    let _ = conn.execute(
                        "INSERT INTO bets(bet_id, first_ts, last_ts) VALUES (?1, ?2, ?2)\
                         ON CONFLICT(bet_id) DO UPDATE SET last_ts=excluded.last_ts",
                        rusqlite::params![bid, ts],
                    );

                    // Event-specific enrichment.
                    match event {
                        "PLACED" => {
                            let _ = conn.execute(
                                "UPDATE bets SET \
                                    match_key=COALESCE(match_key, ?2),\
                                    path=COALESCE(path, ?3),\
                                    odds=COALESCE(odds, ?4),\
                                    stake_usd=COALESCE(stake_usd, ?5),\
                                    edge_pct=COALESCE(edge_pct, ?6),\
                                    source_count=COALESCE(source_count, ?7),\
                                    on_chain_state=COALESCE(on_chain_state, ?8),\
                                    result='PLACED'\
                                 WHERE bet_id=?1",
                                rusqlite::params![
                                    bid,
                                    match_key.as_deref(),
                                    path.as_deref(),
                                    odds,
                                    stake_usd,
                                    edge_pct,
                                    source_count,
                                    on_chain_state.as_deref(),
                                ],
                            );
                        }
                        "ON_CHAIN_ACCEPTED" | "ON_CHAIN_REJECTED" | "REJECTED" | "BET_FAILED" => {
                            let _ = conn.execute(
                                "UPDATE bets SET \
                                    on_chain_state=COALESCE(on_chain_state, ?2),\
                                    result=?3\
                                 WHERE bet_id=?1",
                                rusqlite::params![bid, on_chain_state.as_deref(), event],
                            );
                        }
                        "WON" | "LOST" | "CANCELED" => {
                            let stake_for_pnl = amount_usd.or(stake_usd).unwrap_or(0.0);
                            let payout_for_pnl = payout_usd.unwrap_or(0.0);
                            let pnl = match event {
                                "WON" => payout_for_pnl - stake_for_pnl,
                                "CANCELED" => payout_for_pnl - stake_for_pnl,
                                "LOST" => -stake_for_pnl,
                                _ => 0.0,
                            };
                            let _ = conn.execute(
                                "UPDATE bets SET \
                                    result=?2,\
                                    payout_usd=COALESCE(payout_usd, ?3),\
                                    pnl_usd=?4\
                                 WHERE bet_id=?1",
                                rusqlite::params![bid, event, payout_usd, pnl],
                            );
                        }
                        _ => {}
                    }
                }
            }
        });
        info!("🗄️ SQLite ledger enabled: {} (set BET_SQLITE=0 to disable)", db_path);
        Some(tx)
    } else {
        None
    };

    // === PERMANENT BET LEDGER (append-only, NEVER deleted) ===
    let ledger_path = "data/ledger.jsonl";
    let ledger_write = |event: &str, data: &serde_json::Value| {
        let mut entry = data.clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("ts".to_string(), serde_json::json!(Utc::now().to_rfc3339()));
            obj.insert("event".to_string(), serde_json::json!(event));
        }
        let entry_for_sqlite = entry.clone();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(ledger_path) {
            use std::io::Write;
            let _ = writeln!(f, "{}", entry);
        }
        if let Some(tx) = &sqlite_ledger_tx {
            let _ = tx.send(entry_for_sqlite);
        }
    };

    // === CONDITION BLACKLIST: track conditions that got "not active" / "paused" errors ===
    // After a condition fails with these errors, it rarely recovers → skip all future bets on it
    // Eliminates ~88 repeated BET_FAILED on dead conditions per historical data
    let mut blacklisted_conditions: HashMap<String, std::time::Instant> = HashMap::new();
    const CONDITION_BLACKLIST_TTL_SECS: u64 = 600; // 10 min — allow retry after cooldown

    // === MATCH-LEVEL BLACKLIST: block entire match when conditions keep dying ===
    // After ConditionNotRunning or on-chain Rejected, the match's conditions are usually all dead.
    // New condition_ids get generated per score update → condition-level blacklist alone is insufficient.
    let mut blacklisted_matches: HashMap<String, std::time::Instant> = HashMap::new();
    const MATCH_BLACKLIST_TTL_SECS: u64 = 600; // 10 min — match-level cooldown (raised from 300s: 3x CondNR on same match within 16min)

    // === DEDUP: track already-bet match keys + condition IDs (persisted across restarts) ===
    let bet_history_path = "data/bet_history.txt";
    let mut already_bet_matches: HashSet<String> = HashSet::new();
    let mut already_bet_conditions: HashSet<String> = HashSet::new();
    // BUG #1 FIX: Also track base match keys (without ::mapN_winner suffix)
    // to prevent multiple map-winner bets on the same match (triple exposure)
    let mut already_bet_base_matches: HashSet<String> = HashSet::new();
    // Subset of already_bet_base_matches: only entries from MAP_WINNER bets (not match_winner).
    // Used to allow map2↔map3 sibling bets while still blocking match_winner↔map_winner crosses.
    let mut already_bet_map_winners: HashSet<String> = HashSet::new();
    // Load from file on startup
    if Path::new(bet_history_path).exists() {
        if let Ok(contents) = std::fs::read_to_string(bet_history_path) {
            let now_utc = Utc::now();
            let mut loaded_total: usize = 0;
            let mut loaded_fresh: usize = 0;
            for line in contents.lines() {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 2 {
                    loaded_total += 1;
                    let is_fresh = if parts.len() >= 5 {
                        chrono::DateTime::parse_from_rfc3339(parts[4])
                            .map(|ts| (now_utc - ts.with_timezone(&Utc)).num_hours() <= DEDUP_HISTORY_LOOKBACK_HOURS)
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    if !is_fresh {
                        continue;
                    }
                    already_bet_matches.insert(parts[0].to_string());
                    // Extract base match key (strip ::mapN_winner suffix)
                    let base_key = strip_map_winner_suffix(parts[0]);
                    let is_map_winner_entry = base_key != parts[0];
                    already_bet_conditions.insert(scoped_condition_key(&base_key, parts[1]));
                    if is_map_winner_entry {
                        already_bet_map_winners.insert(base_key.clone());
                    }
                    already_bet_base_matches.insert(base_key);
                    loaded_fresh += 1;
                }
            }
            info!("📋 Loaded {} fresh dedup entries from history ({} total scanned, lookback={}h, {} base matches)",
                loaded_fresh, loaded_total, DEDUP_HISTORY_LOOKBACK_HOURS, already_bet_base_matches.len());
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
                        market_key: "unknown".to_string(),
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
                        path: "loaded".to_string(),
                    });
                }
            }
            info!("📋 Loaded {} pending claims from file", active_bets.len());
        }
    }

    // If no chat_id, wait for user to send /start (timeboxed; never block auto-bets forever)
    if chat_id.is_none() && !token.trim().is_empty() {
        info!("No TELEGRAM_CHAT_ID set. Waiting for /start message from user...");
        info!("Open Telegram and send /start to your bot");

        let deadline = std::time::Instant::now() + Duration::from_secs(25);

        loop {
            if std::time::Instant::now() >= deadline {
                warn!("Timed out waiting for /start — continuing without Telegram.");
                break;
            }
            match tg_get_updates(&client, &token, update_offset).await {
                Ok(updates) => {
                    for u in &updates.result {
                        update_offset = u.update_id + 1;
                        if let Some(msg) = &u.message {
                            let text = msg.text.as_deref().unwrap_or("");
                            if text.starts_with("/start") {
                                chat_id = Some(msg.chat.id);
                                info!("Chat ID discovered: {}", msg.chat.id);
                                let _ = tg_send_message(&client, &token, msg.chat.id,
                                    &format!(
                                        "🤖 <b>RustMisko Alert Bot v3</b> activated!\n\n\
                                         Automatický CS2 Azuro betting system.\n\
                                         Alert → Reply → BET → AUTO-CASHOUT.\n\n\
                                         ⚙️ Min edge: 5%\n\
                                         📡 Polling: 30s\n\
                                         🏠 Feed Hub: {}\n\
                                         🔧 Executor: {}", feed_hub_url, executor_url
                                    )
                                ).await;
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

    let chat_id = chat_id.unwrap_or(0);
    if chat_id != 0 {
        info!("Alert bot running. chat_id={}, feed_hub={}, executor={}", chat_id, feed_hub_url, executor_url);
    } else {
        info!("Alert bot running WITHOUT Telegram. feed_hub={}, executor={}", feed_hub_url, executor_url);
    }

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
        format!("🤖 <b>AUTO-BET v5: ON</b>\n\n\
                 <b>Path A — Score Edge:</b>\n   \
                 CS2/Esports: map_winner, edge ≥12%\n   \
                 Tennis: match_winner, edge ≥12% (set_diff≥1)\n   \
                 Basketball: match_winner, edge ≥12%, stake 0.5x\n   \
                 Football: match_winner, edge ≥18% (goal_diff≥2)\n\n\
                 <b>Path B — Odds Anomaly:</b>\n   \
                 Favorit-only, HIGH conf, sources ≥{}\n   \
                 Tennis: set_diff≥1 | Football: ON | Basketball: ON\n   \
                 Stake: ${:.1}×(1.25/odds)^1.5 | Identická odds blokována\n\n\
                 <b>Sdílené limity:</b>\n   \
                 Odds: {:.2}–{:.2} | Stake: ${:.0}\n   \
                 Daily loss: ${:.0} | Watchdog: {}s\n\
                 💰 <b>AUTO-CLAIM: ON</b> (každých {}s)",
                AUTO_BET_MIN_MARKET_SOURCES,
                AUTO_BET_ODDS_ANOMALY_STAKE_BASE_USD,
                AUTO_BET_MIN_ODDS, AUTO_BET_MAX_ODDS, AUTO_BET_STAKE_USD,
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
             Monitoruji Azuro vs HLTV/Fortuna score+odds.\n\
             Path A: Score Edge → AUTO-BET / Alert\n\
             Path B: Odds Anomaly → AUTO-BET (favorit-only)\n\n\
             /status — stav systému + executor + bety\n\
             /odds — aktuální anomálie\n\
             /bets — aktivní sázky\n\
             /help — nápověda", executor_status, auto_bet_info
        )
    ).await?;

    // ====================================================================
    // WS STATE GATE — real-time condition state daemon
    // ====================================================================
    let ws_state_gate_enabled = std::env::var("WS_STATE_GATE")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false); // DEFAULT OFF — SHADOW-WS in feed-hub is primary; set WS_STATE_GATE=true only for explicit legacy gating
    let ws_condition_cache: WsConditionCache = Arc::new(RwLock::new(HashMap::new()));
    let (ws_sub_tx, ws_sub_rx) = mpsc::channel::<Vec<String>>(64);
    if ws_state_gate_enabled {
        let cache_clone = ws_condition_cache.clone();
        tokio::spawn(async move {
            run_ws_gate(cache_clone, ws_sub_rx).await;
        });
        info!("🔌 [WS-GATE] Legacy WebSocket condition gate ENABLED (opt-in duplicate of SHADOW-WS; kill-switch: WS_STATE_GATE=false)");
    } else {
        info!("⚠️ [WS-GATE] Legacy WebSocket condition gate DISABLED by default; SHADOW-WS remains primary");
    }

    let mut poll_ticker = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    let mut cashout_ticker = tokio::time::interval(Duration::from_secs(CASHOUT_CHECK_SECS));
    let mut claim_ticker = tokio::time::interval(Duration::from_secs(CLAIM_CHECK_SECS));
    let mut portfolio_ticker = tokio::time::interval(Duration::from_secs(PORTFOLIO_REPORT_SECS));
    let mut tg_ticker = tokio::time::interval(Duration::from_secs(3));
    // Bets that have been settled and claimed (to avoid re-processing)
    let mut settled_bet_ids: HashSet<String> = HashSet::new();
    // Ledger-dedup: once WON/LOST/CANCELED is written, NEVER write again
    // (separate from settled_bet_ids which can be un-settled by deferred_bets logic)
    let mut ledger_settled_ids: HashSet<String> = HashSet::new();
    let mut claimed_token_ids: HashSet<String> = HashSet::new();
    let mut claimed_tx_hashes: HashSet<String> = HashSet::new();
    // BUG FIX: Load ledger_settled_ids from ledger.jsonl on startup to prevent duplicate writes after restart
    if Path::new("data/ledger.jsonl").exists() {
        if let Ok(contents) = std::fs::read_to_string("data/ledger.jsonl") {
            for line in contents.lines() {
                if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                    let event = entry.get("event").and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(event, "WON" | "LOST" | "CANCELED") {
                        if let Some(bid) = entry.get("bet_id").and_then(|v| v.as_str()) {
                            ledger_settled_ids.insert(bid.to_string());
                        }
                    }
                    if event == "EXECUTOR_CLAIM" {
                        if let Some(tx) = entry.get("txHash").and_then(|v| v.as_str()) {
                            claimed_tx_hashes.insert(tx.to_string());
                        }
                        if let Some(token_ids) = entry.get("tokenIds").and_then(|v| v.as_array()) {
                            for token in token_ids {
                                if let Some(tid) = token.as_str() {
                                    claimed_token_ids.insert(tid.to_string());
                                }
                            }
                        }
                    }
                }
            }
            info!("📋 Loaded {} ledger-settled IDs from ledger.jsonl (prevents duplicate WON/LOST/CANCELED writes)",
                ledger_settled_ids.len());
            info!("📋 Loaded {} claimed token IDs and {} claim tx hashes from ledger",
                claimed_token_ids.len(), claimed_tx_hashes.len());
        }

        let recovery_stats = recover_unresolved_accepts_from_ledger(&mut active_bets, &ledger_settled_ids);
        if recovery_stats.recovered > 0 {
            info!(
                "🩹 Recovered {} unresolved ON_CHAIN_ACCEPTED bets from ledger into active_bets",
                recovery_stats.recovered
            );
        }
        if recovery_stats.unresolved_total > 0 {
            info!(
                "📋 Startup settlement audit: unresolved={} stale12h={} stale24h={} oldest={:.1}h",
                recovery_stats.unresolved_total,
                recovery_stats.stale_12h,
                recovery_stats.stale_24h,
                recovery_stats.oldest_age_hours.unwrap_or(0.0)
            );
            ledger_write("SETTLEMENT_RECONCILE_AUDIT", &serde_json::json!({
                "phase": "startup",
                "unresolved_total": recovery_stats.unresolved_total,
                "stale_12h": recovery_stats.stale_12h,
                "stale_24h": recovery_stats.stale_24h,
                "oldest_age_hours": recovery_stats.oldest_age_hours,
                "recovered": recovery_stats.recovered,
            }));
        }
    }
    // Running profit/loss tracker
    let mut total_wagered: f64 = 0.0;
    let mut total_returned: f64 = 0.0;
    // Safety net counter for /auto-claim (every 5th claim tick = ~5 min)
    let mut claim_safety_counter: u32 = 0;
    let mut claim_reconcile_counter: u32 = 0;
    let mut last_reconcile_audit_signature = String::new();
    // Session start time for portfolio reporting
    let session_start = Utc::now();
    // WS gate rolling counters (session-level, shown in portfolio)
    let mut ws_gate_active_count: u32 = 0;       // passed WS gate (Active)
    let mut ws_gate_not_active_count: u32 = 0;    // dropped by WS (NotActive)
    let mut ws_gate_stale_fallback_count: u32 = 0; // WS stale → GQL fallback
    let mut ws_gate_nodata_fallback_count: u32 = 0; // WS no data → GQL fallback
    // In-flight TTL: bets with token_id=None older than this are considered stale
    const INFLIGHT_TTL_SECS: i64 = 240; // 4 minutes

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
                if let Some(last_good) = last_good_data {
                    if last_good.elapsed().as_secs() > WATCHDOG_TIMEOUT_SECS && !safe_mode {
                        safe_mode = true;
                        let elapsed = last_good.elapsed().as_secs();
                        warn!("⚠️ SAFE MODE: Feed-hub silent for {}s > {}s threshold", elapsed, WATCHDOG_TIMEOUT_SECS);
                        let _ = tg_send_message(&client, &token, chat_id,
                            &format!("⚠️ <b>SAFE MODE ACTIVATED</b>\n\nFeed-hub neodpovídá {}s.\nAuto-bety POZASTAVENY.\nAlerty stále fungují.\n\nZkontroluj Chrome tab + Tampermonkey.", elapsed)
                        ).await;
                        log_event("SAFE_MODE_ON", &serde_json::json!({"elapsed_secs": elapsed}));
                    }
                }

                // 1. Check /state for cross-bookmaker odds anomalies
                match client.get(format!("{}/state", feed_hub_url)).send().await {
                    Ok(resp) => {
                        match resp.json::<StateResponse>().await {
                            Ok(state) => {
                                // === WATCHDOG: feed-hub is alive ===
                                last_good_data = Some(std::time::Instant::now());
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
                                    daily_limit_override = None; // clear override on new day
                                    // Lock start-of-day bankroll for today's loss limit calc
                                    start_of_day_bankroll = current_bankroll;
                                    info!("📅 SOD bankroll locked: ${:.2}", start_of_day_bankroll);
                                    // Persist SOD bankroll for day-rollover
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
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

                                // === DASHBOARD LIMIT SIGNAL (file-based) ===
                                {
                                    let signal_path = "data/limit_signal.json";
                                    if let Ok(contents) = std::fs::read_to_string(signal_path) {
                                        if let Ok(sig) = serde_json::from_str::<serde_json::Value>(&contents) {
                                            if sig.get("action").and_then(|a| a.as_str()) == Some("raise_limit") {
                                                if let Some(new_lim) = sig.get("new_limit").and_then(|v| v.as_f64()) {
                                                    if new_lim > 0.0 && new_lim <= 500.0 {
                                                        let old = daily_limit_override.unwrap_or(DAILY_LOSS_LIMIT_USD);
                                                        daily_limit_override = Some(new_lim);
                                                        daily_loss_alert_sent = false;
                                                        daily_loss_last_reminder = None;
                                                        let net_now = (daily_wagered - daily_returned).max(0.0);
                                                        let room = (new_lim - net_now).max(0.0);
                                                        info!("⚡ DASHBOARD LIMIT SIGNAL: ${:.0} → ${:.0} (room=${:.2})", old, new_lim, room);
                                                        ledger_write("LIMIT_OVERRIDE", &serde_json::json!({
                                                            "old_limit": old, "new_limit": new_lim,
                                                            "net_loss_now": net_now, "room": room,
                                                            "trigger": "dashboard"
                                                        }));
                                                        if chat_id > 0 {
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format!("⚡ <b>LIMIT via Dashboard</b>\n${:.0} → <b>${:.0}</b> | Room: ${:.2}",
                                                                    old, new_lim, room)).await;
                                                        }
                                                    }
                                                }
                                                // Delete signal file after processing
                                                let _ = std::fs::remove_file(signal_path);
                                            }
                                        }
                                    }
                                }

                                // === DASHBOARD CONFIG RELOAD (every poll) ===
                                {
                                    let cfg_path = "data/dashboard_config.json";
                                    if let Ok(contents) = std::fs::read_to_string(cfg_path) {
                                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&contents) {
                                            if let Some(ms) = v["max_stake"].as_f64() {
                                                if ms > 0.0 && ms < 100.0 {
                                                    dashboard_max_stake = Some(ms);
                                                }
                                            }
                                            if let Some(arr) = v["sport_focus"].as_array() {
                                                let sports: Vec<String> = arr.iter()
                                                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                                                    .collect();
                                                if !sports.is_empty() { dashboard_sport_focus = sports; }
                                            }
                                            if let Some(ae) = v["autobet_enabled"].as_bool() {
                                                dashboard_autobet_enabled = ae;
                                            }
                                        }
                                    }
                                }

                                // === DAILY LOSS CAP NOTIFICATION ===
                                // NET loss = settled losses minus claimed returns
                                // e.g. wagered=$20 on losses, returned=$30 from wins => net = -$10 (profit!)
                                let daily_net_loss = (daily_wagered - daily_returned).max(0.0);
                                // Effective daily limit: min(hard $30, tier-based cap)
                                // Uses START-OF-DAY bankroll to prevent "shrinking box" during losing streaks
                                let effective_daily_limit = {
                                    let (_, _, _, dl_frac, _) = get_exposure_caps(start_of_day_bankroll);
                                    daily_limit_override.unwrap_or_else(|| DAILY_LOSS_LIMIT_USD.min(start_of_day_bankroll * dl_frac))
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
                                        let lim_display = daily_limit_override.unwrap_or(DAILY_LOSS_LIMIT_USD);
                                        let msg = format!(
                                            "🛑 <b>DAILY LOSS LIMIT HIT</b>\n\nDnešní NET loss: <b>${:.2}</b> (wagered ${:.2} - returned ${:.2})\nLimit: <b>${:.2}</b> (min of ${:.0}{}, {:.0}% SOD BR=${:.0})\n\n🤖 Auto-bety jsou pozastavené do dalšího dne nebo ručního resetu.\n📡 Monitoring + alerty jedou dál.\n\n💡 Navýšit limit: /limit +10",
                                            daily_net_loss,
                                            daily_wagered,
                                            daily_returned,
                                            effective_daily_limit,
                                            lim_display,
                                            if daily_limit_override.is_some() { " ⚡override" } else { " hard" },
                                            get_exposure_caps(start_of_day_bankroll).3 * 100.0,
                                            start_of_day_bankroll,
                                        );
                                        let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                                        daily_loss_alert_sent = true;
                                        daily_loss_last_reminder = Some(now_utc);
                                    }
                                }

                                // === CONDITION FRESHNESS: update last-seen for all active conditions ===
                                let poll_instant = std::time::Instant::now();
                                for item in &state.odds {
                                    if let Some(cid) = &item.payload.condition_id {
                                        if !cid.is_empty() {
                                            condition_last_seen.insert(cid.clone(), poll_instant);
                                        }
                                    }
                                }
                                // GC: remove conditions not seen in 10 minutes
                                condition_last_seen.retain(|_, ts| poll_instant.duration_since(*ts).as_secs() < 600);

                                // === WS GATE: subscribe new condition IDs to WS stream ===
                                if ws_state_gate_enabled {
                                    let new_cids: Vec<String> = state.odds.iter()
                                        .filter_map(|item| item.payload.condition_id.as_ref())
                                        .filter(|cid| !cid.is_empty())
                                        .cloned()
                                        .collect();
                                    if !new_cids.is_empty() {
                                        let _ = ws_sub_tx.try_send(new_cids);
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
                                        market_key: edge.market_key.clone(),
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
                                        confidence_reasons: vec![
                                            format!("Score {}-{} → edge {:.1}%", edge.score1, edge.score2, edge.edge_pct),
                                            format!(
                                                "esports_classifier family={} confidence={} reason={}",
                                                edge.esports_family.unwrap_or("unknown"),
                                                edge.esports_confidence,
                                                edge.esports_reason,
                                            ),
                                        ],
                                        teams_swapped: false,
                                        is_live: true,
                                        live_score: Some(format!("{}-{}", edge.score1, edge.score2)),
                                        detailed_score: edge.detailed_score.clone(),
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
                                    let mut cond_id_str = anomaly.condition_id.as_deref().unwrap_or("").to_string();
                                    let match_key_for_bet = edge.match_key.clone();
                                    let base_match_key = strip_map_winner_suffix(&match_key_for_bet);

                                    // === RE-BET LOGIC: check if we already bet, and if re-bet is allowed ===
                                    let is_inflight = (!cond_id_str.is_empty() && inflight_conditions.contains(&cond_id_str))
                                        || inflight_conditions.contains(&match_key_for_bet);

                                    // BUG FIX: Prevent match_winner↔map_winner cross-market overexposure.
                                    // REFINED: map2_winner↔map3_winner sibling bets are ALLOWED when the previous bet
                                    // was also a map-level market (different cond ID, different settlement event).
                                    // match_cap/trim_stake guard total exposure on the base match.
                                    let is_candidate_map_winner = match_key_for_bet != base_match_key;
                                    let base_already_bet = already_bet_base_matches.contains(&base_match_key)
                                        && is_candidate_map_winner
                                        && !already_bet_map_winners.contains(&base_match_key); // existing was match_winner → still block

                                    let mut scoped_cond_key = (!cond_id_str.is_empty())
                                        .then(|| scoped_condition_key(&base_match_key, &cond_id_str));

                                    let (already_bet_this, rebet_ok) = if is_inflight {
                                        (true, false) // In-flight → always block
                                    } else if base_already_bet {
                                        // Previous was match_winner → block this map-winner (cross-market triple exposure guard)
                                        info!("🛡️ BASE-MATCH DEDUP: {} blocked (base {} has match_winner bet, blocking map variant)",
                                            match_key_for_bet, base_match_key);
                                        (true, false)
                                    } else if scoped_cond_key.as_ref().is_some_and(|key| already_bet_conditions.contains(key))
                                        || already_bet_matches.contains(&match_key_for_bet) {
                                        // Already bet → check if re-bet is allowed (only when FF enabled)
                                        let can_rebet = FF_REBET_ENABLED && !cond_id_str.is_empty() && {
                                            let cond_exp_rb = scoped_cond_key.as_ref()
                                                .and_then(|key| condition_exposure.get(key))
                                                .copied()
                                                .unwrap_or(0.0);
                                            let match_exp_rb = match_exposure.get(&base_match_key).copied().unwrap_or(0.0);
                                            let (_, cond_frac, match_frac, _, _) = get_exposure_caps(current_bankroll);
                                            let cond_cap_left = (current_bankroll * cond_frac - cond_exp_rb).max(0.0);
                                            let match_cap_left = (current_bankroll * match_frac - match_exp_rb).max(0.0);
                                            if let Some(rb_state) = scoped_cond_key.as_ref().and_then(|key| rebet_tracker.get(key)) {
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
                                    let sport_raw = edge.match_key.split("::").next().unwrap_or("?");
                                    let sport = effective_score_edge_sport(
                                        &edge.match_key,
                                        edge.resolved_sport.as_deref(),
                                        edge.esports_family,
                                    );
                                    let (sport_auto_allowed, mut sport_min_edge, sport_multiplier, preferred_market) = get_sport_config(sport);
                                    let sport_live_enabled = sport_score_edge_live_enabled(sport);
                                    let sport_dry_run_enabled = sport_score_edge_dry_run_enabled(sport);
                                    // Football: dynamic edge threshold by minute
                                    if sport == "football" {
                                        sport_min_edge = dynamic_football_min_edge(edge.detailed_score.as_deref());
                                    }
                                    // CS2 map_winner: lower threshold to 28% — cs2:: is historically profitable (71% WR),
                                    // and round-level score-edge with 80%+ win prob is high-certainty.
                                    // Keep 38% for match_winner and other sports.
                                    if (sport == "cs2" || (sport == "esports" && edge.esports_family == Some("cs2"))) {
                                        let is_map_winner_bet = edge.market_key.starts_with("map") && edge.market_key.ends_with("_winner");
                                        if is_map_winner_bet {
                                            sport_min_edge = 28.0;
                                        }
                                    }
                                    // Dynamic base stake: bankroll-scaled instead of hardcoded $3
                                    let mut base_stake = dynamic_base_stake(current_bankroll, sport);
                                    // Dashboard max_stake override (caps the calculated stake)
                                    if let Some(max_s) = dashboard_max_stake {
                                        base_stake = base_stake.min(max_s);
                                    }
                                    let score_stake_mult = score_edge_stake_multiplier(edge, sport, azuro_odds);

                                    // Regime-based sizing (Phase 1): use Kelly/3 when true_p is known
                                    let raw_stake = if FF_REGIME_STAKE {
                                        let true_p = edge.score_implied_pct / 100.0;
                                        let regime = classify_regime(true_p, azuro_odds);
                                        let regime_stake = compute_regime_stake(true_p, azuro_odds, current_bankroll);
                                        info!("📈 REGIME SCORE STAKE: {} true_p={:.1}% regime={} kelly_stake=${:.2} (old: base=${:.2}×{:.2}×{:.2}=${:.2})",
                                            edge.match_key, true_p * 100.0, regime, regime_stake,
                                            base_stake, sport_multiplier, score_stake_mult,
                                            base_stake * sport_multiplier * score_stake_mult);
                                        if regime_stake > 0.0 {
                                            regime_stake
                                        } else {
                                            0.0 // NoBet regime
                                        }
                                    } else {
                                        base_stake * sport_multiplier * score_stake_mult
                                    };
                                    info!("📈 SCORE STAKE: {} edge={:.1}% sport={} odds={:.2} raw=${:.2}",
                                        edge.match_key, edge.edge_pct, sport, azuro_odds, raw_stake);

                                    // === EXPOSURE CAPS + STAKE TRIMMER ===
                                    let cond_exp = scoped_cond_key.as_ref()
                                        .and_then(|key| condition_exposure.get(key))
                                        .copied()
                                        .unwrap_or(0.0);
                                    let match_exp = match_exposure.get(&base_match_key).copied().unwrap_or(0.0);
                                    let sport_exp = sport_exposure.get(sport).copied().unwrap_or(0.0);
                                    let daily_net_loss_for_cap = (daily_wagered - daily_returned).max(0.0);
                                    let cv_sm = edge.cv_stake_mult;
                                    let stake = trim_stake(raw_stake, current_bankroll, cond_exp, match_exp, daily_net_loss_for_cap,
                                        inflight_wagered_total, sport_exp, sport, cv_sm, start_of_day_bankroll, "score_edge", azuro_odds,
                                        daily_limit_override.unwrap_or(DAILY_LOSS_LIMIT_USD));
                                    if stake < 0.50 && raw_stake >= 0.50 {
                                        info!("🛡️ EXPOSURE CAP: {} stake trimmed from ${:.2} to $0 (bank=${:.0} cond_exp=${:.2} match_exp=${:.2} daily_loss=${:.2})",
                                            match_key_for_bet, raw_stake, current_bankroll, cond_exp, match_exp, daily_net_loss_for_cap);
                                    }

                                    let esports_identity_allowed = generic_esports_auto_bet_allowed(
                                        &edge.match_key,
                                        edge.resolved_sport.as_deref(),
                                        edge.esports_family,
                                        edge.esports_confidence,
                                        edge.esports_reason,
                                    );
                                    let esports_identity_blocked = !esports_identity_allowed;
                                    if edge.match_key.starts_with("esports::") {
                                        let gate_decision = if esports_identity_allowed {
                                            "promoted"
                                        } else {
                                            "blocked"
                                        };
                                        append_ledger_audit_event("ESPORTS_PROMOTION_GATE_AUDIT", &serde_json::json!({
                                            "match_key": edge.match_key,
                                            "market_key": edge.market_key,
                                            "decision": gate_decision,
                                            "resolved_sport": edge.resolved_sport,
                                            "esports_family": edge.esports_family,
                                            "sport_confidence": edge.esports_confidence,
                                            "sport_reason": edge.esports_reason,
                                            "edge_pct": edge.edge_pct,
                                            "odds": azuro_odds,
                                            "score1": edge.score1,
                                            "score2": edge.score2,
                                        }));
                                    }
                                    if edge.match_key.starts_with("esports::") && esports_identity_allowed {
                                        info!(
                                            "✅ ESPORTS PROMOTION GATE: {} promoted to family={} via {} — auto-bet eligible",
                                            edge.match_key,
                                            edge.esports_family.unwrap_or("unknown"),
                                            edge.esports_reason,
                                        );
                                    }
                                    let is_preferred_market = match preferred_market {
                                        "map_winner" => edge.market_key.starts_with("map") && edge.market_key.ends_with("_winner"),
                                        "set_winner" => edge.match_key.contains("::set"),
                                        "match_or_map" => true,
                                        "match_winner" => edge.market_key == "match_winner",
                                        _ => false,
                                    };

                                    // Check daily NET LOSS limit (settled losses minus claimed returns)
                                    // This prevents oracle lag from blocking us when we're actually in profit
                                    let daily_net_loss = (daily_wagered - daily_returned).max(0.0);
                                    let within_daily_limit = daily_net_loss < effective_daily_limit;

                                    // Sport-specific safety guard
                                    let football_goal_diff = if sport == "football" {
                                        Some((edge.score1 - edge.score2).abs())
                                    } else {
                                        None
                                    };
                                    let football_minute = if sport == "football" {
                                        football_minute_from_context(Some(&edge.live_status), edge.detailed_score.as_deref())
                                    } else {
                                        None
                                    };

                                    let sport_guard_ok = match sport {
                                        "tennis" => {
                                            // Only auto-bet when there's >= 1 set lead
                                            let set_diff = (edge.score1 - edge.score2).abs();
                                            set_diff >= 1
                                        }
                                        "football" => {
                                            // Football is minute-driven: 2 goals alone are not enough early.
                                            football_auto_bet_guard(
                                                football_goal_diff.unwrap_or_default(),
                                                football_minute,
                                            )
                                        }
                                        // ESPORTS guard: BLOCK match_winner auto-bet on map-level scores!
                                        // In Bo3 (CS2/LoL/Dota), score 1-0 means won your map pick → no real edge.
                                        // Only map_winner bets with ROUND-level edges are reliable.
                                        // Match_winner is only allowed on ROUND-level edges (score > 3).
                                        "cs2" | "esports" | "lol" | "dota-2" | "valorant" | "league-of-legends" => {
                                            let is_map_winner_bet = edge.market_key.starts_with("map") && edge.market_key.ends_with("_winner");
                                            if is_map_winner_bet {
                                                true // map_winner bets (round-level edges) are always OK
                                            } else {
                                                // match_winner: allow map-level only when edge is very strong and odds are safer
                                                let max_s = edge.score1.max(edge.score2);
                                                if max_s <= 3 {
                                                    let allow_maplevel_match = edge.edge_pct >= ESPORTS_MAPLEVEL_MATCH_EDGE_MIN_PCT
                                                        && azuro_odds <= ESPORTS_MAPLEVEL_MATCH_MAX_ODDS;
                                                    if !allow_maplevel_match {
                                                        info!("🛡️ ESPORTS GUARD: {} score {}-{} is MAP-LEVEL — blocking match_winner (needs edge≥{:.0}% and odds≤{:.2})",
                                                            edge.match_key, edge.score1, edge.score2,
                                                            ESPORTS_MAPLEVEL_MATCH_EDGE_MIN_PCT,
                                                            ESPORTS_MAPLEVEL_MATCH_MAX_ODDS);
                                                    } else {
                                                        info!("✅ ESPORTS MAP-LEVEL OVERRIDE: {} edge={:.1}% odds={:.2} — allowing match_winner auto-bet",
                                                            edge.match_key, edge.edge_pct, azuro_odds);
                                                    }
                                                    allow_maplevel_match
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

                                    // Safer score-edge corridor: fewer bets, but avoid drifting into high-variance odds regimes.
                                    let effective_min_odds = score_edge_min_odds(sport, &edge.market_key);
                                    let base_max_odds = score_edge_max_odds(&edge.market_key, sport, edge.cs2_map_confidence);
                                    let effective_max_odds = cs2_round_edge_max_odds_override(
                                        sport,
                                        &edge.market_key,
                                        edge.cs2_map_confidence,
                                        edge.edge_pct,
                                        edge.score1,
                                        edge.score2,
                                    ).unwrap_or(base_max_odds);

                                    // CRITICAL: Identical Azuro odds guard (score-edge path)
                                    // When oracle hasn't set real prices (e.g. basketball 1.84/1.84),
                                    // score-edge sees phantom 38%+ edge. Block auto-bet, allow alert.
                                    let azuro_odds_identical_se = (edge.azuro_w1 - edge.azuro_w2).abs() < 0.02;
                                    if azuro_odds_identical_se && edge.confidence == "HIGH" {
                                        info!("🛡️ IDENTICAL ODDS GUARD (score-edge): {} azuro={:.2}/{:.2} — oracle bug, blocking auto-bet (alert only)",
                                            edge.match_key, edge.azuro_w1, edge.azuro_w2);
                                    }

                                    // New guards: bankroll floor, pending cap, loss streak
                                    let bankroll_ok = current_bankroll >= MIN_BANKROLL_USD;
                                    let pending_count = count_pending_slots(&active_bets);
                                    let pending_ok = pending_count < MAX_CONCURRENT_PENDING;
                                    let streak_ok = loss_streak_pause_until.map_or(true, |until| std::time::Instant::now() >= until);

                                    // CONDITION BLACKLIST: skip conditions that previously failed
                                    let condition_blacklisted = edge.condition_id.as_ref()
                                        .map(|cid| {
                                            let scoped_cid = scoped_condition_key(&base_match_key, cid);
                                            if let Some(bl_time) = blacklisted_conditions.get(&scoped_cid) {
                                                if bl_time.elapsed() < std::time::Duration::from_secs(CONDITION_BLACKLIST_TTL_SECS) {
                                                    info!("🚫 CONDITION BLACKLISTED (edge): {} — failed {}s ago, skipping",
                                                        cid, bl_time.elapsed().as_secs());
                                                    true
                                                } else {
                                                    blacklisted_conditions.remove(&scoped_cid);
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        })
                                        .unwrap_or(false);

                                    // MATCH-LEVEL BLACKLIST: skip entire match when conditions keep dying
                                    let match_blacklisted = {
                                        if let Some(bl_time) = blacklisted_matches.get(&edge.match_key) {
                                            if bl_time.elapsed() < std::time::Duration::from_secs(MATCH_BLACKLIST_TTL_SECS) {
                                                info!("🚫 MATCH BLACKLISTED (edge): {} — failed {}s ago, skipping",
                                                    &edge.match_key, bl_time.elapsed().as_secs());
                                                true
                                            } else {
                                                blacklisted_matches.remove(&edge.match_key);
                                                false
                                            }
                                        } else {
                                            false
                                        }
                                    };

                                    let should_auto_bet = AUTO_BET_ENABLED
                                        && dashboard_autobet_enabled
                                        && (dashboard_sport_focus.contains(&"all".to_string()) || dashboard_sport_focus.iter().any(|s| s == sport))
                                        && sport_auto_allowed
                                        && sport_live_enabled
                                        && is_preferred_market
                                        && sport_guard_ok
                                        && within_daily_limit
                                        && !safe_mode
                                        && edge.confidence == "HIGH"
                                        && edge.edge_pct >= sport_min_edge
                                        && azuro_odds >= effective_min_odds
                                        && azuro_odds <= effective_max_odds
                                        && !azuro_odds_identical_se // BUG FIX: block phantom edge from identical oracle odds
                                        && anomaly.condition_id.is_some()
                                        && anomaly.outcome_id.is_some()
                                        && !esports_identity_blocked
                                        && !condition_blacklisted  // CONDITION BLACKLIST: skip dead conditions
                                        && !match_blacklisted      // MATCH BLACKLIST: skip dead matches
                                        && (!already_bet_this || rebet_ok) // RE-BET: allow if re-bet conditions met
                                        && stake >= 0.50 // EXPOSURE CAP: stake trimmer didn't zero it out
                                        && bankroll_ok   // MIN_BANKROLL guard
                                        && pending_ok    // MAX_CONCURRENT_PENDING guard
                                        && streak_ok;    // LOSS_STREAK pause guard

                                    if !bankroll_ok && edge.confidence == "HIGH" {
                                        info!("🛑 MIN BANKROLL: ${:.2} < ${:.2} — skipping auto-bet", current_bankroll, MIN_BANKROLL_USD);
                                    }
                                    if !dashboard_autobet_enabled && edge.confidence == "HIGH" {
                                        info!("📱 DASHBOARD: auto-bet DISABLED via dashboard toggle");
                                    }
                                    if !dashboard_sport_focus.contains(&"all".to_string()) && !dashboard_sport_focus.iter().any(|s| s == sport) && edge.confidence == "HIGH" {
                                        info!("📱 DASHBOARD: sport '{}' not in focus {:?} — skipping auto-bet", sport, dashboard_sport_focus);
                                    }
                                    if !pending_ok && edge.confidence == "HIGH" {
                                        info!("🛑 PENDING CAP: {} >= {} — skipping auto-bet", pending_count, MAX_CONCURRENT_PENDING);
                                    }
                                    if !streak_ok && edge.confidence == "HIGH" {
                                        info!("🛑 LOSS STREAK PAUSE: {} consecutive losses — cooling down", consecutive_losses);
                                    }
                                    if !sport_auto_allowed && edge.confidence == "HIGH" {
                                        info!("📢 {} ALERT ONLY (auto-bet disabled for {})", edge.match_key, sport);
                                    }
                                    if sport_dry_run_enabled && !sport_live_enabled && edge.confidence == "HIGH" {
                                        info!("🧪 SPORT DRY-RUN: {} ({}) passed candidate stage but live auto-bet flag is OFF", edge.match_key, sport);
                                    }
                                    if !within_daily_limit {
                                        info!("🛑 DAILY LOSS LIMIT: net losses={:.2} >= {:.2} (effective), skipping auto-bet", daily_net_loss, effective_daily_limit);
                                    }
                                    if !is_preferred_market && sport_auto_allowed {
                                        info!("🛡️ MARKET GUARD: {} needs {} but got {} — alert only", edge.match_key, preferred_market, edge.market_key);
                                    }
                                    if !sport_guard_ok && sport_auto_allowed && edge.confidence == "HIGH" {
                                        if sport == "football" {
                                            info!("🛡️ FOOTBALL GUARD: {} score {}-{} minute={:?} goal_diff={} doesn't meet containment threshold — alert only",
                                                edge.match_key,
                                                edge.score1,
                                                edge.score2,
                                                football_minute,
                                                football_goal_diff.unwrap_or_default());
                                        } else {
                                            info!("🛡️ SPORT GUARD: {} ({}): score {}-{} doesn't meet safety threshold — alert only",
                                                edge.match_key, sport, edge.score1, edge.score2);
                                        }
                                    }
                                    if esports_identity_blocked {
                                        info!(
                                            "🛡️ ESPORTS IDENTITY GUARD: {} family={} confidence={} reason={} — auto-bet blocked",
                                            edge.match_key,
                                            edge.esports_family.unwrap_or("unknown"),
                                            edge.esports_confidence,
                                            edge.esports_reason,
                                        );
                                    }
                                    let block_reason_codes = blocked_score_edge_reason_codes(
                                        AUTO_BET_ENABLED,
                                        sport_auto_allowed,
                                        sport_live_enabled,
                                        is_preferred_market,
                                        sport_guard_ok,
                                        within_daily_limit,
                                        safe_mode,
                                        edge.confidence == "HIGH",
                                        edge.edge_pct,
                                        sport_min_edge,
                                        azuro_odds,
                                        effective_min_odds,
                                        effective_max_odds,
                                        anomaly.condition_id.is_some(),
                                        anomaly.outcome_id.is_some(),
                                        esports_identity_blocked,
                                        condition_blacklisted,
                                        match_blacklisted,
                                        already_bet_this,
                                        rebet_ok,
                                        stake,
                                        bankroll_ok,
                                        pending_ok,
                                        streak_ok,
                                    );
                                    let auditable_esports = should_audit_esports_score_decision(
                                        &edge.match_key,
                                        edge.esports_family,
                                        edge.esports_confidence,
                                    );
                                    if (sport == "football" || auditable_esports)
                                        && !should_auto_bet
                                        && edge.confidence == "HIGH"
                                        && edge.edge_pct >= sport_min_edge
                                    {
                                        let audit_event = if sport == "football" {
                                            "FOOTBALL_DECISION_AUDIT"
                                        } else {
                                            "ESPORTS_SCORE_DECISION_AUDIT"
                                        };
                                        let joined_reasons = if block_reason_codes.is_empty() {
                                            "UnknownBlock".to_string()
                                        } else {
                                            block_reason_codes.join("|")
                                        };
                                        if sport == "football" {
                                            info!(
                                                "⚽ FOOTBALL DECISION AUDIT: {} edge={:.1}% odds={:.2} minute={:?} goal_diff={} blocked_by={}",
                                                edge.match_key,
                                                edge.edge_pct,
                                                azuro_odds,
                                                football_minute,
                                                football_goal_diff.unwrap_or_default(),
                                                joined_reasons,
                                            );
                                        } else {
                                            info!(
                                                "🎮 ESPORTS DECISION AUDIT: {} sport={} edge={:.1}% odds={:.2} blocked_by={}",
                                                edge.match_key,
                                                sport,
                                                edge.edge_pct,
                                                azuro_odds,
                                                joined_reasons,
                                            );
                                        }
                                        ledger_write(audit_event, &serde_json::json!({
                                            "alert_id": aid,
                                            "match_key": edge.match_key,
                                            "match_prefix": match_prefix_from_match_key(&edge.match_key),
                                            "market_key": edge.market_key,
                                            "path": "edge",
                                            "decision": "blocked_candidate",
                                            "reason_code": joined_reasons,
                                            "reason_codes": block_reason_codes,
                                            "original_sport": sport_raw,
                                            "resolved_sport": sport,
                                            "esports_family": edge.esports_family,
                                            "sport_confidence": edge.esports_confidence,
                                            "sport_reason": edge.esports_reason,
                                            "score1": edge.score1,
                                            "score2": edge.score2,
                                            "goal_diff": football_goal_diff.unwrap_or_default(),
                                            "minute": football_minute,
                                            "live_status": edge.live_status,
                                            "detailed_score": edge.detailed_score,
                                            "edge_pct": edge.edge_pct,
                                            "requested_odds": azuro_odds,
                                            "stake": stake,
                                            "condition_id": anomaly.condition_id,
                                            "outcome_id": anomaly.outcome_id,
                                            "guard_ok": sport_guard_ok,
                                            "market_ok": is_preferred_market,
                                            "daily_ok": within_daily_limit,
                                            "safe_mode": safe_mode,
                                            "already_bet": already_bet_this,
                                            "rebet_ok": rebet_ok,
                                            "bankroll_ok": bankroll_ok,
                                            "pending_ok": pending_ok,
                                            "pending_count": pending_count,
                                            "inflight_total": inflight_wagered_total,
                                            "condition_exposure": cond_exp,
                                            "match_exposure": match_exp,
                                            "sport_exposure": sport_exp,
                                            "daily_net_loss": daily_net_loss_for_cap,
                                            "streak_ok": streak_ok,
                                        }));
                                    }
                                    // DEBUG: log ALL reasons when auto-bet is blocked but edge is high
                                    if !should_auto_bet && edge.confidence == "HIGH" && edge.edge_pct >= 10.0 {
                                        info!("🔍 AUTO-BET BLOCKED for {} edge={:.1}%: enabled={} sport_ok={} market_ok={} guard_ok={} daily_ok={} safe_mode={} conf={} min_edge={:.1} odds={:.2} min={:.2} max={:.2} cond={} out={} dedup={} esports_block={}",
                                            edge.match_key, edge.edge_pct,
                                            AUTO_BET_ENABLED, sport_auto_allowed, is_preferred_market, sport_guard_ok,
                                            within_daily_limit, safe_mode, edge.confidence, sport_min_edge,
                                            azuro_odds, effective_min_odds, effective_max_odds,
                                            anomaly.condition_id.is_some(), anomaly.outcome_id.is_some(),
                                            already_bet_this, esports_identity_blocked);
                                    }

                                    let mut score_alert_sent = false;

                                    if should_auto_bet {
                                        // AUTO-BET with sport-specific stake (set above)
                                        let mut condition_id = anomaly.condition_id.as_ref().unwrap().clone();
                                        let mut outcome_id = anomaly.outcome_id.as_ref().unwrap().clone();

                                        // Condition freshness: how stale is our last sighting? (GQL baseline)
                                        let condition_age_ms = condition_last_seen.get(&condition_id)
                                            .map(|ts| ts.elapsed().as_millis() as u64)
                                            .unwrap_or(999999);
                                        let condition_max_age_ms = condition_max_age_limit_ms(
                                            anomaly.chain.as_deref(),
                                            &edge.azuro_bookmaker,
                                        );

                                        // === PRE-FLIGHT GATE: WS-first with GQL fallback ===
                                        // Priority: WS real-time state > GQL poll staleness
                                        let mut ws_result = {
                                            let cache_r = ws_condition_cache.read().await;
                                            ws_gate_check(&cache_r, &condition_id, ws_state_gate_enabled)
                                        };

                                        // WS subscription race: when a condition appears for the first time,
                                        // the cache can be empty for a short window. Probe once to avoid
                                        // falling back to GQL (which correlates poorly with fast state flips).
                                        if ws_state_gate_enabled {
                                            if matches!(ws_result, WsGateResult::NoData | WsGateResult::Stale { .. }) {
                                                let _ = ws_sub_tx.try_send(vec![condition_id.clone()]);
                                                // SPEED-FIRST: pokud je GQL fresh, nečekáme vůbec (minimalizace odds slippage).
                                                // Pokud je GQL stale, dáme krátký probe, ať WS stihne dorazit.
                                                if condition_age_ms > condition_max_age_ms {
                                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                                    ws_result = {
                                                        let cache_r = ws_condition_cache.read().await;
                                                        ws_gate_check(&cache_r, &condition_id, ws_state_gate_enabled)
                                                    };
                                                }
                                            }
                                        }

                                        let gate_blocked = match &ws_result {
                                            WsGateResult::NotActive { state, age_ms } => {
                                                warn!("🚫 WS-GATE #{}: condition {} state={} (ws_age={}ms) — DROP",
                                                    aid, &condition_id, state, age_ms);
                                                // CONDITION BLACKLIST: WS says not active → blacklist
                                                blacklisted_conditions.insert(
                                                    scoped_condition_key(&base_match_key, &condition_id),
                                                    std::time::Instant::now(),
                                                );
                                                ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                    "alert_id": aid, "match_key": match_key_for_bet,
                                                    "condition_id": condition_id, "outcome_id": outcome_id,
                                                    "error": format!("WS gate: condition state={}", state),
                                                    "reason_code": "WsStateNotActive",
                                                    "is_condition_state_reject": true,
                                                    "ws_state": state, "ws_age_ms": age_ms,
                                                    "condition_age_ms": condition_age_ms,
                                                    "requested_odds": azuro_odds,
                                                    "stake": stake, "path": "edge",
                                                    "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                }));
                                                log_event("WS_GATE_DROP", &serde_json::json!({
                                                    "alert_id": aid, "condition_id": condition_id,
                                                    "ws_state": state, "ws_age_ms": age_ms,
                                                    "gql_age_ms": condition_age_ms, "reason": "WsStateNotActive",
                                                }));
                                                ws_gate_not_active_count += 1;
                                                true
                                            }
                                            WsGateResult::Active { age_ms } => {
                                                debug!("✅ WS-GATE #{}: condition {} Active (ws_age={}ms)", aid, &condition_id, age_ms);
                                                ws_gate_active_count += 1;
                                                false // proceed!
                                            }
                                            WsGateResult::Stale { age_ms } => {
                                                ws_gate_stale_fallback_count += 1;
                                                // WS data stale → fallback to GQL age check
                                                if condition_age_ms > condition_max_age_ms {
                                                    warn!("🚫 PRE-FLIGHT #{}: WsStale({}ms)+GqlStale({}ms) — dropping",
                                                        aid, age_ms, condition_age_ms);
                                                    ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                        "alert_id": aid, "match_key": match_key_for_bet,
                                                        "condition_id": condition_id, "outcome_id": outcome_id,
                                                        "error": format!("WS stale ({}ms) + GQL stale ({}ms)", age_ms, condition_age_ms),
                                                        "reason_code": "WsStaleGqlStale",
                                                        "is_condition_state_reject": true,
                                                        "ws_age_ms": age_ms,
                                                        "condition_age_ms": condition_age_ms,
                                                        "requested_odds": azuro_odds,
                                                        "stake": stake, "path": "edge",
                                                        "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                    }));
                                                    true
                                                } else {
                                                    info!("⚡ WS-GATE #{}: WsStale({}ms) but GQL fresh({}ms) — proceeding",
                                                        aid, age_ms, condition_age_ms);
                                                    false
                                                }
                                            }
                                            WsGateResult::NoData => {
                                                ws_gate_nodata_fallback_count += 1;
                                                // No WS data → fallback to GQL age check
                                                if condition_age_ms > condition_max_age_ms {
                                                    ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                        "alert_id": aid, "match_key": match_key_for_bet,
                                                        "condition_id": condition_id, "outcome_id": outcome_id,
                                                        "error": format!("WS no data + GQL stale ({}ms)", condition_age_ms),
                                                        "reason_code": "WsNoDataGqlStale",
                                                        "is_condition_state_reject": true,
                                                        "condition_age_ms": condition_age_ms,
                                                        "requested_odds": azuro_odds,
                                                        "stake": stake, "path": "edge",
                                                        "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                    }));
                                                    true
                                                } else {
                                                    info!("⚡ WS-GATE #{}: WsNoData but GQL fresh({}ms) — proceeding",
                                                        aid, condition_age_ms);
                                                    false
                                                }
                                            }
                                            WsGateResult::Disabled => {
                                                // WS gate disabled → fallback to GQL gate
                                                if condition_age_ms > condition_max_age_ms {
                                                    warn!("🚫 PRE-FLIGHT GATE #{}: condition {} [WsDisabled->GqlStale] gql_age={}ms > {}ms — dropping",
                                                        aid, &condition_id, condition_age_ms, condition_max_age_ms);
                                                    ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                        "alert_id": aid, "match_key": match_key_for_bet,
                                                        "condition_id": condition_id, "outcome_id": outcome_id,
                                                        "error": "pre-flight gate: WsDisabled->GqlStale",
                                                        "reason_code": "PreFlightStale",
                                                        "is_condition_state_reject": true,
                                                        "condition_age_ms": condition_age_ms,
                                                        "requested_odds": azuro_odds,
                                                        "stake": stake, "path": "edge",
                                                        "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                    }));
                                                    log_event("PREFLIGHT_GATE", &serde_json::json!({
                                                        "alert_id": aid, "condition_id": condition_id,
                                                        "condition_age_ms": condition_age_ms, "reason": "WsDisabled->GqlStale",
                                                    }));
                                                    true
                                                } else {
                                                    false
                                                }
                                            }
                                        };
                                        if gate_blocked {
                                            // Don't mark inflight — we never sent it
                                            // Continue to normal alert flow (no auto-bet)
                                        } else {

                                        // RACE CONDITION FIX: mark in-flight BEFORE sending to executor
                                        if let Some(key) = scoped_cond_key.as_ref() {
                                            inflight_conditions.insert(key.clone());
                                        }
                                        inflight_conditions.insert(match_key_for_bet.clone());

                                        info!("🤖 AUTO-BET #{}: {} @ {:.2} ${:.2} edge={:.1}%",
                                            aid, leading_team, azuro_odds, stake, edge.edge_pct);

                                        // Place the bet — with retry on "condition not active"
                                        let decision_instant = std::time::Instant::now();
                                        let decision_ts = Utc::now();
                                        let amount_raw = (stake * 1e6) as u64;

                                        // Retry loop: Azuro pauses conditions during score events
                                        // (set/game point in tennis, goal in football). We retry twice
                                        // after 5s each in case the condition re-activates.
                                        let max_retries = if sport == "football" { 0 } else { AUTO_BET_RETRY_MAX };
                                        let mut attempt = 0;
                                        let mut minodds_fallback_applied = false;
                                        let mut bet_success = false;
                                        loop {
                                        let mut min_odds_factor = min_odds_factor_with_fallback(&match_key_for_bet, minodds_fallback_applied);
                                        // HIGH EDGE OVERRIDE: when score-edge is >40% and GQL odds are stale,
                                        // accept any odds >= AUTO_BET_MIN_ODDS (1.70). The edge is so large that
                                        // even with massive slippage the bet remains +EV.
                                        // Factor floor = max(AUTO_BET_MIN_ODDS / azuro_odds, 0.50)
                                        if minodds_fallback_applied && edge.edge_pct >= 40.0 && azuro_odds > 2.0 {
                                            let ev_floor = (AUTO_BET_MIN_ODDS / azuro_odds).max(0.50);
                                            if ev_floor < min_odds_factor {
                                                info!("🎯 HIGH-EDGE MIN_ODDS OVERRIDE: edge={:.1}% odds={:.2} → floor {:.3} (was {:.3})",
                                                    edge.edge_pct, azuro_odds, ev_floor, min_odds_factor);
                                                min_odds_factor = ev_floor;
                                            }
                                        }
                                        let (min_odds, min_odds_display) = compute_min_odds_raw(azuro_odds, min_odds_factor);
                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "requestedOdds": azuro_odds,
                                            "matchKey": match_key_for_bet,
                                            "team1": edge.team1,
                                            "team2": edge.team2,
                                            "valueTeam": leading_team,
                                        });
                                        // Signal TTL check — abort if decision is stale
                                        if decision_instant.elapsed() > std::time::Duration::from_secs(SIGNAL_TTL_SECS) {
                                            warn!("⏰ AUTO-BET #{}: Signal TTL expired ({}ms elapsed) — aborting stale bet",
                                                aid, decision_instant.elapsed().as_millis());
                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                inflight_conditions.remove(key);
                                            }
                                            inflight_conditions.remove(&match_key_for_bet);
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "⏰ <b>AUTO-BET #{} TTL EXPIRED</b>\n\
                                                     🏷️ <b>{}</b> | path: <b>edge</b>\n\
                                                     🧩 match: <b>{}</b>\n\
                                                     ⏱ elapsed: {}ms",
                                                    aid,
                                                    match_key_for_bet.split("::").next().unwrap_or("?").to_uppercase(),
                                                    match_key_for_bet,
                                                    decision_instant.elapsed().as_millis()
                                                )
                                            ).await;
                                            break;
                                        }
                                        // Pipeline budget check — abort if processing took too long
                                        let elapsed_pipeline = decision_instant.elapsed().as_millis() as u64;
                                        if elapsed_pipeline > PIPELINE_BUDGET_MS {
                                            warn!("⏰ AUTO-BET #{}: Pipeline budget exceeded ({}ms > {}ms) — dropping",
                                                aid, elapsed_pipeline, PIPELINE_BUDGET_MS);
                                            ledger_write("BET_FAILED", &serde_json::json!({
                                                "alert_id": aid, "match_key": &match_key_for_bet,
                                                "match_prefix": match_prefix_from_match_key(&match_key_for_bet),
                                                "market_key": &edge.market_key,
                                                "condition_id": &cond_id_str, "outcome_id": &outcome_id,
                                                "error": "pipeline budget exceeded",
                                                "reason_code": "PipelineBudgetExceeded",
                                                "is_condition_state_reject": false,
                                                "condition_age_ms": condition_age_ms,
                                                "pipeline_ms": elapsed_pipeline,
                                                "resolved_sport": sport,
                                                "leading_side": edge.leading_side,
                                                "score1": edge.score1,
                                                "score2": edge.score2,
                                                "edge_pct": edge.edge_pct,
                                                "score_implied_pct": edge.score_implied_pct,
                                                "azuro_implied_pct": edge.azuro_implied_pct,
                                                "requested_odds": azuro_odds, "stake": stake,
                                                "path": "score_edge", "retries": attempt,
                                                "rtt_ms": 0,
                                            }));
                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                inflight_conditions.remove(key);
                                            }
                                            inflight_conditions.remove(&match_key_for_bet);
                                            break;
                                        }
                                        let send_ts = Utc::now();
                                        let send_instant = std::time::Instant::now();
                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                let response_ts = Utc::now();
                                                let rtt_ms = send_instant.elapsed().as_millis();
                                                let pipeline_ms = decision_instant.elapsed().as_millis();
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
                                                            // Permanent dead condition (map3 finished, live unavailable, etc.)
                                                            // These should NEVER be retried but MUST be blacklisted immediately.
                                                            let is_condition_dead = err_lower.contains("not available")
                                                                || err_lower.contains("live is not");
                                                            let is_fatal = err_lower.contains("insufficient")
                                                                || err_lower.contains("allowance")
                                                                || err_lower.contains("revert")
                                                                || err_lower.contains("nonce");
                                                            if is_condition_paused && !is_condition_dead && !is_fatal && attempt < max_retries {
                                                                if let Some((new_condition_id, new_outcome_id)) = remap_execution_ids_from_state(
                                                                    &state,
                                                                    &match_key_for_bet,
                                                                    &edge.team1,
                                                                    &edge.team2,
                                                                    edge.leading_side,
                                                                ) {
                                                                    if new_condition_id != condition_id || new_outcome_id != outcome_id {
                                                                        info!("🔁 AUTO-BET #{} remap retry: cond {}→{} out {}→{}",
                                                                            aid, condition_id, new_condition_id, outcome_id, new_outcome_id);
                                                                        if let Some(key) = scoped_cond_key.as_ref() {
                                                                            inflight_conditions.remove(key);
                                                                        }
                                                                        condition_id = new_condition_id;
                                                                        outcome_id = new_outcome_id;
                                                                        cond_id_str = condition_id.clone();
                                                                        scoped_cond_key = Some(scoped_condition_key(&base_match_key, &cond_id_str));
                                                                        if let Some(key) = scoped_cond_key.as_ref() {
                                                                            inflight_conditions.insert(key.clone());
                                                                        }
                                                                        attempt += 1;
                                                                        tokio::time::sleep(std::time::Duration::from_millis(REMAP_RETRY_DELAY_MS)).await;
                                                                        continue;
                                                                    }
                                                                }
                                                                let too_stale_for_retry = condition_age_ms > RETRY_CONDITION_MAX_AGE_MS;
                                                                let too_late_for_retry = decision_instant.elapsed().as_millis() as u64 > (PIPELINE_BUDGET_MS / 2);
                                                                if too_stale_for_retry || too_late_for_retry {
                                                                    info!("⏭️ AUTO-BET #{} retry skipped (stale={} late={} age={}ms pipeline={}ms)",
                                                                        aid,
                                                                        too_stale_for_retry,
                                                                        too_late_for_retry,
                                                                        condition_age_ms,
                                                                        decision_instant.elapsed().as_millis());
                                                                } else {
                                                                    attempt += 1;
                                                                    let base_delay = AUTO_BET_RETRY_DELAYS_MS.get(attempt.saturating_sub(1)).copied().unwrap_or(500);
                                                                    let jitter = ((aid as u64).wrapping_mul(7).wrapping_add(attempt as u64 * 13)) % (base_delay / 2 + 1);
                                                                    let delay_ms = base_delay + jitter;
                                                                    info!("🔄 AUTO-BET #{} retry {}/{}: condition paused, waiting {}ms (base={}+jitter={})... ({})",
                                                                        aid, attempt, max_retries, delay_ms, base_delay, jitter, err);
                                                                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                                                    continue; // retry the loop
                                                                }
                                                            }
                                                            // Classify error for noise reduction
                                                            let is_minodds_reject = err_lower.contains("min odds") || err_lower.contains("minodds")
                                                                || err_lower.contains("real odds");
                                                            if is_minodds_reject && !minodds_fallback_applied && attempt < max_retries {
                                                                attempt += 1;
                                                                minodds_fallback_applied = true;
                                                                let base_delay = 30;
                                                                let jitter = ((aid as u64).wrapping_mul(11).wrapping_add(attempt as u64 * 17)) % 80;
                                                                let delay_ms = base_delay + jitter;
                                                                info!("🔁 AUTO-BET #{} min-odds fallback retry {}/{}: factor step -{:.2}, wait {}ms ({})",
                                                                    aid, attempt, max_retries, MIN_ODDS_FALLBACK_STEP, delay_ms, err);
                                                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                                                continue;
                                                            }
                                                            let is_dedup = err_lower.contains("dedup") || err_lower.contains("already bet");
                                                            let is_condition_state_reject = is_condition_paused || is_condition_dead;
                                                            let reason_code = if is_condition_state_reject { "ConditionNotRunning" }
                                                                else if is_minodds_reject { "MinOddsReject" }
                                                                else if is_dedup { "Dedup" }
                                                                else if is_fatal { "Fatal" }
                                                                else { "Unknown" };
                                                            // CONDITION BLACKLIST: add failed condition to blacklist
                                                            if is_condition_state_reject || is_minodds_reject {
                                                                blacklisted_conditions.insert(
                                                                    scoped_condition_key(&base_match_key, &condition_id),
                                                                    std::time::Instant::now(),
                                                                );
                                                                info!("🚫 BLACKLISTED condition {} (reason={}, TTL={}s)",
                                                                    &condition_id, reason_code, CONDITION_BLACKLIST_TTL_SECS);
                                                            }
                                                            // MATCH BLACKLIST: ConditionNotRunning → block entire match
                                                            if is_condition_state_reject {
                                                                blacklisted_matches.insert(match_key_for_bet.clone(), std::time::Instant::now());
                                                                info!("🚫 MATCH BLACKLISTED {} (ConditionNotRunning, TTL={}s)",
                                                                    &match_key_for_bet, MATCH_BLACKLIST_TTL_SECS);
                                                            }
                                                            error!("❌ AUTO-BET #{} FAILED: {} (cond={}, outcome={}, match={}, rtt={}ms, pipeline={}ms, requested_odds={:.4}, min_odds={:.4}, reason={})",
                                                                aid, err,
                                                                &condition_id,
                                                                &outcome_id,
                                                                match_key_for_bet,
                                                                rtt_ms, pipeline_ms,
                                                                azuro_odds, min_odds_display,
                                                                reason_code);
                                                            // === LEDGER: BET_FAILED (skip DEDUP — operational noise, ~84/day) ===
                                                            if !is_dedup {
                                                            ledger_write("BET_FAILED", &serde_json::json!({
                                                                "alert_id": aid, "match_key": match_key_for_bet,
                                                                "match_prefix": match_prefix_from_match_key(&match_key_for_bet),
                                                                "market_key": &edge.market_key,
                                                                "condition_id": condition_id, "outcome_id": outcome_id,
                                                                "error": err, "retries": attempt,
                                                                "requested_odds": azuro_odds, "min_odds": min_odds_display,
                                                                "stake": stake, "path": "edge",
                                                                "resolved_sport": sport,
                                                                "leading_side": edge.leading_side,
                                                                "score1": edge.score1,
                                                                "score2": edge.score2,
                                                                "edge_pct": edge.edge_pct,
                                                                "score_implied_pct": edge.score_implied_pct,
                                                                "azuro_implied_pct": edge.azuro_implied_pct,
                                                                "decision_ts": decision_ts.to_rfc3339(),
                                                                "send_ts": send_ts.to_rfc3339(),
                                                                "response_ts": response_ts.to_rfc3339(),
                                                                "rtt_ms": rtt_ms as u64,
                                                                "pipeline_ms": pipeline_ms as u64,
                                                                "is_minodds_reject": is_minodds_reject,
                                                                "is_dedup": is_dedup,
                                                                "is_condition_state_reject": is_condition_state_reject,
                                                                "reason_code": reason_code,
                                                                "condition_age_ms": condition_age_ms,
                                                            }));
                                                            }
                                                            // Don't spam TG for dedup (409) — it's normal operational behavior
                                                            if !is_dedup {
                                                                let _ = tg_send_message(&client, &token, chat_id,
                                                                    &format_auto_bet_failed_message(
                                                                        aid,
                                                                        "edge",
                                                                        &match_key_for_bet,
                                                                        &condition_id,
                                                                        reason_code,
                                                                        err,
                                                                        attempt,
                                                                        rtt_ms,
                                                                        pipeline_ms,
                                                                        azuro_odds,
                                                                        min_odds_display,
                                                                    )
                                                                ).await;
                                                            } else {
                                                                info!("🔇 Suppressed TG for dedup rejection #{}: {}", aid, err);
                                                            }
                                                            // Remove from inflight so we can retry on next edge
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                inflight_conditions.remove(key);
                                                            }
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break; // exit retry loop
                                                        } else if is_rejected {
                                                            error!("❌ AUTO-BET #{} REJECTED: state={} (cond={}, match={})",
                                                                aid, br.state.as_deref().unwrap_or("?"),
                                                                &condition_id,
                                                                match_key_for_bet);
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format_auto_bet_rejected_message(
                                                                    aid,
                                                                    "edge",
                                                                    &match_key_for_bet,
                                                                    &condition_id,
                                                                    br.state.as_deref().unwrap_or("?"),
                                                                )
                                                            ).await;
                                                            // === LEDGER: REJECTED ===
                                                            ledger_write("REJECTED", &serde_json::json!({
                                                                "alert_id": aid, "match_key": match_key_for_bet,
                                                                "condition_id": condition_id,
                                                                "state": br.state, "path": "edge"
                                                            }));
                                                            // Remove from inflight so we can retry on next edge
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                inflight_conditions.remove(key);
                                                            }
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break; // exit retry loop
                                                        } else {
                                                            auto_bet_count += 1;
                                                            daily_wagered += stake;
                                                            // Persist daily P&L
                                                            {
                                                                let today = Utc::now().format("%Y-%m-%d").to_string();
                                                                let _ = std::fs::write(bet_count_path, format!("{}|{}", today, auto_bet_count));
                                                                let _ = std::fs::write("data/daily_pnl.json",
                                                                    serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                                                            }
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let bet_state = br.state.as_deref().unwrap_or("?");
                                                            let token_id_opt = sanitize_token_id(br.token_id.clone());
                                                            let graph_bet_id_opt = br.graph_bet_id.clone();
                                                            let accepted_odds = br.accepted_odds.unwrap_or(azuro_odds);

                                                            // === DEDUP: record bet to prevent duplicates ===
                                                            already_bet_matches.insert(match_key_for_bet.clone());
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                already_bet_conditions.insert(key.clone());
                                                            }
                                                            // BUG #1 FIX: Also record base match key
                                                            already_bet_base_matches.insert(base_match_key.clone());
                                                            // Track map_winner placements for sibling-map dedup logic
                                                            if match_key_for_bet != base_match_key {
                                                                already_bet_map_winners.insert(base_match_key.clone());
                                                            }

                                                            // === EXPOSURE TRACKING: update condition + match + sport + inflight ===
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                *condition_exposure.entry(key.clone()).or_insert(0.0) += stake;
                                                            }
                                                            *match_exposure.entry(base_match_key.clone()).or_insert(0.0) += stake;
                                                            *sport_exposure.entry(sport.to_string()).or_insert(0.0) += stake;
                                                            inflight_wagered_total += stake;

                                                            // === RE-BET TRACKING: update or create state ===
                                                            if let Some(rb) = scoped_cond_key.as_ref().and_then(|key| rebet_tracker.get_mut(key)) {
                                                                rb.bet_count += 1;
                                                                rb.highest_tier = edge.confidence.to_string();
                                                                rb.last_edge_pct = edge.edge_pct;
                                                                rb.last_bet_at = Utc::now();
                                                                rb.total_wagered += stake;
                                                                info!("🔄 RE-BET #{}: {} total bets on cond={}, total wagered=${:.2}",
                                                                    rb.bet_count, match_key_for_bet, cond_id_str, rb.total_wagered);
                                                            } else {
                                                                rebet_tracker.insert(scoped_condition_key(&base_match_key, &cond_id_str),
                                                                    ReBetState::new(edge.confidence, edge.edge_pct, stake));
                                                            }

                                                            // Remove from inflight (bet is now in persistent dedup)
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                inflight_conditions.remove(key);
                                                            }
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            // Persist to file
                                                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                .create(true).append(true)
                                                                .open(bet_history_path) {
                                                                use std::io::Write;
                                                                let _ = writeln!(f, "{}|{}|{}|{}|{}",
                                                                    match_key_for_bet, cond_id_str,
                                                                    leading_team, accepted_odds, Utc::now().to_rfc3339());
                                                            }

                                                            let is_dry_run = bet_state == "DRY-RUN" || bet_id.starts_with("dry-");
                                                            if !is_dry_run {
                                                                active_bets.push(ActiveBet {
                                                                    alert_id: aid,
                                                                    bet_id: bet_id.to_string(),
                                                                    match_key: edge.match_key.clone(),
                                                                    market_key: edge.market_key.clone(),
                                                                    team1: edge.team1.clone(),
                                                                    team2: edge.team2.clone(),
                                                                    value_team: leading_team.to_string(),
                                                                    amount_usd: stake,
                                                                    odds: accepted_odds,
                                                                    placed_at: Utc::now().to_rfc3339(),
                                                                    condition_id: condition_id.clone(),
                                                                    outcome_id: outcome_id.clone(),
                                                                    graph_bet_id: graph_bet_id_opt.clone(),
                                                                    token_id: token_id_opt.clone(),
                                                                    path: "score_edge".to_string(),
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
                                                                        leading_team, stake, accepted_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                                // === LEDGER: BET PLACED ===
                                                                ledger_write("PLACED", &serde_json::json!({
                                                                    "alert_id": aid, "bet_id": bet_id,
                                                                    "match_key": edge.match_key,
                                                                    "match_prefix": match_prefix_from_match_key(&edge.match_key),
                                                                    "market_key": edge.market_key,
                                                                    "team1": edge.team1, "team2": edge.team2,
                                                                    "value_team": leading_team,
                                                                    "amount_usd": stake, "odds": accepted_odds,
                                                                    "requested_odds": azuro_odds,
                                                                    "condition_id": condition_id,
                                                                    "outcome_id": outcome_id,
                                                                    "token_id": token_id_opt,
                                                                    "graph_bet_id": graph_bet_id_opt,
                                                                    "on_chain_state": bet_state,
                                                                    "path": "edge",
                                                                    "original_sport": sport_raw,
                                                                    "resolved_sport": sport,
                                                                    "esports_family": edge.esports_family,
                                                                    "sport_confidence": edge.esports_confidence,
                                                                    "sport_reason": edge.esports_reason,
                                                                    "leading_side": edge.leading_side,
                                                                    "score1": edge.score1,
                                                                    "score2": edge.score2,
                                                                    "score_implied_pct": edge.score_implied_pct,
                                                                    "azuro_implied_pct": edge.azuro_implied_pct,
                                                                    "edge_pct": edge.edge_pct,
                                                                    "cv_stake_mult": cv_sm,
                                                                    "decision_ts": decision_ts.to_rfc3339(),
                                                                    "send_ts": send_ts.to_rfc3339(),
                                                                    "response_ts": response_ts.to_rfc3339(),
                                                                    "rtt_ms": rtt_ms as u64,
                                                                    "pipeline_ms": pipeline_ms as u64,
                                                                    "min_odds": min_odds_display,
                                                                    "condition_age_ms": condition_age_ms,
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

                                                                // === LEDGER: ON-CHAIN ACCEPTED (immediate) ===
                                                                if bet_state == "Accepted" {
                                                                    ledger_write("ON_CHAIN_ACCEPTED", &serde_json::json!({
                                                                        "alert_id": aid,
                                                                        "match_key": edge.match_key,
                                                                        "match_prefix": match_prefix_from_match_key(&edge.match_key),
                                                                        "market_key": edge.market_key,
                                                                        "condition_id": condition_id,
                                                                        "outcome_id": outcome_id,
                                                                        "bet_id": bet_id,
                                                                        "on_chain_state": bet_state,
                                                                        "token_id": token_id_opt,
                                                                        "graph_bet_id": graph_bet_id_opt,
                                                                        "path": "edge",
                                                                        "original_sport": sport_raw,
                                                                        "resolved_sport": sport,
                                                                        "esports_family": edge.esports_family,
                                                                        "sport_confidence": edge.esports_confidence,
                                                                        "sport_reason": edge.esports_reason,
                                                                        "leading_side": edge.leading_side,
                                                                        "score1": edge.score1,
                                                                        "score2": edge.score2,
                                                                        "score_implied_pct": edge.score_implied_pct,
                                                                        "azuro_implied_pct": edge.azuro_implied_pct,
                                                                        "edge_pct": edge.edge_pct,
                                                                        "odds": accepted_odds,
                                                                        "requested_odds": azuro_odds,
                                                                        "stake": stake,
                                                                    }));
                                                                }
                                                            }

                                                            let result_msg = format_auto_bet_result_message(
                                                                aid,
                                                                "edge",
                                                                &match_key_for_bet,
                                                                leading_team,
                                                                accepted_odds,
                                                                stake,
                                                                bet_id,
                                                                bet_state,
                                                                auto_bet_count,
                                                                is_dry_run,
                                                            );
                                                            if let Err(e) = tg_send_message(&client, &token, chat_id, &result_msg).await {
                                                                error!("Failed to send auto-bet result alert: {}", e);
                                                            } else {
                                                                score_alert_sent = true;
                                                            }

                                                            // === UNIFIED FOLLOW-UP: Poll Created bets to detect async Rejected ===
                                                            if !is_dry_run && (bet_state == "Created" || bet_state == "Pending") {
                                                                let follow_client = client.clone();
                                                                let follow_token = token.clone();
                                                                let follow_executor = executor_url.clone();
                                                                let follow_bet_id = bet_id.to_string();
                                                                let follow_aid = aid;
                                                                let follow_team = leading_team.to_string();
                                                                let follow_chat = chat_id;
                                                                let follow_match_key = match_key_for_bet.clone();
                                                                let follow_market_key = edge.market_key.clone();
                                                                let follow_condition_id = condition_id.clone();
                                                                let follow_outcome_id = outcome_id.clone();
                                                                let follow_odds = azuro_odds;
                                                                let follow_stake = stake;
                                                                tokio::spawn(async move {
                                                                    tokio::time::sleep(Duration::from_secs(20)).await;
                                                                    if let Ok(resp) = follow_client.get(
                                                                        format!("{}/bet/{}", follow_executor, follow_bet_id)
                                                                    ).send().await {
                                                                        if let Ok(br) = resp.json::<serde_json::Value>().await {
                                                                            let final_state = br.get("state")
                                                                                .and_then(|v| v.as_str()).unwrap_or("?");
                                                                            let err_msg = br.get("errorMessage")
                                                                                .and_then(|v| v.as_str()).unwrap_or("");
                                                                            if final_state == "Rejected" || final_state == "Failed" || final_state == "Cancelled" {
                                                                                let alert = format!(
                                                                                    "❌ <b>AUTO-BET #{} REJECTED (follow-up)</b>\n\
                                                                                     path: <b>edge</b>\n\
                                                                                     💡 Pick: <b>{}</b>\n\
                                                                                     🧠 on-chain state: <b>{}</b>\n\
                                                                                     📝 {}",
                                                                                    follow_aid, follow_team, final_state, err_msg);
                                                                                let _ = tg_send_message(
                                                                                    &follow_client, &follow_token,
                                                                                    follow_chat, &alert).await;
                                                                                warn!("❌ AUTO-BET #{} FOLLOW-UP REJECTED: {} err={}",
                                                                                    follow_aid, follow_bet_id, err_msg);
                                                                                // LEDGER: write on-chain REJECTED event
                                                                                {
                                                                                    let entry = serde_json::json!({
                                                                                        "ts": chrono::Utc::now().to_rfc3339(),
                                                                                        "event": "ON_CHAIN_REJECTED",
                                                                                        "alert_id": follow_aid,
                                                                                        "match_key": follow_match_key,
                                                                                        "market_key": follow_market_key,
                                                                                        "condition_id": follow_condition_id,
                                                                                        "outcome_id": follow_outcome_id,
                                                                                        "bet_id": follow_bet_id,
                                                                                        "on_chain_state": final_state,
                                                                                        "error": err_msg,
                                                                                        "path": "edge",
                                                                                        "odds": follow_odds,
                                                                                        "stake": follow_stake,
                                                                                    });
                                                                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                                        .create(true).append(true).open("data/ledger.jsonl") {
                                                                                        use std::io::Write;
                                                                                        let _ = writeln!(f, "{}", entry);
                                                                                    }
                                                                                }
                                                                            } else if final_state == "Accepted" {
                                                                                let token_id = br.get("tokenId")
                                                                                    .and_then(|v| v.as_str()).unwrap_or("?");
                                                                                let alert = format!(
                                                                                    "✅ <b>AUTO-BET #{} CONFIRMED (follow-up)</b>\n\
                                                                                     path: <b>edge</b>\n\
                                                                                     💡 Pick: <b>{}</b>\n\
                                                                                     🧾 Bet ID: <code>{}</code>\n\
                                                                                     🧠 on-chain state: <b>{}</b>\n\
                                                                                     🪙 tokenId: <code>{}</code>",
                                                                                    follow_aid,
                                                                                    follow_team,
                                                                                    follow_bet_id,
                                                                                    final_state,
                                                                                    token_id,
                                                                                );
                                                                                let _ = tg_send_message(
                                                                                    &follow_client,
                                                                                    &follow_token,
                                                                                    follow_chat,
                                                                                    &alert,
                                                                                ).await;
                                                                                info!("✅ AUTO-BET #{} FOLLOW-UP CONFIRMED: state={} tokenId={}",
                                                                                    follow_aid, final_state, token_id);
                                                                                // LEDGER: write on-chain ACCEPTED event
                                                                                {
                                                                                    let entry = serde_json::json!({
                                                                                        "ts": chrono::Utc::now().to_rfc3339(),
                                                                                        "event": "ON_CHAIN_ACCEPTED",
                                                                                        "alert_id": follow_aid,
                                                                                        "match_key": follow_match_key,
                                                                                        "market_key": follow_market_key,
                                                                                        "condition_id": follow_condition_id,
                                                                                        "outcome_id": follow_outcome_id,
                                                                                        "bet_id": follow_bet_id,
                                                                                        "on_chain_state": final_state,
                                                                                        "token_id": token_id,
                                                                                        "path": "edge",
                                                                                        "odds": follow_odds,
                                                                                        "stake": follow_stake,
                                                                                        "path": "edge",
                                                                                    });
                                                                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                                        .create(true).append(true).open("data/ledger.jsonl") {
                                                                                        use std::io::Write;
                                                                                        let _ = writeln!(f, "{}", entry);
                                                                                    }
                                                                                }
                                                                            } else {
                                                                                info!("⏳ AUTO-BET #{} FOLLOW-UP: state={} (still pending)",
                                                                                    follow_aid, final_state);
                                                                            }
                                                                        }
                                                                    }
                                                                });
                                                            }
                                                            bet_success = true;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        // Remove from inflight on parse error too
                                                        inflight_conditions.remove(&cond_id_str);
                                                        inflight_conditions.remove(&match_key_for_bet);
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            &format!(
                                                                "❌ <b>AUTO-BET #{} RESPONSE ERROR</b>\n\
                                                                 path: <b>edge</b>\n\
                                                                 match: <b>{}</b>\n\
                                                                 {}",
                                                                aid,
                                                                match_key_for_bet,
                                                                e,
                                                            )
                                                        ).await;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                // Remove from inflight on executor error
                                                inflight_conditions.remove(&cond_id_str);
                                                inflight_conditions.remove(&match_key_for_bet);
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!(
                                                        "❌ <b>AUTO-BET #{} EXECUTOR OFFLINE</b>\n\
                                                         path: <b>edge</b>\n\
                                                         match: <b>{}</b>\n\
                                                         {}",
                                                        aid,
                                                        match_key_for_bet,
                                                        e,
                                                    )
                                                ).await;
                                            }
                                        }
                                        break; // exit retry loop (success, parse error, or executor offline)
                                        } // end retry loop

                                        } // end pre-flight gate else block
                                    } else if condition_blacklisted {
                                        info!("🔕 MANUAL ALERT SUPPRESSED (blacklisted condition): {}",
                                            edge.match_key);
                                    } else if match_blacklisted {
                                        info!("🔕 MANUAL ALERT SUPPRESSED (blacklisted match): {}",
                                            edge.match_key);
                                    } else if already_bet_this && !rebet_ok {
                                        info!("🔕 MANUAL ALERT SUPPRESSED (dedup): {} (base={})",
                                            edge.match_key, base_match_key);
                                    } else if !mute_manual_alerts {
                                        // Manual alert (MEDIUM confidence or auto-bet disabled)
                                        let now_alert = Utc::now();
                                        let too_soon = manual_offer_last_sent
                                            .get(&edge.match_key)
                                            .map(|ts| (now_alert - *ts).num_seconds() < MANUAL_MATCH_COOLDOWN_SECS)
                                            .unwrap_or(false);
                                        if too_soon {
                                            info!("⏱️ MANUAL ALERT THROTTLED: {} (cooldown={}s)",
                                                edge.match_key, MANUAL_MATCH_COOLDOWN_SECS);
                                        } else {
                                            let msg = format_score_edge_alert(edge, aid);
                                            match tg_send_message(&client, &token, chat_id, &msg).await {
                                                Ok(msg_id) => {
                                                    score_alert_sent = true;
                                                    msg_id_to_alert_id.insert(msg_id, aid);
                                                    manual_offer_last_sent.insert(edge.match_key.clone(), now_alert);
                                                }
                                                Err(e) => {
                                                    error!("Failed to send score edge alert: {}", e);
                                                }
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

                                    let mut cond_id_str = anomaly.condition_id.as_deref().unwrap_or("").to_string();
                                    let match_key_for_bet = anomaly.match_key.clone();
                                    let base_match_key = strip_map_winner_suffix(&match_key_for_bet);
                                    let mut scoped_cond_key = (!cond_id_str.is_empty())
                                        .then(|| scoped_condition_key(&base_match_key, &cond_id_str));
                                    // BUG FIX: Prevent match_winner↔map cross-exposure.
                                    // REFINED: map2↔map3 sibling bets allowed when previous was also map-level (match_cap guards).
                                    let is_candidate_map_winner_anom = match_key_for_bet != base_match_key;
                                    let base_already_bet_anom = already_bet_base_matches.contains(&base_match_key)
                                        && is_candidate_map_winner_anom
                                        && !already_bet_map_winners.contains(&base_match_key);
                                    let already_bet_this = base_already_bet_anom
                                        || scoped_cond_key.as_ref().is_some_and(|key| already_bet_conditions.contains(key))
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
                                    let anomaly_cond_exp = scoped_cond_key.as_ref()
                                        .and_then(|key| condition_exposure.get(key))
                                        .copied()
                                        .unwrap_or(0.0);
                                    let anomaly_match_exp = match_exposure.get(&base_match_key).copied().unwrap_or(0.0);
                                    let anomaly_sport = match_key_for_bet.split("::").next().unwrap_or("?");
                                    let anomaly_sport_exp = sport_exposure.get(anomaly_sport).copied().unwrap_or(0.0);
                                    let anomaly_daily_loss = (daily_wagered - daily_returned).max(0.0);

                                    // Regime-based stake: estimate true_p from anomaly score context
                                    let anomaly_raw_stake = if FF_REGIME_STAKE {
                                        // Try to estimate true_p from score data
                                        let anomaly_true_p = {
                                            let a_score = anomaly.live_score.as_deref().unwrap_or("?");
                                            let a_detail = anomaly.detailed_score.as_deref().unwrap_or("");
                                            let parts: Vec<&str> = a_score.split('-').collect();
                                            let (as1, as2) = if parts.len() == 2 {
                                                (parts[0].trim().parse::<i32>().unwrap_or(0),
                                                 parts[1].trim().parse::<i32>().unwrap_or(0))
                                            } else { (0, 0) };
                                            let a_leading = as1.max(as2);
                                            let a_losing = as1.min(as2);

                                            match anomaly_sport {
                                                "cs2" | "esports" | "valorant" | "dota-2" | "league-of-legends" | "lol" => {
                                                    // Try round→match prob with map context
                                                    if a_leading > 3 && FF_CS2_MATCH_FROM_ROUNDS {
                                                        let (ml, mm) = parse_esports_map_score(a_detail, as1, as2);
                                                        let (map_l, map_lo) = if ml > mm { (ml, mm) } else if mm > ml { (mm, ml) } else {
                                                            if let Some(ms) = parse_dust2_map_score(a_detail) {
                                                                if ms.0 > ms.1 { (ms.0, ms.1) } else { (ms.1, ms.0) }
                                                            } else { (0, 0) }
                                                        };
                                                        cs2_round_to_match_prob(map_l, map_lo, a_leading, a_losing)
                                                    } else if a_leading <= 3 && a_leading > a_losing {
                                                        // Map-level score
                                                        score_to_win_prob(a_leading, a_losing)
                                                    } else { None }
                                                }
                                                "football" => {
                                                    let minute = football_minute_from_context(None, anomaly.detailed_score.as_deref());
                                                    football_score_to_win_prob(a_leading, a_losing, minute)
                                                }
                                                "tennis" => tennis_score_to_win_prob(a_leading, a_losing),
                                                "basketball" => basketball_score_to_win_prob(a_leading, a_losing),
                                                _ => None,
                                            }
                                        };

                                        if let Some(tp) = anomaly_true_p {
                                            let regime = classify_regime(tp, azuro_odds);
                                            let stake = compute_regime_stake(tp, azuro_odds, current_bankroll);
                                            info!("📐 REGIME STAKE: {} odds={:.2} true_p={:.1}% regime={} → ${:.2}",
                                                anomaly.match_key, azuro_odds, tp * 100.0, regime, stake);
                                            stake
                                        } else {
                                            // No true_p → fallback to FalseFavorite test stake
                                            let fallback = anomaly_stake_for_odds(azuro_odds);
                                            info!("📐 FALLBACK STAKE (no true_p): {} odds={:.2} → ${:.2}",
                                                anomaly.match_key, azuro_odds, fallback);
                                            fallback
                                        }
                                    } else {
                                        let old_stake = anomaly_stake_for_odds(azuro_odds);
                                        info!("📐 ODDS-PROP STAKE: {} odds={:.2} → raw_stake=${:.2} (base={:.1} ref={:.2} scale={:.3})",
                                            anomaly.match_key, azuro_odds, old_stake,
                                            AUTO_BET_ODDS_ANOMALY_STAKE_BASE_USD, AUTO_BET_ODDS_ANOMALY_REF_ODDS,
                                            (AUTO_BET_ODDS_ANOMALY_REF_ODDS / azuro_odds).powf(1.5));
                                        old_stake
                                    };
                                    let anomaly_stake = trim_stake(anomaly_raw_stake, current_bankroll, anomaly_cond_exp, anomaly_match_exp, anomaly_daily_loss,
                                        inflight_wagered_total, anomaly_sport_exp, anomaly_sport, 1.0, start_of_day_bankroll, "anomaly", azuro_odds,
                                        daily_limit_override.unwrap_or(DAILY_LOSS_LIMIT_USD));

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
                                        // Football: ENABLED for anomaly path ONLY with goal_diff ≥ 2 (Phase 1, 2026-03-04)
                                        // Rationale: 0/2 football anomaly lost → but those were game-start bets.
                                        // With goal_diff ≥ 2, true_p is 0.85+ (very strong position).
                                        "football" => {
                                            if !FF_FOOTBALL_ANOMALY_GOALDIFF2 { false }
                                            else if let Some(ref score) = anomaly.live_score {
                                                let parts: Vec<&str> = score.split('-').collect();
                                                if parts.len() == 2 {
                                                    let (fs1, fs2) = (
                                                        parts[0].trim().parse::<i32>().unwrap_or(0),
                                                        parts[1].trim().parse::<i32>().unwrap_or(0),
                                                    );
                                                    let goal_diff = (fs1 - fs2).abs();
                                                    let football_ok = goal_diff >= 2;
                                                    if !football_ok {
                                                        info!("⚽ ANOMALY SPORT GUARD: {} score={:?} — football needs goal_diff≥2 for anomaly",
                                                            anomaly.match_key, anomaly.live_score);
                                                    }
                                                    football_ok
                                                } else { false }
                                            } else {
                                                false // no score → skip
                                            }
                                        }
                                        // Basketball: ENABLED — anomaly odds comparison is valid for mainstream leagues
                                        // Score-edge path also lives with point-diff model.
                                        "basketball" => true,
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
                                        // Esports anomaly DISABLED: production data shows -EV across ALL odds buckets
                                        // esports anomaly (all odds): WR 52.4%, breakeven 60.8%, margin -8.4pp, PnL -$14.55
                                        // Score-edge path remains active for esports with 30% edge threshold
                                        "cs2" | "esports" | "valorant" | "dota-2" | "league-of-legends" | "lol" => {
                                            info!("🎮 ANOMALY DISABLED for esports: {} — production data -EV, use score-edge path instead",
                                                anomaly.match_key);
                                            false
                                        }
                                        _ => true,
                                    };

                                    // Check daily NET LOSS limit for anomaly path too
                                    let anomaly_within_daily_limit = {
                                        let net = (daily_wagered - daily_returned).max(0.0);
                                        let (_, _, _, dl_frac, _) = get_exposure_caps(current_bankroll);
                                        let lim = daily_limit_override.unwrap_or_else(|| DAILY_LOSS_LIMIT_USD.min(current_bankroll * dl_frac));
                                        net < lim
                                    };

                                    // New guards for anomaly path too
                                    let anomaly_bankroll_ok = current_bankroll >= MIN_BANKROLL_USD;
                                    let anomaly_pending_count = count_pending_slots(&active_bets);
                                    let anomaly_pending_ok = anomaly_pending_count < MAX_CONCURRENT_PENDING;
                                    let anomaly_streak_ok = loss_streak_pause_until.map_or(true, |until| std::time::Instant::now() >= until);

                                    // CONDITION BLACKLIST: skip conditions that previously failed
                                    let anomaly_condition_blacklisted = anomaly.condition_id.as_ref()
                                        .map(|cid| {
                                            let scoped_cid = scoped_condition_key(&base_match_key, cid);
                                            if let Some(bl_time) = blacklisted_conditions.get(&scoped_cid) {
                                                if bl_time.elapsed() < std::time::Duration::from_secs(CONDITION_BLACKLIST_TTL_SECS) {
                                                    info!("🚫 CONDITION BLACKLISTED (anomaly): {} — failed {}s ago, skipping",
                                                        cid, bl_time.elapsed().as_secs());
                                                    true
                                                } else {
                                                    blacklisted_conditions.remove(&scoped_cid);
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        })
                                        .unwrap_or(false);

                                    // MATCH-LEVEL BLACKLIST: skip entire match when conditions keep dying
                                    let anomaly_match_blacklisted = {
                                        if let Some(bl_time) = blacklisted_matches.get(&anomaly.match_key) {
                                            if bl_time.elapsed() < std::time::Duration::from_secs(MATCH_BLACKLIST_TTL_SECS) {
                                                info!("🚫 MATCH BLACKLISTED (anomaly): {} — failed {}s ago, skipping",
                                                    &anomaly.match_key, bl_time.elapsed().as_secs());
                                                true
                                            } else {
                                                blacklisted_matches.remove(&anomaly.match_key);
                                                false
                                            }
                                        } else {
                                            false
                                        }
                                    };

                                    // === SCORE-CONFIRMED GATE ===
                                    // Anomaly auto-bet ONLY when live score confirms the value direction.
                                    // Leading team must be the same as value_side. No score = no bet.
                                    let anomaly_score_confirmed = if let Some(ref score) = anomaly.live_score {
                                        let parts: Vec<&str> = score.split('-').collect();
                                        if parts.len() == 2 {
                                            if let (Ok(s1), Ok(s2)) = (parts[0].trim().parse::<i32>(), parts[1].trim().parse::<i32>()) {
                                                let score_diff = s1 - s2;
                                                if score_diff == 0 {
                                                    // Draw: no score confirmation → skip
                                                    false
                                                } else {
                                                    let leading_side: u8 = if score_diff > 0 { 1 } else { 2 };
                                                    let confirmed = leading_side == anomaly.value_side;
                                                    if !confirmed {
                                                        info!("🛡️ SCORE-CONFIRM GATE: {} score={} leading_side={} but value_side={} — score CONTRADICTS anomaly, blocking",
                                                            anomaly.match_key, score, leading_side, anomaly.value_side);
                                                    }
                                                    confirmed
                                                }
                                            } else { false }
                                        } else { false }
                                    } else {
                                        // No live score at all → cannot confirm → skip
                                        false
                                    };

                                    // === DISCREPANCY MINIMUM for auto-bet ===
                                        let anomaly_disc_min = anomaly_min_disc_autobet(anomaly_sport);
                                        let anomaly_disc_ok = anomaly.discrepancy_pct >= anomaly_disc_min;
                                    if !anomaly_disc_ok && anomaly.discrepancy_pct >= MIN_EDGE_PCT {
                                        info!("📊 ANOMALY DISC TOO LOW for auto-bet: {} disc={:.1}% < {:.0}% (alert only)",
                                            anomaly.match_key, anomaly.discrepancy_pct, anomaly_disc_min);
                                    }

                                    let should_auto_bet_anomaly = AUTO_BET_ENABLED
                                        && dashboard_autobet_enabled
                                        && (dashboard_sport_focus.contains(&"all".to_string()) || dashboard_sport_focus.iter().any(|s| s == anomaly_sport))
                                        && AUTO_BET_ODDS_ANOMALY_ENABLED
                                        && anomaly.is_live
                                        && anomaly.confidence == "HIGH"
                                        && anomaly_odds_ok
                                        && anomaly_sport_allowed
                                        && anomaly_score_confirmed // SCORE-CONFIRMED: leading team = value side
                                        && anomaly_disc_ok         // DISC MINIMUM: ≥15% for auto-bet
                                        && anomaly_within_daily_limit
                                        && azuro_odds >= ANOMALY_MIN_ODDS  // <1.45 production WR 63% vs need 69% → -EV
                                        && azuro_odds <= ANOMALY_MAX_ODDS  // >1.70 is -EV for anomaly
                                        && !azuro_odds_identical
                                        && market_source_count >= AUTO_BET_MIN_MARKET_SOURCES
                                        && !already_bet_this
                                        && !anomaly_condition_blacklisted
                                        && !anomaly_match_blacklisted
                                        && anomaly.condition_id.is_some()
                                        && anomaly.outcome_id.is_some()
                                        && anomaly_stake >= 0.50
                                        && anomaly_bankroll_ok
                                        && anomaly_pending_ok    // MAX_CONCURRENT_PENDING guard
                                        && anomaly_streak_ok;    // LOSS_STREAK pause guard

                                    if anomaly.is_live && market_source_count < AUTO_BET_MIN_MARKET_SOURCES {
                                        info!("⏭️ ODDS ANOMALY {} skipped for auto-bet: only {} market source(s)",
                                            anomaly.match_key, market_source_count);
                                    }

                                    let mut anomaly_alert_sent = false;

                                    if should_auto_bet_anomaly {
                                        let stake = anomaly_stake;
                                        let mut condition_id = anomaly.condition_id.as_ref().unwrap().clone();
                                        let mut outcome_id = anomaly.outcome_id.as_ref().unwrap().clone();

                                        // Condition freshness: how stale is our last sighting? (GQL)
                                        let condition_age_ms_b = condition_last_seen.get(&condition_id)
                                            .map(|ts| ts.elapsed().as_millis() as u64)
                                            .unwrap_or(999999);
                                        let condition_max_age_ms_b = condition_max_age_limit_ms(
                                            anomaly.chain.as_deref(),
                                            &anomaly.azuro_bookmaker,
                                        );

                                        // === PRE-FLIGHT GATE (Path B): WS-first with GQL fallback ===
                                        let mut ws_result_b = {
                                            let cache_r = ws_condition_cache.read().await;
                                            ws_gate_check(&cache_r, &condition_id, ws_state_gate_enabled)
                                        };

                                        // Same subscription race as Path A — probe once to avoid GQL fallback.
                                        if ws_state_gate_enabled {
                                            if matches!(ws_result_b, WsGateResult::NoData | WsGateResult::Stale { .. }) {
                                                let _ = ws_sub_tx.try_send(vec![condition_id.clone()]);
                                                // SPEED-FIRST: při GQL fresh nečekat; při GQL stale krátký probe pro WS.
                                                if condition_age_ms_b > condition_max_age_ms_b {
                                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                                    ws_result_b = {
                                                        let cache_r = ws_condition_cache.read().await;
                                                        ws_gate_check(&cache_r, &condition_id, ws_state_gate_enabled)
                                                    };
                                                }
                                            }
                                        }

                                        let gate_blocked_b = match &ws_result_b {
                                            WsGateResult::NotActive { state, age_ms } => {
                                                warn!("🚫 WS-GATE ODDS #{}: condition {} state={} (ws_age={}ms) — DROP",
                                                    aid, &condition_id, state, age_ms);
                                                // CONDITION BLACKLIST: WS says not active → blacklist
                                                blacklisted_conditions.insert(
                                                    scoped_condition_key(&base_match_key, &condition_id),
                                                    std::time::Instant::now(),
                                                );
                                                ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                    "alert_id": aid, "match_key": anomaly.match_key,
                                                    "condition_id": condition_id, "outcome_id": outcome_id,
                                                    "error": format!("WS gate: condition state={}", state),
                                                    "reason_code": "WsStateNotActive",
                                                    "is_condition_state_reject": true,
                                                    "ws_state": state, "ws_age_ms": age_ms,
                                                    "condition_age_ms": condition_age_ms_b,
                                                    "requested_odds": azuro_odds,
                                                    "stake": stake, "path": "anomaly_odds",
                                                    "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                }));
                                                log_event("WS_GATE_DROP", &serde_json::json!({
                                                    "alert_id": aid, "condition_id": condition_id,
                                                    "ws_state": state, "ws_age_ms": age_ms,
                                                    "gql_age_ms": condition_age_ms_b, "reason": "WsStateNotActive",
                                                }));
                                                ws_gate_not_active_count += 1;
                                                true
                                            }
                                            WsGateResult::Active { age_ms } => {
                                                debug!("✅ WS-GATE ODDS #{}: condition {} Active (ws_age={}ms)", aid, &condition_id, age_ms);
                                                ws_gate_active_count += 1;
                                                false
                                            }
                                            WsGateResult::Stale { age_ms } => {
                                                ws_gate_stale_fallback_count += 1;
                                                // WS data stale → fallback to GQL age check
                                                if condition_age_ms_b > condition_max_age_ms_b {
                                                    warn!("🚫 PRE-FLIGHT ODDS #{}: WsStale({}ms)+GqlStale({}ms) — dropping",
                                                        aid, age_ms, condition_age_ms_b);
                                                    ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                        "alert_id": aid, "match_key": anomaly.match_key,
                                                        "condition_id": condition_id, "outcome_id": outcome_id,
                                                        "error": format!("WS stale ({}ms) + GQL stale ({}ms)", age_ms, condition_age_ms_b),
                                                        "reason_code": "WsStaleGqlStale",
                                                        "is_condition_state_reject": true,
                                                        "ws_age_ms": age_ms,
                                                        "condition_age_ms": condition_age_ms_b,
                                                        "requested_odds": azuro_odds,
                                                        "stake": stake, "path": "anomaly_odds",
                                                        "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                    }));
                                                    true
                                                } else {
                                                    info!("⚡ WS-GATE ODDS #{}: WsStale({}ms) but GQL fresh({}ms) — proceeding",
                                                        aid, age_ms, condition_age_ms_b);
                                                    false
                                                }
                                            }
                                            WsGateResult::NoData => {
                                                ws_gate_nodata_fallback_count += 1;
                                                // No WS data → fallback to GQL age check
                                                if condition_age_ms_b > condition_max_age_ms_b {
                                                    ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                        "alert_id": aid, "match_key": anomaly.match_key,
                                                        "condition_id": condition_id, "outcome_id": outcome_id,
                                                        "error": format!("WS no data + GQL stale ({}ms)", condition_age_ms_b),
                                                        "reason_code": "WsNoDataGqlStale",
                                                        "is_condition_state_reject": true,
                                                        "condition_age_ms": condition_age_ms_b,
                                                        "requested_odds": azuro_odds,
                                                        "stake": stake, "path": "anomaly_odds",
                                                        "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                    }));
                                                    true
                                                } else {
                                                    info!("⚡ WS-GATE ODDS #{}: WsNoData but GQL fresh({}ms) — proceeding",
                                                        aid, condition_age_ms_b);
                                                    false
                                                }
                                            }
                                            WsGateResult::Disabled => {
                                                // WS gate disabled → fallback to GQL gate
                                                if condition_age_ms_b > condition_max_age_ms_b {
                                                    warn!("🚫 PRE-FLIGHT GATE ODDS #{}: condition {} [WsDisabled->GqlStale] gql_age={}ms > {}ms — dropping",
                                                        aid, &condition_id, condition_age_ms_b, condition_max_age_ms_b);
                                                    ledger_write("AUTO_BET_SKIPPED", &serde_json::json!({
                                                        "alert_id": aid, "match_key": anomaly.match_key,
                                                        "condition_id": condition_id, "outcome_id": outcome_id,
                                                        "error": "pre-flight gate: WsDisabled->GqlStale",
                                                        "reason_code": "PreFlightStale",
                                                        "is_condition_state_reject": true,
                                                        "condition_age_ms": condition_age_ms_b,
                                                        "requested_odds": azuro_odds,
                                                        "stake": stake, "path": "anomaly_odds",
                                                        "retries": 0, "pipeline_ms": 0, "rtt_ms": 0,
                                                    }));
                                                    log_event("PREFLIGHT_GATE", &serde_json::json!({
                                                        "alert_id": aid, "condition_id": condition_id,
                                                        "condition_age_ms": condition_age_ms_b, "reason": "WsDisabled->GqlStale",
                                                    }));
                                                    true
                                                } else {
                                                    false
                                                }
                                            }
                                        };
                                        if gate_blocked_b {
                                            // Don't mark inflight — we never sent it
                                        } else {

                                        let decision_instant = std::time::Instant::now();
                                        let decision_ts_b = Utc::now();
                                        let amount_raw = (stake * 1e6) as u64;

                                        let max_retries = AUTO_BET_RETRY_MAX;
                                        let mut attempt = 0;
                                        let mut minodds_fallback_applied = false;
                                        loop {
                                        let min_odds_factor = min_odds_factor_with_fallback(&match_key_for_bet, minodds_fallback_applied);
                                        let (min_odds, min_odds_display_b) = compute_min_odds_raw(azuro_odds, min_odds_factor);
                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "requestedOdds": azuro_odds,
                                            "matchKey": match_key_for_bet,
                                            "team1": anomaly.team1,
                                            "team2": anomaly.team2,
                                            "valueTeam": value_team,
                                        });
                                        // Signal TTL check — abort if decision is stale
                                        if decision_instant.elapsed() > std::time::Duration::from_secs(SIGNAL_TTL_SECS) {
                                            warn!("⏰ AUTO-BET ODDS #{}: Signal TTL expired ({}ms elapsed) — aborting stale bet",
                                                aid, decision_instant.elapsed().as_millis());
                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                inflight_conditions.remove(key);
                                            }
                                            inflight_conditions.remove(&match_key_for_bet);
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "⏰ <b>AUTO-BET #{} TTL EXPIRED</b>\n\
                                                     🏷️ <b>{}</b> | path: <b>anomaly_odds</b>\n\
                                                     🧩 match: <b>{}</b>\n\
                                                     ⏱ elapsed: {}ms",
                                                    aid,
                                                    match_key_for_bet.split("::").next().unwrap_or("?").to_uppercase(),
                                                    match_key_for_bet,
                                                    decision_instant.elapsed().as_millis()
                                                )
                                            ).await;
                                            break;
                                        }
                                        // Pipeline budget check — abort if processing took too long
                                        let elapsed_pipeline_b = decision_instant.elapsed().as_millis() as u64;
                                        if elapsed_pipeline_b > PIPELINE_BUDGET_MS {
                                            warn!("⏰ AUTO-BET ODDS #{}: Pipeline budget exceeded ({}ms > {}ms) — dropping",
                                                aid, elapsed_pipeline_b, PIPELINE_BUDGET_MS);
                                            ledger_write("BET_FAILED", &serde_json::json!({
                                                "alert_id": aid, "match_key": &match_key_for_bet,
                                                "condition_id": &condition_id, "outcome_id": &outcome_id,
                                                "error": "pipeline budget exceeded",
                                                "reason_code": "PipelineBudgetExceeded",
                                                "is_condition_state_reject": false,
                                                "condition_age_ms": condition_age_ms_b,
                                                "pipeline_ms": elapsed_pipeline_b,
                                                "requested_odds": azuro_odds, "stake": stake,
                                                "path": "anomaly_odds", "retries": attempt,
                                                "rtt_ms": 0,
                                            }));
                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                inflight_conditions.remove(key);
                                            }
                                            inflight_conditions.remove(&match_key_for_bet);
                                            break;
                                        }
                                        let send_ts_b = Utc::now();
                                        let send_instant_b = std::time::Instant::now();
                                        match client.post(format!("{}/bet", executor_url))
                                            .json(&bet_body).send().await {
                                            Ok(resp) => {
                                                let response_ts_b = Utc::now();
                                                let rtt_ms_b = send_instant_b.elapsed().as_millis();
                                                let pipeline_ms_b = decision_instant.elapsed().as_millis();
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
                                                            // Permanent dead condition — no retry, but blacklist immediately
                                                            let is_condition_dead = err_lower.contains("not available")
                                                                || err_lower.contains("live is not");
                                                            let is_fatal = err_lower.contains("insufficient")
                                                                || err_lower.contains("allowance")
                                                                || err_lower.contains("revert")
                                                                || err_lower.contains("nonce");
                                                            if is_condition_paused && !is_condition_dead && !is_fatal && attempt < max_retries {
                                                                if let Some((new_condition_id, new_outcome_id)) = remap_execution_ids_from_state(
                                                                    &state,
                                                                    &match_key_for_bet,
                                                                    &anomaly.team1,
                                                                    &anomaly.team2,
                                                                    anomaly.value_side,
                                                                ) {
                                                                    if new_condition_id != condition_id || new_outcome_id != outcome_id {
                                                                        info!("🔁 AUTO-BET ODDS #{} remap retry: cond {}→{} out {}→{}",
                                                                            aid, condition_id, new_condition_id, outcome_id, new_outcome_id);
                                                                        if let Some(key) = scoped_cond_key.as_ref() {
                                                                            inflight_conditions.remove(key);
                                                                        }
                                                                        condition_id = new_condition_id;
                                                                        outcome_id = new_outcome_id;
                                                                        cond_id_str = condition_id.clone();
                                                                        scoped_cond_key = Some(scoped_condition_key(&base_match_key, &cond_id_str));
                                                                        if let Some(key) = scoped_cond_key.as_ref() {
                                                                            inflight_conditions.insert(key.clone());
                                                                        }
                                                                        attempt += 1;
                                                                        tokio::time::sleep(std::time::Duration::from_millis(REMAP_RETRY_DELAY_MS)).await;
                                                                        continue;
                                                                    }
                                                                }
                                                                let too_stale_for_retry = condition_age_ms_b > RETRY_CONDITION_MAX_AGE_MS;
                                                                let too_late_for_retry = decision_instant.elapsed().as_millis() as u64 > (PIPELINE_BUDGET_MS / 2);
                                                                if too_stale_for_retry || too_late_for_retry {
                                                                    info!("⏭️ AUTO-BET ODDS #{} retry skipped (stale={} late={} age={}ms pipeline={}ms)",
                                                                        aid,
                                                                        too_stale_for_retry,
                                                                        too_late_for_retry,
                                                                        condition_age_ms_b,
                                                                        decision_instant.elapsed().as_millis());
                                                                } else {
                                                                    attempt += 1;
                                                                    let base_delay = AUTO_BET_RETRY_DELAYS_MS.get(attempt.saturating_sub(1)).copied().unwrap_or(500);
                                                                    let jitter = ((aid as u64).wrapping_mul(7).wrapping_add(attempt as u64 * 13)) % (base_delay / 2 + 1);
                                                                    let delay_ms = base_delay + jitter;
                                                                    info!("🔄 AUTO-BET ODDS #{} retry {}/{}: condition paused, waiting {}ms (base={}+jitter={})... ({})",
                                                                        aid, attempt, max_retries, delay_ms, base_delay, jitter, err);
                                                                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                                                    continue;
                                                                }
                                                            }
                                                            let is_minodds_b = err_lower.contains("min odds") || err_lower.contains("minodds")
                                                                || err_lower.contains("real odds");
                                                            if is_minodds_b && !minodds_fallback_applied && attempt < max_retries {
                                                                attempt += 1;
                                                                minodds_fallback_applied = true;
                                                                let base_delay = 30;
                                                                let jitter = ((aid as u64).wrapping_mul(11).wrapping_add(attempt as u64 * 17)) % 80;
                                                                let delay_ms = base_delay + jitter;
                                                                info!("🔁 AUTO-BET ODDS #{} min-odds fallback retry {}/{}: factor step -{:.2}, wait {}ms ({})",
                                                                    aid, attempt, max_retries, MIN_ODDS_FALLBACK_STEP, delay_ms, err);
                                                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                                                continue;
                                                            }
                                                            let is_dedup_b = err_lower.contains("dedup") || err_lower.contains("already bet");
                                                            let is_condition_state_reject_b = is_condition_paused || is_condition_dead;
                                                            let reason_code_b = if is_condition_state_reject_b { "ConditionNotRunning" }
                                                                else if is_minodds_b { "MinOddsReject" }
                                                                else if is_dedup_b { "Dedup" }
                                                                else if is_fatal { "Fatal" }
                                                                else { "Unknown" };
                                                            // CONDITION BLACKLIST: add failed condition to blacklist
                                                            if is_condition_state_reject_b || is_minodds_b {
                                                                blacklisted_conditions.insert(
                                                                    scoped_condition_key(&base_match_key, &condition_id),
                                                                    std::time::Instant::now(),
                                                                );
                                                                info!("🚫 BLACKLISTED condition {} (reason={}, TTL={}s)",
                                                                    &condition_id, reason_code_b, CONDITION_BLACKLIST_TTL_SECS);
                                                            }
                                                            // MATCH BLACKLIST: ConditionNotRunning → block entire match
                                                            if is_condition_state_reject_b {
                                                                blacklisted_matches.insert(anomaly.match_key.clone(), std::time::Instant::now());
                                                                info!("🚫 MATCH BLACKLISTED {} (ConditionNotRunning, TTL={}s)",
                                                                    &anomaly.match_key, MATCH_BLACKLIST_TTL_SECS);
                                                            }
                                                            error!("❌ AUTO-BET ODDS #{} FAILED: {} (cond={}, match={}, rtt={}ms, pipeline={}ms, odds={:.4}, min={:.4}, reason={})",
                                                                aid, err,
                                                                &condition_id,
                                                                match_key_for_bet,
                                                                rtt_ms_b, pipeline_ms_b,
                                                                azuro_odds, min_odds_display_b,
                                                                reason_code_b);
                                                            // === LEDGER: BET_FAILED (Path B, skip DEDUP noise) ===
                                                            if !is_dedup_b {
                                                            ledger_write("BET_FAILED", &serde_json::json!({
                                                                "alert_id": aid, "match_key": match_key_for_bet,
                                                                "condition_id": condition_id, "outcome_id": outcome_id,
                                                                "error": err, "retries": attempt,
                                                                "requested_odds": azuro_odds, "min_odds": min_odds_display_b,
                                                                "stake": stake, "path": "anomaly_odds",
                                                                "decision_ts": decision_ts_b.to_rfc3339(),
                                                                "send_ts": send_ts_b.to_rfc3339(),
                                                                "response_ts": response_ts_b.to_rfc3339(),
                                                                "rtt_ms": rtt_ms_b as u64,
                                                                "pipeline_ms": pipeline_ms_b as u64,
                                                                "is_minodds_reject": is_minodds_b,
                                                                "is_dedup": is_dedup_b,
                                                                "is_condition_state_reject": is_condition_state_reject_b,
                                                                "reason_code": reason_code_b,
                                                                "condition_age_ms": condition_age_ms_b,
                                                            }));
                                                            }
                                                            if !is_dedup_b {
                                                                let _ = tg_send_message(&client, &token, chat_id,
                                                                    &format_auto_bet_failed_message(
                                                                        aid,
                                                                        "anomaly_odds",
                                                                        &match_key_for_bet,
                                                                        &condition_id,
                                                                        reason_code_b,
                                                                        err,
                                                                        attempt,
                                                                        rtt_ms_b,
                                                                        pipeline_ms_b,
                                                                        azuro_odds,
                                                                        min_odds_display_b,
                                                                    )
                                                                ).await;
                                                            } else {
                                                                info!("🔇 Suppressed TG for dedup rejection odds #{}: {}", aid, err);
                                                            }
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                inflight_conditions.remove(key);
                                                            }
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break;
                                                        } else if is_rejected {
                                                            error!("❌ AUTO-BET ODDS #{} REJECTED: state={} (cond={}, match={})",
                                                                aid, br.state.as_deref().unwrap_or("?"),
                                                                &condition_id,
                                                                match_key_for_bet);
                                                            let _ = tg_send_message(&client, &token, chat_id,
                                                                &format_auto_bet_rejected_message(
                                                                    aid,
                                                                    "anomaly_odds",
                                                                    &match_key_for_bet,
                                                                    &condition_id,
                                                                    br.state.as_deref().unwrap_or("?"),
                                                                )
                                                            ).await;
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                inflight_conditions.remove(key);
                                                            }
                                                            inflight_conditions.remove(&match_key_for_bet);
                                                            break;
                                                        } else {
                                                            auto_bet_count += 1;
                                                            daily_wagered += stake;
                                                            // Persist daily P&L
                                                            {
                                                                let today = Utc::now().format("%Y-%m-%d").to_string();
                                                                let _ = std::fs::write(bet_count_path, format!("{}|{}", today, auto_bet_count));
                                                                let _ = std::fs::write("data/daily_pnl.json",
                                                                    serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                                                            }
                                                            let bet_id = br.bet_id.as_deref().unwrap_or("?");
                                                            let bet_state = br.state.as_deref().unwrap_or("?");
                                                            let token_id_opt = sanitize_token_id(br.token_id.clone());
                                                            let graph_bet_id_opt = br.graph_bet_id.clone();
                                                            let accepted_odds = br.accepted_odds.unwrap_or(azuro_odds);

                                                            already_bet_matches.insert(match_key_for_bet.clone());
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                already_bet_conditions.insert(key.clone());
                                                            }
                                                            already_bet_base_matches.insert(base_match_key.clone());
                                                            // Track map_winner placements for sibling-map dedup logic
                                                            if match_key_for_bet != base_match_key {
                                                                already_bet_map_winners.insert(base_match_key.clone());
                                                            }

                                                            // === EXPOSURE TRACKING (odds anomaly path) ===
                                                            if let Some(key) = scoped_cond_key.as_ref() {
                                                                *condition_exposure.entry(key.clone()).or_insert(0.0) += stake;
                                                            }
                                                            *match_exposure.entry(base_match_key.clone()).or_insert(0.0) += stake;
                                                            *sport_exposure.entry(anomaly_sport.to_string()).or_insert(0.0) += stake;
                                                            inflight_wagered_total += stake;

                                                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                .create(true).append(true)
                                                                .open(bet_history_path) {
                                                                use std::io::Write;
                                                                let _ = writeln!(f, "{}|{}|{}|{}|{}",
                                                                    match_key_for_bet, cond_id_str,
                                                                    value_team, accepted_odds, Utc::now().to_rfc3339());
                                                            }

                                                            let is_dry_run = bet_state == "DRY-RUN" || bet_id.starts_with("dry-");
                                                            if !is_dry_run {
                                                                active_bets.push(ActiveBet {
                                                                    alert_id: aid,
                                                                    bet_id: bet_id.to_string(),
                                                                    match_key: anomaly.match_key.clone(),
                                                                    market_key: anomaly.market_key.clone(),
                                                                    team1: anomaly.team1.clone(),
                                                                    team2: anomaly.team2.clone(),
                                                                    value_team: value_team.clone(),
                                                                    amount_usd: stake,
                                                                    odds: accepted_odds,
                                                                    placed_at: Utc::now().to_rfc3339(),
                                                                    condition_id: condition_id.clone(),
                                                                    outcome_id: outcome_id.clone(),
                                                                    graph_bet_id: graph_bet_id_opt.clone(),
                                                                    token_id: token_id_opt.clone(),
                                                                    path: "anomaly_odds".to_string(),
                                                                });
                                                                let token_to_write = token_id_opt.as_deref().unwrap_or("?");
                                                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                    .create(true).append(true)
                                                                    .open(pending_claims_path) {
                                                                    use std::io::Write;
                                                                    let _ = writeln!(f, "{}|{}|{}|{}|{}|{}|{}",
                                                                        token_to_write,
                                                                        bet_id, anomaly.match_key,
                                                                        value_team, stake, accepted_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                                // === LEDGER: BET PLACED ===
                                                                ledger_write("PLACED", &serde_json::json!({
                                                                    "alert_id": aid, "bet_id": bet_id,
                                                                    "match_key": anomaly.match_key,
                                                                    "market_key": anomaly.market_key,
                                                                    "team1": anomaly.team1, "team2": anomaly.team2,
                                                                    "value_team": value_team,
                                                                    "amount_usd": stake, "odds": accepted_odds,
                                                                    "requested_odds": azuro_odds,
                                                                    "min_odds": min_odds_display_b,
                                                                    "condition_id": condition_id,
                                                                    "outcome_id": outcome_id,
                                                                    "token_id": token_id_opt,
                                                                    "graph_bet_id": graph_bet_id_opt,
                                                                    "on_chain_state": bet_state,
                                                                    "path": "anomaly_odds",
                                                                    "condition_age_ms": condition_age_ms_b,
                                                                    // Anomaly audit data — enables post-hoc signal validation
                                                                    "anomaly_confidence": anomaly.confidence,
                                                                    "anomaly_disc_pct": anomaly.discrepancy_pct,
                                                                    "anomaly_market_bookmaker": anomaly.market_bookmaker,
                                                                    "anomaly_azuro_w1": anomaly.azuro_w1,
                                                                    "anomaly_azuro_w2": anomaly.azuro_w2,
                                                                    "anomaly_market_w1": anomaly.market_w1,
                                                                    "anomaly_market_w2": anomaly.market_w2,
                                                                    "anomaly_live_score": anomaly.live_score,
                                                                    "anomaly_detailed_score": anomaly.detailed_score,
                                                                    "anomaly_market_source_count": market_source_count,
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

                                                                // === LEDGER: ON-CHAIN ACCEPTED (immediate) ===
                                                                if bet_state == "Accepted" {
                                                                    ledger_write("ON_CHAIN_ACCEPTED", &serde_json::json!({
                                                                        "alert_id": aid,
                                                                        "match_key": anomaly.match_key,
                                                                        "market_key": anomaly.market_key,
                                                                        "condition_id": condition_id,
                                                                        "outcome_id": outcome_id,
                                                                        "bet_id": bet_id,
                                                                        "on_chain_state": bet_state,
                                                                        "token_id": token_id_opt,
                                                                        "graph_bet_id": graph_bet_id_opt,
                                                                        "path": "anomaly_odds",
                                                                        "odds": accepted_odds,
                                                                        "requested_odds": azuro_odds,
                                                                        "stake": stake,
                                                                    }));
                                                                }
                                                            }

                                                            let result_msg = format_auto_bet_result_message(
                                                                aid,
                                                                "anomaly_odds",
                                                                &match_key_for_bet,
                                                                &value_team,
                                                                accepted_odds,
                                                                stake,
                                                                bet_id,
                                                                bet_state,
                                                                auto_bet_count,
                                                                is_dry_run,
                                                            );
                                                            if let Err(e) = tg_send_message(&client, &token, chat_id, &result_msg).await {
                                                                error!("Failed to send auto-bet anomaly result alert: {}", e);
                                                            } else {
                                                                anomaly_alert_sent = true;
                                                            }

                                                            // === UNIFIED FOLLOW-UP: Poll Created bets to detect async Rejected ===
                                                            if !is_dry_run && (bet_state == "Created" || bet_state == "Pending") {
                                                                let follow_client = client.clone();
                                                                let follow_token = token.clone();
                                                                let follow_executor = executor_url.clone();
                                                                let follow_bet_id = bet_id.to_string();
                                                                let follow_aid = aid;
                                                                let follow_team = value_team.clone();
                                                                let follow_chat = chat_id;
                                                                let follow_match_key = anomaly.match_key.clone();
                                                                let follow_market_key = anomaly.market_key.clone();
                                                                let follow_condition_id = condition_id.clone();
                                                                let follow_outcome_id = outcome_id.clone();
                                                                let follow_odds = azuro_odds;
                                                                let follow_stake = stake;
                                                                tokio::spawn(async move {
                                                                    tokio::time::sleep(Duration::from_secs(20)).await;
                                                                    if let Ok(resp) = follow_client.get(
                                                                        format!("{}/bet/{}", follow_executor, follow_bet_id)
                                                                    ).send().await {
                                                                        if let Ok(br) = resp.json::<serde_json::Value>().await {
                                                                            let final_state = br.get("state")
                                                                                .and_then(|v| v.as_str()).unwrap_or("?");
                                                                            let err_msg = br.get("errorMessage")
                                                                                .and_then(|v| v.as_str()).unwrap_or("");
                                                                            if final_state == "Rejected" || final_state == "Failed" || final_state == "Cancelled" {
                                                                                let alert = format!(
                                                                                    "❌ <b>AUTO-BET #{} REJECTED (follow-up)</b>\n\
                                                                                     path: <b>anomaly_odds</b>\n\
                                                                                     💡 Pick: <b>{}</b>\n\
                                                                                     🧠 on-chain state: <b>{}</b>\n\
                                                                                     📝 {}",
                                                                                    follow_aid, follow_team, final_state, err_msg);
                                                                                let _ = tg_send_message(
                                                                                    &follow_client, &follow_token,
                                                                                    follow_chat, &alert).await;
                                                                                warn!("❌ AUTO-BET ODDS #{} FOLLOW-UP REJECTED: {} err={}",
                                                                                    follow_aid, follow_bet_id, err_msg);
                                                                                // LEDGER: write on-chain REJECTED event
                                                                                {
                                                                                    let entry = serde_json::json!({
                                                                                        "ts": chrono::Utc::now().to_rfc3339(),
                                                                                        "event": "ON_CHAIN_REJECTED",
                                                                                        "alert_id": follow_aid,
                                                                                        "match_key": follow_match_key,
                                                                                        "market_key": follow_market_key,
                                                                                        "condition_id": follow_condition_id,
                                                                                        "outcome_id": follow_outcome_id,
                                                                                        "bet_id": follow_bet_id,
                                                                                        "on_chain_state": final_state,
                                                                                        "error": err_msg,
                                                                                        "path": "anomaly_odds",
                                                                                        "odds": follow_odds,
                                                                                        "stake": follow_stake,
                                                                                    });
                                                                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                                        .create(true).append(true).open("data/ledger.jsonl") {
                                                                                        use std::io::Write;
                                                                                        let _ = writeln!(f, "{}", entry);
                                                                                    }
                                                                                }
                                                                            } else if final_state == "Accepted" {
                                                                                let token_id = br.get("tokenId")
                                                                                    .and_then(|v| v.as_str()).unwrap_or("?");
                                                                                let alert = format!(
                                                                                    "✅ <b>AUTO-BET #{} CONFIRMED (follow-up)</b>\n\
                                                                                     path: <b>anomaly_odds</b>\n\
                                                                                     💡 Pick: <b>{}</b>\n\
                                                                                     🧾 Bet ID: <code>{}</code>\n\
                                                                                     🧠 on-chain state: <b>{}</b>\n\
                                                                                     🪙 tokenId: <code>{}</code>",
                                                                                    follow_aid,
                                                                                    follow_team,
                                                                                    follow_bet_id,
                                                                                    final_state,
                                                                                    token_id,
                                                                                );
                                                                                let _ = tg_send_message(
                                                                                    &follow_client,
                                                                                    &follow_token,
                                                                                    follow_chat,
                                                                                    &alert,
                                                                                ).await;
                                                                                info!("✅ AUTO-BET ODDS #{} FOLLOW-UP CONFIRMED: state={} tokenId={}",
                                                                                    follow_aid, final_state, token_id);
                                                                                // LEDGER: write on-chain ACCEPTED event
                                                                                {
                                                                                    let entry = serde_json::json!({
                                                                                        "ts": chrono::Utc::now().to_rfc3339(),
                                                                                        "event": "ON_CHAIN_ACCEPTED",
                                                                                        "alert_id": follow_aid,
                                                                                        "match_key": follow_match_key,
                                                                                        "market_key": follow_market_key,
                                                                                        "condition_id": follow_condition_id,
                                                                                        "outcome_id": follow_outcome_id,
                                                                                        "bet_id": follow_bet_id,
                                                                                        "on_chain_state": final_state,
                                                                                        "token_id": token_id,
                                                                                        "path": "anomaly_odds",
                                                                                        "odds": follow_odds,
                                                                                        "stake": follow_stake,
                                                                                    });
                                                                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                                        .create(true).append(true).open("data/ledger.jsonl") {
                                                                                        use std::io::Write;
                                                                                        let _ = writeln!(f, "{}", entry);
                                                                                    }
                                                                                }
                                                                            } else {
                                                                                info!("⏳ AUTO-BET ODDS #{} FOLLOW-UP: state={} (still pending)",
                                                                                    follow_aid, final_state);
                                                                            }
                                                                        }
                                                                    }
                                                                });
                                                            }
                                                            break;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = tg_send_message(&client, &token, chat_id,
                                                            &format!(
                                                                "❌ <b>AUTO-BET #{} RESPONSE ERROR</b>\n\
                                                                 path: <b>anomaly_odds</b>\n\
                                                                 match: <b>{}</b>\n\
                                                                 {}",
                                                                aid,
                                                                match_key_for_bet,
                                                                e,
                                                            )
                                                        ).await;
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tg_send_message(&client, &token, chat_id,
                                                    &format!(
                                                        "❌ <b>AUTO-BET #{} EXECUTOR OFFLINE</b>\n\
                                                         path: <b>anomaly_odds</b>\n\
                                                         match: <b>{}</b>\n\
                                                         {}",
                                                        aid,
                                                        match_key_for_bet,
                                                        e,
                                                    )
                                                ).await;
                                                break;
                                            }
                                        }
                                        } // end loop
                                        } // end pre-flight gate else block (Path B)
                                    } else if anomaly_condition_blacklisted {
                                        info!("🔕 MANUAL ALERT SUPPRESSED (blacklisted condition anomaly): {}",
                                            anomaly.match_key);
                                    } else if anomaly_match_blacklisted {
                                        info!("🔕 MANUAL ALERT SUPPRESSED (blacklisted match anomaly): {}",
                                            anomaly.match_key);
                                    } else if already_bet_this {
                                        info!("🔕 MANUAL ALERT SUPPRESSED (anomaly dedup): {} (base={})",
                                            anomaly.match_key, base_match_key);
                                    } else if !mute_manual_alerts {
                                        let now_alert = Utc::now();
                                        let too_soon = manual_offer_last_sent
                                            .get(&anomaly.match_key)
                                            .map(|ts| (now_alert - *ts).num_seconds() < MANUAL_MATCH_COOLDOWN_SECS)
                                            .unwrap_or(false);
                                        if too_soon {
                                            info!("⏱️ MANUAL ALERT THROTTLED (anomaly): {} (cooldown={}s)",
                                                anomaly.match_key, MANUAL_MATCH_COOLDOWN_SECS);
                                        } else {
                                            let msg = format_anomaly_alert(&anomaly, aid);
                                            match tg_send_message(&client, &token, chat_id, &msg).await {
                                                Ok(msg_id) => {
                                                    anomaly_alert_sent = true;
                                                    msg_id_to_alert_id.insert(msg_id, aid);
                                                    manual_offer_last_sent.insert(anomaly.match_key.clone(), now_alert);
                                                }
                                                Err(e) => {
                                                    error!("Failed to send alert: {}", e);
                                                }
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
                let mut needs_pending_rewrite = false;
                claim_reconcile_counter += 1;
                if claim_reconcile_counter % LEDGER_RECONCILE_EVERY_CLAIM_TICKS == 0 {
                    let recovery_stats = recover_unresolved_accepts_from_ledger(&mut active_bets, &ledger_settled_ids);
                    if recovery_stats.recovered > 0 {
                        needs_pending_rewrite = true;
                        info!(
                            "🩹 CLAIM RECONCILE: recovered {} unresolved ON_CHAIN_ACCEPTED bets from ledger",
                            recovery_stats.recovered
                        );
                    }
                    let reconcile_signature = format!(
                        "{}|{}|{}|{}|{}",
                        recovery_stats.recovered,
                        recovery_stats.unresolved_total,
                        recovery_stats.stale_12h,
                        recovery_stats.stale_24h,
                        recovery_stats.oldest_age_hours.map(|v| v.floor() as i64).unwrap_or(0)
                    );
                    if reconcile_signature != last_reconcile_audit_signature
                        && (recovery_stats.recovered > 0 || recovery_stats.unresolved_total > 0)
                    {
                        info!(
                            "📋 SETTLEMENT RECONCILE AUDIT: unresolved={} stale12h={} stale24h={} oldest={:.1}h recovered={}",
                            recovery_stats.unresolved_total,
                            recovery_stats.stale_12h,
                            recovery_stats.stale_24h,
                            recovery_stats.oldest_age_hours.unwrap_or(0.0),
                            recovery_stats.recovered,
                        );
                        ledger_write("SETTLEMENT_RECONCILE_AUDIT", &serde_json::json!({
                            "unresolved_total": recovery_stats.unresolved_total,
                            "stale_12h": recovery_stats.stale_12h,
                            "stale_24h": recovery_stats.stale_24h,
                            "oldest_age_hours": recovery_stats.oldest_age_hours,
                            "recovered": recovery_stats.recovered,
                            "cadence_ticks": LEDGER_RECONCILE_EVERY_CLAIM_TICKS,
                        }));
                        last_reconcile_audit_signature = reconcile_signature;
                    }
                }

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
                                    let tx = cr.get("txHash").and_then(|v| v.as_str()).unwrap_or("");
                                    let token_ids: Vec<String> = cr.get("tokenIds")
                                        .and_then(|v| v.as_array())
                                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                                        .unwrap_or_default();
                                    let new_claim_tokens: Vec<String> = token_ids.iter()
                                        .filter(|tid| !claimed_token_ids.contains(*tid))
                                        .cloned()
                                        .collect();
                                    let is_new_tx = !tx.is_empty() && claimed_tx_hashes.insert(tx.to_string());
                                    let should_count_claim = claimed > 0 && (!new_claim_tokens.is_empty() || is_new_tx);
                                    for tid in &token_ids {
                                        claimed_token_ids.insert(tid.clone());
                                    }
                                    if should_count_claim {
                                        total_returned += payout;
                                        daily_returned += payout;
                                        {
                                            let today = Utc::now().format("%Y-%m-%d").to_string();
                                            let _ = std::fs::write("data/daily_pnl.json",
                                                serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                                        }
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!("💰 <b>AUTO-CLAIM (safety net)</b>\n\nVyplaceno {} sázek, ${:.2}\n💰 Nový zůstatek: {} USDT",
                                                claimed, payout, new_bal)
                                        ).await;
                                        ledger_write("EXECUTOR_CLAIM", &serde_json::json!({
                                            "claimed": claimed,
                                            "tokenIds": token_ids,
                                            "totalPayoutUsd": payout,
                                            "newBalanceUsd": new_bal,
                                            "txHash": tx,
                                            "context": "no_active_bets"
                                        }));
                                        ledger_write("SAFETY_CLAIM", &serde_json::json!({
                                            "claimed_count": claimed, "payout_usd": payout,
                                            "new_balance": new_bal, "context": "no_active_bets"
                                        }));
                                    } else if claimed > 0 {
                                        info!("💤 Duplicate safety-net claim ignored: claimed={} payout=${:.2} tx={} tokens={:?}",
                                            claimed, payout, tx, token_ids);
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
                                // Reset loss streak on win
                                if result == "Won" {
                                    consecutive_losses = 0;
                                    loss_streak_pause_until = None;
                                }
                                // === LEDGER: WON/CANCELED detected (check_payout) ===
                                if !ledger_settled_ids.contains(&bet.bet_id) {
                                    ledger_write(if result == "Won" { "WON" } else { "CANCELED" }, &serde_json::json!({
                                        "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                        "match_key": bet.match_key,
                                        "match_prefix": match_prefix_from_match_key(&bet.match_key),
                                        "market_key": bet.market_key,
                                        "value_team": bet.value_team,
                                        "amount_usd": bet.amount_usd, "odds": bet.odds,
                                        "payout_usd": payout_usd,
                                        "token_id": bet.token_id, "path": &bet.path, "settle": "check_payout"
                                    }));
                                    ledger_settled_ids.insert(bet.bet_id.clone());
                                }
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
                                    // NOTE: daily_wagered is already incremented at PLACED time (BUG FIX: removed double-count)
                                    // Loss streak tracking
                                    record_live_loss_for_streak(
                                        bet,
                                        &mut consecutive_losses,
                                        &mut loss_streak_pause_until,
                                    );
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write(
                                            "data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string(),
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
                                    // === LEDGER: LOST (check_payout) ===
                                    if !ledger_settled_ids.contains(&bet.bet_id) {
                                        ledger_write("LOST", &serde_json::json!({
                                            "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                            "match_key": bet.match_key,
                                            "match_prefix": match_prefix_from_match_key(&bet.match_key),
                                            "market_key": bet.market_key,
                                            "value_team": bet.value_team,
                                            "amount_usd": bet.amount_usd, "odds": bet.odds,
                                            "token_id": bet.token_id, "path": &bet.path, "settle": "check_payout"
                                        }));
                                        ledger_settled_ids.insert(bet.bet_id.clone());
                                    }
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
                                // === LEDGER: WON/CANCELED (bet_status) ===
                                if !ledger_settled_ids.contains(&bet.bet_id) {
                                    ledger_write(if effective_result == "Won" { "WON" } else { "CANCELED" }, &serde_json::json!({
                                        "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                        "match_key": bet.match_key,
                                        "match_prefix": match_prefix_from_match_key(&bet.match_key),
                                        "market_key": bet.market_key,
                                        "value_team": bet.value_team,
                                        "amount_usd": bet.amount_usd, "odds": bet.odds,
                                        "token_id": bet.token_id, "path": &bet.path, "settle": "bet_status"
                                    }));
                                    ledger_settled_ids.insert(bet.bet_id.clone());
                                }
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
                                record_live_loss_for_streak(
                                    bet,
                                    &mut consecutive_losses,
                                    &mut loss_streak_pause_until,
                                );
                                // === LEDGER: LOST (bet_status) ===
                                if !ledger_settled_ids.contains(&bet.bet_id) {
                                    ledger_write("LOST", &serde_json::json!({
                                        "alert_id": bet.alert_id, "bet_id": bet.bet_id,
                                        "match_key": bet.match_key,
                                        "match_prefix": match_prefix_from_match_key(&bet.match_key),
                                        "market_key": bet.market_key,
                                        "value_team": bet.value_team,
                                        "amount_usd": bet.amount_usd, "odds": bet.odds,
                                        "token_id": bet.token_id, "path": &bet.path, "settle": "bet_status"
                                    }));
                                    ledger_settled_ids.insert(bet.bet_id.clone());
                                }
                                // NOTE: daily_wagered is already incremented at PLACED time (BUG FIX: removed double-count)
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
                            deferred_claim_tokens.remove(tid);
                            if i < claim_details.len() {
                                verified_details.push(claim_details[i].clone());
                            }
                        } else {
                            let reason = payout_check.as_ref()
                                .and_then(|p| p.get("reason").and_then(|v| v.as_str()))
                                .unwrap_or("unknown");
                            info!("⏳ Token {} not ready on-chain yet ({}), deferring claim", tid, reason);
                            deferred_bets.push(tid.clone());
                            deferred_claim_tokens.insert(tid.clone());
                        }
                    }

                    // Remove deferred bets from the "remove" list — they need to stay active.
                    // IMPORTANT: also remove them from settled_bet_ids, otherwise they become zombies
                    // (skipped forever) and block new auto-bets via MAX_CONCURRENT_PENDING.
                    if !deferred_bets.is_empty() {
                        info!("⏳ {} bets deferred (chain not ready): {:?}", deferred_bets.len(), deferred_bets);
                        // Find bet_ids that match deferred tokens and remove from bets_to_remove
                        let deferred_set: std::collections::HashSet<&str> = deferred_bets.iter().map(|s| s.as_str()).collect();
                        bets_to_remove.retain(|bid| {
                            let bet_token = active_bets.iter().find(|b| b.bet_id == *bid).and_then(|b| b.token_id.as_deref());
                            !bet_token.map(|t| deferred_set.contains(t)).unwrap_or(false)
                        });

                        // Unmark as settled so next tick will retry the claim.
                        for b in active_bets.iter() {
                            if let Some(tid) = b.token_id.as_deref() {
                                if deferred_set.contains(tid) {
                                    settled_bet_ids.remove(&b.bet_id);
                                }
                            }
                        }
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
                                    let claimed_tokens = cr.token_ids.clone().unwrap_or_else(|| verified_tokens.clone());
                                    let new_claim_tokens: Vec<String> = claimed_tokens.iter()
                                        .filter(|tid| !claimed_token_ids.contains(*tid))
                                        .cloned()
                                        .collect();
                                    let is_new_tx = tx != "?" && claimed_tx_hashes.insert(tx.to_string());
                                    let should_count_claim = !claimed_tokens.is_empty() && (!new_claim_tokens.is_empty() || is_new_tx);
                                    for tid in &claimed_tokens {
                                        claimed_token_ids.insert(tid.clone());
                                    }
                                    if should_count_claim {
                                        total_returned += total_payout;
                                        daily_returned += total_payout;
                                        {
                                            let today = Utc::now().format("%Y-%m-%d").to_string();
                                            let _ = std::fs::write("data/daily_pnl.json",
                                                serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                                        }
                                        ledger_write("EXECUTOR_CLAIM", &serde_json::json!({
                                            "claimed": claimed_tokens.len(),
                                            "tokenIds": claimed_tokens,
                                            "totalPayoutUsd": total_payout,
                                            "newBalanceUsd": new_balance,
                                            "txHash": tx,
                                            "context": "batch_claim"
                                        }));
                                    } else {
                                        info!("💤 Duplicate batch claim ignored: payout=${:.2} tx={} tokens={:?}",
                                            total_payout, tx, claimed_tokens);
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
                                    if should_count_claim {
                                        for (aid, _t1, _t2, vt, amt, odds, res) in &verified_details {
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

                // Keep inflight cap grounded in reality: total USD currently locked
                // in on-chain pending + in-flight bets (NOT cumulative daily wagered).
                inflight_wagered_total = active_bets.iter().map(|b| b.amount_usd).sum();

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
                                    let tx = cr.get("txHash").and_then(|v| v.as_str()).unwrap_or("");
                                    let token_ids: Vec<String> = cr.get("tokenIds")
                                        .and_then(|v| v.as_array())
                                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                                        .unwrap_or_default();
                                    let new_claim_tokens: Vec<String> = token_ids.iter()
                                        .filter(|tid| !claimed_token_ids.contains(*tid))
                                        .cloned()
                                        .collect();
                                    let is_new_tx = !tx.is_empty() && claimed_tx_hashes.insert(tx.to_string());
                                    let should_count_claim = claimed > 0 && (!new_claim_tokens.is_empty() || is_new_tx);
                                    for tid in &token_ids {
                                        claimed_token_ids.insert(tid.clone());
                                    }
                                    if should_count_claim {
                                        total_returned += payout;
                                        daily_returned += payout;
                                        {
                                            let today = Utc::now().format("%Y-%m-%d").to_string();
                                            let _ = std::fs::write("data/daily_pnl.json",
                                                serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                                        }
                                        info!("💰 Safety-net auto-claim: {} bets, ${:.2} (daily_returned now ${:.2})", claimed, payout, daily_returned);
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!("💰 <b>AUTO-CLAIM (safety net)</b>\n\nVyplaceno {} sázek, ${:.2}\n💰 Nový zůstatek: {} USDT",
                                                claimed, payout, new_bal)
                                        ).await;
                                        ledger_write("EXECUTOR_CLAIM", &serde_json::json!({
                                            "claimed": claimed,
                                            "tokenIds": token_ids,
                                            "totalPayoutUsd": payout,
                                            "newBalanceUsd": new_bal,
                                            "txHash": tx,
                                            "context": "main_loop"
                                        }));
                                        ledger_write("SAFETY_CLAIM", &serde_json::json!({
                                            "claimed_count": claimed, "payout_usd": payout,
                                            "new_balance": new_bal, "context": "main_loop"
                                        }));
                                    } else if claimed > 0 {
                                        info!("💤 Duplicate main-loop safety claim ignored: claimed={} payout=${:.2} tx={} tokens={:?}",
                                            claimed, payout, tx, token_ids);
                                    }
                                }
                                // "nothing" status = no redeemable bets, silent
                            }
                        }
                        Err(e) => warn!("Auto-claim safety net error: {}", e),
                    }
                }

                match client.get(format!("{}/my-bets", executor_url)).send().await {
                    Ok(resp) => {
                        if let Ok(mb) = resp.json::<serde_json::Value>().await {
                            if let Some(bets_arr) = mb.get("bets").and_then(|v| v.as_array()) {
                                inflight_wagered_total = reconcile_active_bets_with_executor_snapshot(
                                    &mut active_bets,
                                    bets_arr,
                                    pending_claims_path,
                                    session_start,
                                    INFLIGHT_TTL_SECS,
                                );
                            }
                        }
                    }
                    Err(e) => warn!("Pending reconcile /my-bets error: {}", e),
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
                    // executor returns "alreadyPaid" not "won" — already_paid = settled+claimed wins
                    let won = mb.get("alreadyPaid").and_then(|v| v.as_u64()).unwrap_or(0);
                    let lost = mb.get("lost").and_then(|v| v.as_u64()).unwrap_or(0);
                    let pending_sg = mb.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
                    let redeemable = mb.get("claimable").and_then(|v| v.as_u64()).unwrap_or(0);
                    let src = mb.get("source").and_then(|v| v.as_str()).unwrap_or("?");
                    msg.push_str(&format!(
                        "📋 Bety na Azuro ({}):\n\
                         \u{2022} Celkem: {} | Won: {} | Lost: {} | Pending: {}\n\
                         \u{2022} Vyplatitelné: <b>{}</b>\n",
                        src, total, won, lost, pending_sg, redeemable
                    ));
                    if redeemable > 0 {
                        msg.push_str("⚠️ <b>Nevybráno!</b> Pošlu /auto-claim...\n");
                    }

                    // === RECONCILE: on-chain pending is source of truth for locked exposure ===
                    if let Some(bets_arr) = mb.get("bets").and_then(|v| v.as_array()) {
                        inflight_wagered_total = reconcile_active_bets_with_executor_snapshot(
                            &mut active_bets,
                            bets_arr,
                            pending_claims_path,
                            session_start,
                            INFLIGHT_TTL_SECS,
                        );
                    }
                }

                // === SPLIT DISPLAY: On-chain (truth) vs In-flight (unconfirmed) ===
                let onchain_bets: Vec<&ActiveBet> = active_bets.iter()
                    .filter(|b| b.token_id.is_some())
                    .collect();
                let inflight_bets: Vec<&ActiveBet> = active_bets.iter()
                    .filter(|b| b.token_id.is_none())
                    .collect();

                if onchain_bets.is_empty() && inflight_bets.is_empty() {
                    msg.push_str("🎰 Pending sázek: 0\n");
                } else {
                    // On-chain verified pending (truth)
                    if !onchain_bets.is_empty() {
                        let total_onchain: f64 = onchain_bets.iter().map(|b| b.amount_usd).sum();
                        msg.push_str(&format!("🎰 Pending sázek: <b>{}</b> (ve hře: ${:.2})\n",
                            onchain_bets.len(), total_onchain));
                        for b in &onchain_bets {
                            msg.push_str(&format!("  \u{2022} {} @ {:.2} ${:.2}\n", b.value_team, b.odds, b.amount_usd));
                        }
                    }
                    // In-flight unconfirmed (not yet on-chain, NOT counted in exposure)
                    if !inflight_bets.is_empty() {
                        msg.push_str(&format!("✈️ In-flight (neověřeno): <b>{}</b>\n", inflight_bets.len()));
                        for b in &inflight_bets {
                            let age_str = match chrono::DateTime::parse_from_rfc3339(&b.placed_at) {
                                Ok(placed) => {
                                    let secs = (Utc::now() - placed.with_timezone(&Utc)).num_seconds();
                                    format!("{}s ago", secs)
                                }
                                Err(_) => b.placed_at.clone(),
                            };
                            msg.push_str(&format!("  \u{2022} {} @ {:.2} ${:.2} ({})\n",
                                b.value_team, b.odds, b.amount_usd, age_str));
                        }
                    }
                }

                // Daily P&L (persisted across restarts)
                let daily_pnl = daily_returned - daily_wagered;
                let (pnl_sign, pnl_emoji) = if daily_pnl >= 0.0 { ("+", "📈") } else { ("", "📉") };
                msg.push_str(&format!("\n{} Daily P/L: <b>{}{:.2} USDT</b>\n", pnl_emoji, pnl_sign, daily_pnl));
                msg.push_str(&format!("   Vsazeno: ${:.2} | Vráceno: ${:.2}\n", daily_wagered, daily_returned));
                let daily_net_loss = (daily_wagered - daily_returned).max(0.0);
                let effective_daily_limit = {
                    let (_, _, _, dl_frac, _) = get_exposure_caps(start_of_day_bankroll);
                    daily_limit_override.unwrap_or_else(|| DAILY_LOSS_LIMIT_USD.min(start_of_day_bankroll * dl_frac))
                };
                let lim_override_tag = if daily_limit_override.is_some() { " ⚡" } else { "" };
                msg.push_str(&format!("   Loss limit: ${:.2} / ${:.2}{}\n", daily_net_loss, effective_daily_limit, lim_override_tag));
                msg.push_str(&format!("   Auto-bets dnes: {}\n", auto_bet_count));

                // WS gate diagnostics
                {
                    let cache_r = ws_condition_cache.read().await;
                    let cache_size = cache_r.len();
                    let newest_age_ms = cache_r.values()
                        .map(|e| e.updated_at.elapsed().as_millis())
                        .min()
                        .unwrap_or(0);
                    let ws_status = if !ws_state_gate_enabled {
                        "OFF".to_string()
                    } else if cache_size == 0 {
                        "ON (no data yet)".to_string()
                    } else {
                        format!("ON ({} conds, newest {}ms)", cache_size, newest_age_ms)
                    };
                    msg.push_str(&format!("\n🔌 WS gate: {}\n", ws_status));
                    msg.push_str(&format!("  ✅ active={} | 🚫 not_active={} | ⏳ stale={} | ❓ nodata={}\n",
                        ws_gate_active_count, ws_gate_not_active_count,
                        ws_gate_stale_fallback_count, ws_gate_nodata_fallback_count));
                }

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
                                    // Show on-chain bets (real-time) with enriched metadata
                                    let mut bets_msg = String::from("🎰 <b>SÁZKY</b>\n\n");

                                    // On-chain bets from executor
                                    match client.get(format!("{}/my-bets", executor_url)).send().await {
                                        Ok(resp) => {
                                            match resp.json::<serde_json::Value>().await {
                                                Ok(mb) => {
                                                    let total = mb.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    // executor returns "alreadyPaid" not "won"
                                                    let won = mb.get("alreadyPaid").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let lost = mb.get("lost").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let pending_sg = mb.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let claimable_count = mb.get("claimable").and_then(|v| v.as_u64()).unwrap_or(0);
                                                    let claimable_usd = mb.get("claimableUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                                    let src = mb.get("source").and_then(|v| v.as_str()).unwrap_or("?");
                                                    bets_msg.push_str(&format!(
                                                        "📊 <b>Azuro</b> ({}):\n\
                                                         Celkem: {} | ✅ Won: {} | ❌ Lost: {} | ⏳ Pending: {}\n\
                                                         💰 Claimable: <b>{}</b> (${:.2})\n\n",
                                                        src, total, won, lost, pending_sg, claimable_count, claimable_usd
                                                    ));
                                                    if let Some(bets_arr) = mb.get("bets").and_then(|v| v.as_array()) {
                                                        // Show pending bets with enriched team/odds
                                                        let pending_bets: Vec<&serde_json::Value> = bets_arr.iter()
                                                            .filter(|b| {
                                                                let st = b.get("status").and_then(|v| v.as_str()).unwrap_or("");
                                                                st == "pending" || st == "claimable"
                                                            })
                                                            .collect();
                                                        if !pending_bets.is_empty() {
                                                            bets_msg.push_str(&format!("📋 <b>Pending/Claimable ({}):</b>\n", pending_bets.len()));
                                                            for b in pending_bets.iter().take(40) {
                                                                let tid = b.get("tokenId").and_then(|v| v.as_str()).unwrap_or("?");
                                                                let status = b.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                                                                let team = b.get("team").and_then(|v| v.as_str()).unwrap_or("");
                                                                let odds = b.get("odds").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                                                let amount = b.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                                                let payout = b.get("payoutUsd").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                                                let emoji = if status == "claimable" { "💰" } else { "⏳" };
                                                                if !team.is_empty() {
                                                                    bets_msg.push_str(&format!(
                                                                        "{} {} @ {:.2} ${:.2}{}\n",
                                                                        emoji, team, odds, amount,
                                                                        if payout > 0.0 { format!(" → ${:.2}", payout) } else { String::new() }
                                                                    ));
                                                                } else {
                                                                    bets_msg.push_str(&format!(
                                                                        "{} #{}{}\n",
                                                                        emoji, &tid[..tid.len().min(8)],
                                                                        if payout > 0.0 { format!(" → ${:.2}", payout) } else { String::new() }
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(_) => bets_msg.push_str("⚠️ Parse error\n\n"),
                                            }
                                        }
                                        Err(_) => bets_msg.push_str("❌ Executor offline\n\n"),
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
                                                    let token_ids: Vec<String> = cr.get("tokenIds")
                                                        .and_then(|v| v.as_array())
                                                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                                                        .unwrap_or_default();
                                                    let new_claim_tokens: Vec<String> = token_ids.iter()
                                                        .filter(|tid| !claimed_token_ids.contains(*tid))
                                                        .cloned()
                                                        .collect();
                                                    let is_new_tx = !tx.is_empty() && claimed_tx_hashes.insert(tx.to_string());
                                                    let should_count_claim = claimed > 0 && (!new_claim_tokens.is_empty() || is_new_tx);
                                                    for tid in &token_ids {
                                                        claimed_token_ids.insert(tid.clone());
                                                    }
                                                    if status == "ok" && should_count_claim {
                                                        total_returned += payout;
                                                        daily_returned += payout;
                                                        {
                                                            let today = Utc::now().format("%Y-%m-%d").to_string();
                                                            let _ = std::fs::write("data/daily_pnl.json",
                                                                serde_json::json!({"date": today, "wagered": daily_wagered, "returned": daily_returned, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
                                                        }
                                                        ledger_write("EXECUTOR_CLAIM", &serde_json::json!({
                                                            "claimed": claimed,
                                                            "tokenIds": token_ids,
                                                            "totalPayoutUsd": payout,
                                                            "newBalanceUsd": new_bal,
                                                            "txHash": tx,
                                                            "context": "manual_command"
                                                        }));
                                                    }
                                                    let msg_text = if status == "ok" {
                                                        if should_count_claim {
                                                            format!("✅ <b>Claim hotovo!</b>\nVyplaceno: {} sázek, ${:.2}\nNový balance: {} USDT\nTX: <code>{}</code>",
                                                                claimed, payout, new_bal, tx)
                                                        } else {
                                                            format!("ℹ️ <b>Claim už byl započítán</b>\nVyplaceno: {} sázek, ${:.2}\nBalance: {} USDT\nTX: <code>{}</code>",
                                                                claimed, payout, new_bal, tx)
                                                        }
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

                                } else if text.starts_with("/limit") {
                                    let arg = text.trim_start_matches("/limit").trim();
                                    let delta_str = arg.trim_start_matches('+').trim();
                                    if let Ok(delta) = delta_str.parse::<f64>() {
                                        if delta <= 0.0 || delta > 500.0 {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                "❌ Delta musí být 0–500 USD. Příklad: /limit +10"
                                            ).await;
                                        } else {
                                            let base = DAILY_LOSS_LIMIT_USD;
                                            let new_limit = base + delta;
                                            daily_limit_override = Some(new_limit);
                                            daily_loss_alert_sent = false;
                                            daily_loss_last_reminder = None;
                                            let net_now = (daily_wagered - daily_returned).max(0.0);
                                            let room = (new_limit - net_now).max(0.0);
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "📈 <b>DAILY LIMIT NAVÝŠEN</b>\n\n\
                                                     Base: ${:.0} + přidáno ${:.0} = <b>${:.0}</b>\n\
                                                     Net loss dnes: ${:.2}\n\
                                                     Zbývá room: <b>${:.2}</b>\n\n\
                                                     ✅ Auto-bety odblokované.",
                                                    base, delta, new_limit, net_now, room
                                                )
                                            ).await;
                                            info!("📈 /limit +{:.0} → override={:.0} (room={:.2})", delta, new_limit, room);
                                            ledger_write("LIMIT_OVERRIDE", &serde_json::json!({
                                                "base": base, "delta": delta,
                                                "new_limit": new_limit,
                                                "net_loss_now": net_now,
                                                "room": room,
                                                "trigger": "manual_command"
                                            }));
                                        }
                                    } else {
                                        let cur_lim = daily_limit_override.unwrap_or(DAILY_LOSS_LIMIT_USD);
                                        let net_now = (daily_wagered - daily_returned).max(0.0);
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!("❌ Syntax: /limit +10\nAktuální limit: ${:.0}{}\nNet loss dnes: ${:.2}",
                                                cur_lim,
                                                if daily_limit_override.is_some() { " ⚡" } else { "" },
                                                net_now)
                                        ).await;
                                    }

                                } else if text == "/reset_daily" || text == "/resetdaily" {
                                    let old_w = daily_wagered;
                                    let old_r = daily_returned;
                                    let old_net = (old_w - old_r).max(0.0);
                                    daily_wagered = 0.0;
                                    daily_returned = 0.0;
                                    daily_loss_alert_sent = false;
                                    daily_loss_last_reminder = None;
                                    daily_limit_override = None; // reset override on full daily reset
                                    {
                                        let today = Utc::now().format("%Y-%m-%d").to_string();
                                        let _ = std::fs::write("data/daily_pnl.json",
                                            serde_json::json!({"date": today, "wagered": 0.0, "returned": 0.0, "sod_bankroll": start_of_day_bankroll, "limit_override": daily_limit_override}).to_string());
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

                                    // On-chain summary + reconcile
                                    if let Ok(resp) = client.get(format!("{}/my-bets", executor_url)).send().await {
                                        if let Ok(mb) = resp.json::<serde_json::Value>().await {
                                            let total = mb.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                                            // executor returns "alreadyPaid" (settled+claimed wins); keep fallback for older schemas
                                            let won = mb.get("alreadyPaid")
                                                .and_then(|v| v.as_u64())
                                                .or_else(|| mb.get("won").and_then(|v| v.as_u64()))
                                                .unwrap_or(0);
                                            let lost = mb.get("lost").and_then(|v| v.as_u64()).unwrap_or(0);
                                            let pending_count = mb.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
                                            let redeemable = mb.get("claimable").and_then(|v| v.as_u64()).unwrap_or(0);
                                            msg.push_str(&format!(
                                                "\n📋 Azuro bety: {} total | ✅{} ❌{} | ⏳{} | 💰 Claim: {}\n",
                                                total, won, lost, pending_count, redeemable
                                            ));

                                            // Reconcile active_bets from on-chain pending data (same as ticker)
                                            if let Some(bets_arr) = mb.get("bets").and_then(|v| v.as_array()) {
                                                inflight_wagered_total = reconcile_active_bets_with_executor_snapshot(
                                                    &mut active_bets,
                                                    bets_arr,
                                                    pending_claims_path,
                                                    session_start,
                                                    INFLIGHT_TTL_SECS,
                                                );
                                            }
                                        }
                                    }

                                    // Split display: on-chain vs in-flight
                                    let onchain_bets: Vec<&ActiveBet> = active_bets.iter()
                                        .filter(|b| b.token_id.is_some())
                                        .collect();
                                    let inflight_bets_view: Vec<&ActiveBet> = active_bets.iter()
                                        .filter(|b| b.token_id.is_none())
                                        .collect();

                                    if onchain_bets.is_empty() && inflight_bets_view.is_empty() {
                                        msg.push_str("\n🎰 Žádné pending sázky\n");
                                    } else {
                                        if !onchain_bets.is_empty() {
                                            let total_onchain: f64 = onchain_bets.iter().map(|b| b.amount_usd).sum();
                                            msg.push_str(&format!("\n🎰 <b>On-chain pending ({})</b> — ve hře: ${:.2}\n",
                                                onchain_bets.len(), total_onchain));
                                            for b in &onchain_bets {
                                                msg.push_str(&format!("  \u{2022} {} @ {:.2} ${:.2}\n", b.value_team, b.odds, b.amount_usd));
                                            }
                                        }
                                        if !inflight_bets_view.is_empty() {
                                            msg.push_str(&format!("\n✈️ <b>In-flight ({})</b> — neověřeno on-chain\n",
                                                inflight_bets_view.len()));
                                            for b in &inflight_bets_view {
                                                let age_str = match chrono::DateTime::parse_from_rfc3339(&b.placed_at) {
                                                    Ok(placed) => {
                                                        let secs = (Utc::now() - placed.with_timezone(&Utc)).num_seconds();
                                                        format!("{}s ago", secs)
                                                    }
                                                    Err(_) => b.placed_at.clone(),
                                                };
                                                msg.push_str(&format!("  \u{2022} {} @ {:.2} ${:.2} ({})\n",
                                                    b.value_team, b.odds, b.amount_usd, age_str));
                                            }
                                        }
                                    }

                                    // WS gate diagnostics
                                    {
                                        let cache_r = ws_condition_cache.read().await;
                                        let cache_size = cache_r.len();
                                        let newest_age_ms = cache_r.values()
                                            .map(|e| e.updated_at.elapsed().as_millis())
                                            .min()
                                            .unwrap_or(0);
                                        let ws_status = if !ws_state_gate_enabled {
                                            "OFF".to_string()
                                        } else if cache_size == 0 {
                                            "ON (no data)".to_string()
                                        } else {
                                            format!("ON ({} conds, newest {}ms)", cache_size, newest_age_ms)
                                        };
                                        msg.push_str(&format!("\n🔌 WS gate: {}\n", ws_status));
                                        msg.push_str(&format!("  ✅ active={} | 🚫 not_active={} | ⏳ stale={} | ❓ nodata={}\n",
                                            ws_gate_active_count, ws_gate_not_active_count,
                                            ws_gate_stale_fallback_count, ws_gate_nodata_fallback_count));
                                    }

                                    let daily_pnl = daily_returned - daily_wagered;
                                    let (pnl_sign, pnl_emoji) = if daily_pnl >= 0.0 { ("+", "📈") } else { ("", "📉") };
                                    msg.push_str(&format!("\n{} <b>Daily P/L: {}{:.2} USDT</b>\n", pnl_emoji, pnl_sign, daily_pnl));
                                    msg.push_str(&format!("Vsazeno: ${:.2} | Vráceno: ${:.2}\n", daily_wagered, daily_returned));
                                    let daily_net_loss = (daily_wagered - daily_returned).max(0.0);
                                    let effective_daily_limit = {
                                        let (_, _, _, dl_frac, _) = get_exposure_caps(start_of_day_bankroll);
                                        daily_limit_override.unwrap_or_else(|| DAILY_LOSS_LIMIT_USD.min(start_of_day_bankroll * dl_frac))
                                    };
                                    let lim_override_tag2 = if daily_limit_override.is_some() { " ⚡override" } else { "" };
                                    msg.push_str(&format!("Loss limit: ${:.2} / ${:.2}{}\n", daily_net_loss, effective_daily_limit, lim_override_tag2));
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

                                        let esports_meta = classify_esports_family(
                                            &anomaly.match_key,
                                            None,
                                            None,
                                            &anomaly.team1,
                                            &anomaly.team2,
                                        );
                                        if BLOCK_GENERIC_ESPORTS_BETS
                                            && anomaly.match_key.starts_with("esports::")
                                            && (esports_meta.family.is_none()
                                                || matches!(esports_meta.confidence, "low" | "unknown")) {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "🛑 <b>MANUAL BET BLOCKED</b>\n\nAlert #{} má neověřenou esports identitu. family={} confidence={} reason={}",
                                                    aid,
                                                    esports_meta.family.unwrap_or("unknown"),
                                                    esports_meta.confidence,
                                                    esports_meta.reason,
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

                                        // Manual dedup guard (same as auto-bet protection):
                                        // don't allow re-betting same condition/base match from stale alert messages.
                                        let manual_base_match_key = strip_map_winner_suffix(&anomaly.match_key);
                                        let manual_dedup_base = already_bet_base_matches.contains(&manual_base_match_key)
                                            && anomaly.match_key != manual_base_match_key;
                                        let manual_scoped_condition = scoped_condition_key(&manual_base_match_key, &condition_id);
                                        let manual_dedup = already_bet_conditions.contains(&manual_scoped_condition)
                                            || already_bet_matches.contains(&anomaly.match_key)
                                            || manual_dedup_base
                                            || inflight_conditions.contains(&manual_scoped_condition)
                                            || inflight_conditions.contains(&anomaly.match_key);
                                        if manual_dedup {
                                            let _ = tg_send_message(&client, &token, chat_id,
                                                &format!(
                                                    "🚫 <b>MANUAL BET BLOCKED (DEDUP)</b>\n\nAlert #{}\n{}\ncondition {} je už vsazená / in-flight.",
                                                    aid, anomaly.match_key, condition_id
                                                )
                                            ).await;
                                            continue;
                                        }

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
                                        let (min_odds, _min_odds_display_cmd) = compute_min_odds_raw(azuro_odds, min_odds_factor_for_match(&anomaly.match_key));
                                        let amount_raw = (amount * 1e6) as u64; // USDT 6 decimals

                                        let bet_body = serde_json::json!({
                                            "conditionId": condition_id,
                                            "outcomeId": outcome_id,
                                            "amount": amount_raw.to_string(),
                                            "minOdds": min_odds.to_string(),
                                            "requestedOdds": azuro_odds,
                                            "matchKey": anomaly.match_key,
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
                                                            let accepted_odds = br.accepted_odds.unwrap_or(azuro_odds);

                                                            let is_dry_run = state == "DRY-RUN" || bet_id.starts_with("dry-");

                                                            // Don't track dry-run bets as active
                                                            if !is_dry_run {
                                                                active_bets.push(ActiveBet {
                                                                    alert_id: aid,
                                                                    bet_id: bet_id.to_string(),
                                                                    match_key: anomaly.match_key.clone(),
                                                                    market_key: anomaly.market_key.clone(),
                                                                    team1: anomaly.team1.clone(),
                                                                    team2: anomaly.team2.clone(),
                                                                    value_team: value_team.to_string(),
                                                                    amount_usd: amount,
                                                                    odds: accepted_odds,
                                                                    placed_at: Utc::now().to_rfc3339(),
                                                                    condition_id: condition_id.clone(),
                                                                    outcome_id: outcome_id.clone(),
                                                                    graph_bet_id: graph_bet_id_opt.clone(),
                                                                    token_id: token_id_opt.clone(),
                                                                    path: "bet_command".to_string(),
                                                                });

                                                                let token_to_write = token_id_opt.as_deref().unwrap_or("?");
                                                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                                                    .create(true).append(true)
                                                                    .open(pending_claims_path) {
                                                                    use std::io::Write;
                                                                    let _ = writeln!(f, "{}|{}|{}|{}|{}|{}|{}",
                                                                        token_to_write,
                                                                        bet_id, anomaly.match_key,
                                                                        value_team, amount, accepted_odds,
                                                                        Utc::now().to_rfc3339());
                                                                }
                                                                // === LEDGER: BET PLACED (bet-command) ===
                                                                ledger_write("PLACED", &serde_json::json!({
                                                                    "alert_id": aid, "bet_id": bet_id,
                                                                    "match_key": anomaly.match_key,
                                                                    "market_key": anomaly.market_key,
                                                                    "team1": anomaly.team1, "team2": anomaly.team2,
                                                                    "value_team": value_team,
                                                                    "amount_usd": amount, "odds": accepted_odds,
                                                                    "requested_odds": azuro_odds,
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
                                                                    aid, value_team, accepted_odds, amount,
                                                                    bet_id, state, CASHOUT_MIN_PROFIT_PCT
                                                                )
                                                            };

                                                            let _ = tg_send_message(&client, &token, chat_id, &msg).await;

                                                            // === FOLLOW-UP: Poll Created bets to detect async Rejected ===
                                                            // Azuro relayer returns State:Created immediately,
                                                            // but on-chain tx can revert 10-30s later → Rejected.
                                                            // Without this follow-up, user thinks bet went through.
                                                            if !is_dry_run && (state == "Created" || state == "Pending") {
                                                                let follow_client = client.clone();
                                                                let follow_token = token.clone();
                                                                let follow_executor = executor_url.clone();
                                                                let follow_bet_id = bet_id.to_string();
                                                                let follow_aid = aid;
                                                                let follow_team = value_team.to_string();
                                                                let follow_chat = chat_id;
                                                                tokio::spawn(async move {
                                                                    // Wait for on-chain confirmation
                                                                    tokio::time::sleep(Duration::from_secs(20)).await;
                                                                    if let Ok(resp) = follow_client.get(
                                                                        format!("{}/bet/{}", follow_executor, follow_bet_id)
                                                                    ).send().await {
                                                                        if let Ok(br) = resp.json::<serde_json::Value>().await {
                                                                            let final_state = br.get("state")
                                                                                .and_then(|v| v.as_str()).unwrap_or("?");
                                                                            let err_msg = br.get("errorMessage")
                                                                                .and_then(|v| v.as_str()).unwrap_or("");
                                                                            if final_state == "Rejected" || final_state == "Failed" || final_state == "Cancelled" {
                                                                                let alert = format!(
                                                                                    "❌ <b>BET #{} REJECTED (follow-up)</b>\n\n\
                                                                                     {} — transakce reverted on-chain.\n\
                                                                                     Error: {}\n\
                                                                                     💰 Peníze nebyly strženy.",
                                                                                    follow_aid, follow_team, err_msg);
                                                                                let _ = tg_send_message(
                                                                                    &follow_client, &follow_token,
                                                                                    follow_chat, &alert).await;
                                                                                warn!("❌ BET #{} FOLLOW-UP REJECTED: {} err={}",
                                                                                    follow_aid, follow_bet_id, err_msg);
                                                                            } else if final_state == "Accepted" {
                                                                                let token_id = br.get("tokenId")
                                                                                    .and_then(|v| v.as_str()).unwrap_or("?");
                                                                                info!("✅ BET #{} FOLLOW-UP CONFIRMED: state={} tokenId={}",
                                                                                    follow_aid, final_state, token_id);
                                                                            } else {
                                                                                info!("⏳ BET #{} FOLLOW-UP: state={} (still pending)",
                                                                                    follow_aid, final_state);
                                                                            }
                                                                        }
                                                                    }
                                                                });
                                                            }
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
