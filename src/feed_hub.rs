//! feed-hub — WS ingest pro headful browser/Android feeds + Azuro GraphQL poller
//!
//! Cíl: přijímat realtime JSON z Lenovo (Tampermonkey) / Zebra (Android) a v Rustu
//! udržovat „co je LIVE" + „kde jsou LIVE odds", s gatingem a audit logy.
//! Navíc: periodicky polluje Azuro Protocol (The Graph) pro CS2 on-chain odds.
//!
//! Spuštění:
//!   $env:FEED_HUB_BIND="0.0.0.0:8080"; cargo run --bin feed-hub
//!
//! Tampermonkey (příklad):
//!   const ws = new WebSocket('ws://10.107.109.85:8080/feed');
//!   ws.send(JSON.stringify({v:1, type:'live_match', source:'hltv', ...}))

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use logger::EventLogger;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

mod feed_db;
mod azuro_poller;
use feed_db::{
    spawn_db_writer,
    DbConfig,
    DbFusionRow,
    DbHeartbeatRow,
    DbIngestRow,
    DbLiveRow,
    DbMsg,
    DbOddsRow,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedMessageType {
    LiveMatch,
    Odds,
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEnvelope {
    pub v: u32,
    #[serde(rename = "type")]
    pub msg_type: FeedMessageType,
    pub source: String,
    pub ts: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveMatchPayload {
    pub sport: String,
    pub team1: String,
    pub team2: String,
    pub score1: Option<i64>,
    pub score2: Option<i64>,
    pub status: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OddsPayload {
    pub sport: String,
    pub bookmaker: String,
    pub market: String,
    pub team1: String,
    pub team2: String,

    pub odds_team1: f64,
    pub odds_team2: f64,

    /// Odhadovaná likvidita v USD (nebo ekvivalent) — pro gating
    pub liquidity_usd: Option<f64>,
    /// Spread v procentech (např. 1.2 znamená 1.2%) — pro gating
    pub spread_pct: Option<f64>,

    pub url: Option<String>,

    // === Azuro execution data (pro BUY + cashout) ===
    /// Azuro game ID (subgraph)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    /// Azuro condition ID (pro bet placement)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition_id: Option<String>,
    /// Azuro outcome ID pro team1 win
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome1_id: Option<String>,
    /// Azuro outcome ID pro team2 win
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome2_id: Option<String>,
    /// Chain name (polygon, gnosis, base, chiliz)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
}

#[derive(Debug, Clone)]
struct LiveMatchState {
    source: String,
    seen_at: DateTime<Utc>,
    payload: LiveMatchPayload,
}

#[derive(Debug, Clone)]
struct OddsState {
    source: String,
    seen_at: DateTime<Utc>,
    payload: OddsPayload,
}

#[derive(Debug, Clone, Serialize)]
struct FeedIngestEvent {
    ts: String,
    event: &'static str,
    source: String,
    msg_type: String,
    ok: bool,
    note: String,
}

#[derive(Debug, Clone, Serialize)]
struct LiveFusionReadyEvent {
    ts: String,
    event: &'static str, // "LIVE_FUSION_READY"
    sport: String,
    match_key: String,
    live_source: String,
    odds_source: String,
    bookmaker: String,
    market: String,
    liquidity_usd: Option<f64>,
    spread_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct FeedHeartbeatEvent {
    ts: String,
    event: &'static str, // "FEED_HUB_HEARTBEAT"
    connections: usize,
    live_items: usize,
    odds_items: usize,
    fused_ready: usize,
}

/// Key for multi-bookmaker odds: match_key + bookmaker
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct OddsKey {
    match_key: String,
    bookmaker: String,
}

#[derive(Clone)]
struct FeedHubState {
    live: Arc<RwLock<HashMap<String, LiveMatchState>>>,
    odds: Arc<RwLock<HashMap<OddsKey, OddsState>>>,
    connections: Arc<RwLock<usize>>,
}

impl FeedHubState {
    fn new() -> Self {
        Self {
            live: Arc::new(RwLock::new(HashMap::new())),
            odds: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(0)),
        }
    }
}

/// For tennis: extract SURNAME portion for cross-platform matching.
/// FlashScore format: "Blanchet U." → "blanchet"
/// Azuro format: "Ugo Blanchet" → "blanchet"
/// Handles particles: "De Stefano S." → "destefano", "Samira De Stefano" → "destefano"
fn normalize_tennis_name(name: &str) -> String {
    let name = name.trim();
    let parts: Vec<&str> = name.split_whitespace().collect();

    if parts.len() <= 1 {
        // Single word — just lowercase + alphanumeric
        return name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    }

    // Detect FlashScore format: last part is an INITIAL (has period)
    // e.g. "Blanchet U.", "De Stefano S.", "Rakhimova K."
    // Must contain period — "Ce" (2-letter surname) is NOT an initial!
    let last = parts.last().unwrap();
    let last_clean: String = last.chars().filter(|c| c.is_alphanumeric()).collect();
    let is_initial = (last.contains('.') && last_clean.len() <= 2)
        || last_clean.len() <= 1;

    if is_initial && parts.len() >= 2 {
        // FlashScore format: surname = all parts except last (initial)
        let surname: String = parts[..parts.len() - 1]
            .join("")
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        return surname;
    }

    // Azuro format: "Firstname [particles] Surname"
    // Take last word + any preceding particles (de, van, von, da, di, del, ...)
    let particles = ["de", "van", "von", "da", "di", "del", "le", "la", "el", "al", "bin", "mc"];
    let mut surname_start = parts.len() - 1;
    while surname_start > 1 {
        let prev: String = parts[surname_start - 1]
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        if particles.contains(&prev.as_str()) {
            surname_start -= 1;
        } else {
            break;
        }
    }

    let surname: String = parts[surname_start..]
        .join("")
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    surname
}

fn normalize_name(name: &str) -> String {
    // Strip ALL non-alphanumeric chars so "Thunder Downunder" == "THUNDERdOWNUNDER"
    let mut s: String = name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();

    // Strip common prefixes that differ between sources
    // HLTV: "Nemesis", Azuro: "Team Nemesis" → both → "nemesis"
    // Also: "Clan X" vs "X", "FC X" vs "X"
    let prefixes = ["team", "clan", "fc", "pro", "cf", "ac", "as", "cd", "rc", "rcd", "sd", "ud"];
    for prefix in &prefixes {
        if s.len() > prefix.len() + 2 && s.starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            break;
        }
    }

    // Strip common suffixes that differ between sources
    // "Newells Old Boys" → "newells", "Celtic FC" → "celtic", "Corinthians SP" → "corinthians"
    // "Corinthians MG" → "corinthians", "Flamengo RJ" → "flamengo"
    let suffixes = ["gaming", "esports", "esport", "gg", "club", "org",
                    "academy", "rising", "fe",
                    // Club suffixes
                    "fc", "cf", "sc", "ac",
                    // Brazilian state abbreviations (appear in Azuro names)
                    "sp", "mg", "rj", "rs", "ba", "pr", "ce", "go", "pe",
                    // Other common suffixes
                    "oldboys", "united", "city", "wanderers",
                    // Azuro specific
                    "whitecapsfc", "whitecaps"];
    for suffix in &suffixes {
        if s.len() > suffix.len() + 3 && s.ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            break;
        }
    }

    // Strip trailing digits that some sources append (e.g. team name duplicates)
    while s.len() > 3 {
        if let Some(last) = s.chars().last() {
            if last.is_ascii_digit() {
                s.pop();
            } else {
                break;
            }
        } else {
            break;
        }
    }

    s
}

fn match_key(sport: &str, team1: &str, team2: &str) -> String {
    let sport_lower = sport.to_lowercase();
    // Tennis uses surname-only matching (FlashScore "Blanchet U." ↔ Azuro "Ugo Blanchet")
    let (a, b) = if sport_lower == "tennis" {
        (normalize_tennis_name(team1), normalize_tennis_name(team2))
    } else {
        (normalize_name(team1), normalize_name(team2))
    };
    // Sort alphabetically so team order doesn't matter for matching
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    format!("{}::{}_vs_{}", sport_lower, first, second)
}

fn parse_ts(ts: &Option<String>) -> DateTime<Utc> {
    ts.as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn gate_odds(odds: &OddsPayload, seen_at: DateTime<Utc>) -> (bool, String) {
    // null = ok (Azuro doesn't always send liquidity/spread)
    let liquidity_ok = odds.liquidity_usd.map_or(true, |l| l >= 500.0);
    let spread_ok = odds.spread_pct.map_or(true, |s| s <= 5.0);

    let age = Utc::now().signed_duration_since(seen_at);
    let stale_ok = age.num_seconds().abs() <= 10;

    if !liquidity_ok {
        return (false, "liquidity<2000".to_string());
    }
    if !spread_ok {
        return (false, "spread>1.5%".to_string());
    }
    if !stale_ok {
        return (false, "stale>10s".to_string());
    }

    (true, "ok".to_string())
}

#[derive(Debug, Clone, Serialize)]
struct HttpLiveItem {
    match_key: String,
    source: String,
    seen_at: String,
    payload: LiveMatchPayload,
}

#[derive(Debug, Clone, Serialize)]
struct HttpOddsItem {
    match_key: String,
    source: String,
    seen_at: String,
    payload: OddsPayload,
}

#[derive(Debug, Clone, Serialize)]
struct HttpStateResponse {
    ts: String,
    connections: usize,
    live_items: usize,
    odds_items: usize,
    fused_ready: usize,
    fused_keys: Vec<String>,
    live: Vec<HttpLiveItem>,
    odds: Vec<HttpOddsItem>,
}

// ====================================================================
// OPPORTUNITIES — value/arb detection
// ====================================================================

#[derive(Debug, Clone, Serialize)]
struct Opportunity {
    match_key: String,
    /// "value_bet" | "score_momentum" | "arb_cross_book"
    opp_type: String,
    team1: String,
    team2: String,
    score: String,
    /// Which team has value: 1 or 2
    value_side: u8,
    /// Description of the signal
    signal: String,
    /// Confidence 0.0..1.0
    confidence: f64,
    /// The odds that represent value
    odds: f64,
    /// Implied probability from odds
    implied_prob_pct: f64,
    /// Our estimated fair probability based on score
    estimated_fair_pct: f64,
    /// Edge = estimated_fair - implied (positive = value)
    edge_pct: f64,
    bookmaker: String,
    odds_age_secs: i64,
    live_age_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
struct OpportunitiesResponse {
    ts: String,
    total_live: usize,
    total_odds: usize,
    fused_matches: usize,
    opportunities: Vec<Opportunity>,
}

async fn build_opportunities(state: &FeedHubState) -> OpportunitiesResponse {
    let live_map = state.live.read().await;
    let odds_map = state.odds.read().await;
    let now = Utc::now();

    let total_live = live_map.len();
    let total_odds = odds_map.len();

    // Group odds by match_key
    let mut odds_by_match: HashMap<&str, Vec<&OddsState>> = HashMap::new();
    for (ok, ov) in odds_map.iter() {
        odds_by_match.entry(&ok.match_key).or_default().push(ov);
    }

    let fused_matches = odds_by_match.keys()
        .filter(|k| live_map.contains_key(**k))
        .count();

    let mut opportunities = Vec::new();

    for (match_key, live) in live_map.iter() {
        // Try alternate sport prefixes for ANY live key that doesn't match Azuro directly.
        // FlashScore/Tipsport may label a match as 'esports' while Azuro uses 'cs2',
        // 'dota-2', 'basketball', 'football' etc.
        // Also: Tipsport 'basketball'/'football' live won't match if Azuro key differs slightly.
        let esports_alts: &[&str] = &[
            "cs2", "dota-2", "league-of-legends", "valorant",
            "basketball", "football", "mma", "starcraft",
        ];
        let odds_list_opt = odds_by_match.get(match_key.as_str())
            .or_else(|| {
                if match_key.starts_with("esports::") {
                    let tail = &match_key["esports::".len()..];
                    esports_alts.iter().find_map(|alt| {
                        let alt_key = format!("{}::{}", alt, tail);
                        odds_by_match.get(alt_key.as_str())
                    })
                } else {
                    None
                }
            });
        let Some(odds_list) = odds_list_opt else {
            continue;
        };

        let score1 = live.payload.score1.unwrap_or(0);
        let score2 = live.payload.score2.unwrap_or(0);
        let score_str = format!("{}-{}", score1, score2);
        let live_age = now.signed_duration_since(live.seen_at).num_seconds();

        for odds_state in odds_list {
            let odds = &odds_state.payload;
            let odds_age = now.signed_duration_since(odds_state.seen_at).num_seconds();

            // Skip stale odds (>60s)
            if odds_age > 60 { continue; }

            let implied1 = 1.0 / odds.odds_team1 * 100.0;
            let implied2 = 1.0 / odds.odds_team2 * 100.0;

            // === SCORE MOMENTUM DETECTION ===
            // If team1 is ahead on score, but odds still imply lower prob
            // → potential value on team1 (odds haven't adjusted to live situation)
            let score_diff = score1 - score2;

            // CS2 Bo3 MAP scores: 1-0 = won first map (~68% win prob)
            // Even +1 map lead is significant for score edge!
            if score_diff >= 1 && implied1 < 75.0 {
                // team1 is leading maps but odds haven't fully adjusted
                // Score-implied fair probability: 1-0 → 68%, 2-0 → 95%
                let fair1 = match score_diff {
                    1 => 68.0_f64,
                    2 => 95.0,
                    _ => (implied1 + 15.0).min(95.0),
                };
                let edge = fair1 - implied1;
                if edge > 3.0 {
                    opportunities.push(Opportunity {
                        match_key: match_key.clone(),
                        opp_type: "score_momentum".to_string(),
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score: score_str.clone(),
                        value_side: 1,
                        signal: format!("{} leads {}, odds imply only {:.1}%",
                            live.payload.team1, score_str, implied1),
                        confidence: (edge / 20.0).min(1.0),
                        odds: odds.odds_team1,
                        implied_prob_pct: (implied1 * 100.0).round() / 100.0,
                        estimated_fair_pct: (fair1 * 100.0).round() / 100.0,
                        edge_pct: (edge * 100.0).round() / 100.0,
                        bookmaker: odds.bookmaker.clone(),
                        odds_age_secs: odds_age,
                        live_age_secs: live_age,
                    });
                }
            }

            if score_diff <= -1 && implied2 < 75.0 {
                let fair2 = match score_diff.abs() {
                    1 => 68.0_f64,
                    2 => 95.0,
                    _ => (implied2 + 15.0).min(95.0),
                };
                let edge = fair2 - implied2;
                if edge > 3.0 {
                    opportunities.push(Opportunity {
                        match_key: match_key.clone(),
                        opp_type: "score_momentum".to_string(),
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score: score_str.clone(),
                        value_side: 2,
                        signal: format!("{} leads {}, odds imply only {:.1}%",
                            live.payload.team2, score_str, implied2),
                        confidence: (edge / 20.0).min(1.0),
                        odds: odds.odds_team2,
                        implied_prob_pct: (implied2 * 100.0).round() / 100.0,
                        estimated_fair_pct: (fair2 * 100.0).round() / 100.0,
                        edge_pct: (edge * 100.0).round() / 100.0,
                        bookmaker: odds.bookmaker.clone(),
                        odds_age_secs: odds_age,
                        live_age_secs: live_age,
                    });
                }
            }

            // === SPREAD CHECK (single bookmaker) ===
            // Very low spread (<3%) = bookmaker very sure → potential value on the underdog
            // High spread (>12%) = bookmaker unsure → avoid
            let spread = (implied1 + implied2 - 100.0).abs();
            if spread < 3.0 && (odds.odds_team1 > 2.5 || odds.odds_team2 > 2.5) {
                let (side, underdog_odds, underdog_implied) = if odds.odds_team1 > odds.odds_team2 {
                    (1u8, odds.odds_team1, implied1)
                } else {
                    (2u8, odds.odds_team2, implied2)
                };
                let fair = underdog_implied + 5.0;
                let edge = fair - underdog_implied;
                let underdog_name = if side == 1 { &live.payload.team1 } else { &live.payload.team2 };
                opportunities.push(Opportunity {
                    match_key: match_key.clone(),
                    opp_type: "tight_spread_underdog".to_string(),
                    team1: live.payload.team1.clone(),
                    team2: live.payload.team2.clone(),
                    score: score_str.clone(),
                    value_side: side,
                    signal: format!("Tight spread {:.1}%, {} at {:.2}",
                        spread, underdog_name, underdog_odds),
                    confidence: 0.3,
                    odds: underdog_odds,
                    implied_prob_pct: (underdog_implied * 100.0).round() / 100.0,
                    estimated_fair_pct: (fair * 100.0).round() / 100.0,
                    edge_pct: (edge * 100.0).round() / 100.0,
                    bookmaker: odds.bookmaker.clone(),
                    odds_age_secs: odds_age,
                    live_age_secs: live_age,
                });
            }
        }

        // === CROSS-BOOKMAKER ARB (if multiple bookmaker odds for same match) ===
        if odds_list.len() >= 2 {
            for i in 0..odds_list.len() {
                for j in (i+1)..odds_list.len() {
                    let a = &odds_list[i].payload;
                    let b = &odds_list[j].payload;
                    // Check arb: 1/odds_a_team1 + 1/odds_b_team2 < 1
                    let arb1 = 1.0 / a.odds_team1 + 1.0 / b.odds_team2;
                    let arb2 = 1.0 / a.odds_team2 + 1.0 / b.odds_team1;
                    if arb1 < 1.0 {
                        let profit_pct = (1.0 - arb1) * 100.0;
                        opportunities.push(Opportunity {
                            match_key: match_key.clone(),
                            opp_type: "arb_cross_book".to_string(),
                            team1: live.payload.team1.clone(),
                            team2: live.payload.team2.clone(),
                            score: score_str.clone(),
                            value_side: 0,
                            signal: format!("ARB {:.2}%: {} t1@{:.2}({}) + t2@{:.2}({})",
                                profit_pct, match_key, a.odds_team1, a.bookmaker,
                                b.odds_team2, b.bookmaker),
                            confidence: (profit_pct / 5.0).min(1.0),
                            odds: a.odds_team1,
                            implied_prob_pct: arb1 * 100.0,
                            estimated_fair_pct: 100.0,
                            edge_pct: (profit_pct * 100.0).round() / 100.0,
                            bookmaker: format!("{}+{}", a.bookmaker, b.bookmaker),
                            odds_age_secs: 0,
                            live_age_secs: live_age,
                        });
                    }
                    if arb2 < 1.0 {
                        let profit_pct = (1.0 - arb2) * 100.0;
                        opportunities.push(Opportunity {
                            match_key: match_key.clone(),
                            opp_type: "arb_cross_book".to_string(),
                            team1: live.payload.team1.clone(),
                            team2: live.payload.team2.clone(),
                            score: score_str.clone(),
                            value_side: 0,
                            signal: format!("ARB {:.2}%: {} t2@{:.2}({}) + t1@{:.2}({})",
                                profit_pct, match_key, a.odds_team2, a.bookmaker,
                                b.odds_team1, b.bookmaker),
                            confidence: (profit_pct / 5.0).min(1.0),
                            odds: a.odds_team2,
                            implied_prob_pct: arb2 * 100.0,
                            estimated_fair_pct: 100.0,
                            edge_pct: (profit_pct * 100.0).round() / 100.0,
                            bookmaker: format!("{}+{}", a.bookmaker, b.bookmaker),
                            odds_age_secs: 0,
                            live_age_secs: live_age,
                        });
                    }
                }
            }
        }
    }

    // Sort by edge descending
    opportunities.sort_by(|a, b| b.edge_pct.partial_cmp(&a.edge_pct).unwrap_or(std::cmp::Ordering::Equal));

    OpportunitiesResponse {
        ts: Utc::now().to_rfc3339(),
        total_live,
        total_odds,
        fused_matches,
        opportunities,
    }
}

async fn build_state_snapshot(state: &FeedHubState) -> HttpStateResponse {
    let connections = *state.connections.read().await;
    let live_map = state.live.read().await;
    let odds_map = state.odds.read().await;

    let live_items = live_map.len();
    let odds_items = odds_map.len();

    // Collect unique match keys from odds
    let mut odds_match_keys = std::collections::HashSet::new();
    for ok in odds_map.keys() {
        odds_match_keys.insert(ok.match_key.clone());
    }

    // Esports fallback alts — same as in build_opportunities
    let esports_alts_snap: &[&str] = &[
        "cs2", "dota-2", "league-of-legends", "valorant",
        "basketball", "football", "mma", "starcraft",
    ];
    let mut fused_keys = Vec::new();
    for k in &odds_match_keys {
        let is_fused = if live_map.contains_key(k) {
            true
        } else {
            // Check if any live key with esports:: prefix matches via alt
            let parts: Vec<&str> = k.splitn(2, "::").collect();
            if parts.len() == 2 {
                let tail = parts[1];
                esports_alts_snap.iter().any(|alt| {
                    if *alt == parts[0] {
                        // live key would be esports::tail
                        live_map.contains_key(&format!("esports::{}", tail))
                    } else {
                        false
                    }
                })
            } else { false }
        };
        if is_fused {
            fused_keys.push(k.clone());
        }
        if fused_keys.len() >= 50 {
            break;
        }
    }

    let fused_ready = fused_keys.len();

    let mut live = Vec::new();
    for (k, v) in live_map.iter() {
        live.push(HttpLiveItem {
            match_key: k.clone(),
            source: v.source.clone(),
            seen_at: v.seen_at.to_rfc3339(),
            payload: v.payload.clone(),
        });
        if live.len() >= 50 {
            break;
        }
    }

    let mut odds = Vec::new();
    for (k, v) in odds_map.iter() {
        odds.push(HttpOddsItem {
            match_key: k.match_key.clone(),
            source: v.source.clone(),
            seen_at: v.seen_at.to_rfc3339(),
            payload: v.payload.clone(),
        });
        if odds.len() >= 50 {
            break;
        }
    }

    HttpStateResponse {
        ts: Utc::now().to_rfc3339(),
        connections,
        live_items,
        odds_items,
        fused_ready,
        fused_keys,
        live,
        odds,
    }
}

async fn handle_http_connection(mut stream: TcpStream, state: FeedHubState) -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.context("http read")?;
    if n == 0 {
        return Ok(());
    }

    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (status_line, content_type, body) = match (method, path) {
        ("GET", "/health") => ("HTTP/1.1 200 OK", "text/plain; charset=utf-8", "ok".to_string()),
        ("GET", "/state") => {
            let snap = build_state_snapshot(&state).await;
            let json = serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".to_string());
            ("HTTP/1.1 200 OK", "application/json; charset=utf-8", json)
        }
        ("GET", "/opportunities") => {
            let opps = build_opportunities(&state).await;
            let json = serde_json::to_string_pretty(&opps).unwrap_or_else(|_| "{}".to_string());
            ("HTTP/1.1 200 OK", "application/json; charset=utf-8", json)
        }
        _ => (
            "HTTP/1.1 404 Not Found",
            "text/plain; charset=utf-8",
            "not found".to_string(),
        ),
    };

    let resp = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );
    stream.write_all(resp.as_bytes()).await.context("http write")?;
    Ok(())
}

async fn start_http_server(state: FeedHubState, bind: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(bind).await.context("http bind")?;
    info!("feed-hub http listening on http://{} (GET /health, /state, /opportunities)", bind);

    loop {
        let (stream, peer) = listener.accept().await.context("http accept")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_http_connection(stream, state).await {
                debug!("http handler err {}: {}", peer, e);
            }
        });
    }
}

async fn handle_socket(
    peer: SocketAddr,
    stream: TcpStream,
    state: FeedHubState,
    logger: Arc<EventLogger>,
    db_tx: mpsc::Sender<DbMsg>,
) -> Result<()> {
    let ws_stream = accept_async(stream).await.context("WS handshake failed")?;

    {
        let mut c = state.connections.write().await;
        *c += 1;
    }

    info!("WS client connected: {}", peer);

    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    while let Some(msg) = ws_stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!("WS recv err from {}: {}", peer, e);
                break;
            }
        };

        match msg {
            Message::Text(txt) => {
                let txt = txt.to_string();
                let parsed: Result<FeedEnvelope> = serde_json::from_str(&txt)
                    .context("invalid JSON envelope")
                    .map_err(Into::into);

                let (ok, note) = match parsed {
                    Ok(env) => {
                        if env.v != 1 {
                            (false, format!("unsupported version {}", env.v))
                        } else {
                            let env_source = env.source.clone();
                            match env.msg_type {
                                FeedMessageType::LiveMatch => {
                                    let payload: LiveMatchPayload = serde_json::from_value(env.payload)
                                        .context("invalid live_match payload")?;
                                    let seen_at = parse_ts(&env.ts);
                                    let key = match_key(&payload.sport, &payload.team1, &payload.team2);
                                    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

                                    state.live.write().await.insert(
                                        key.clone(),
                                        LiveMatchState {
                                            source: env_source.clone(),
                                            seen_at,
                                            payload: payload.clone(),
                                        },
                                    );

                                    let _ = db_tx.try_send(DbMsg::LiveUpsert(DbLiveRow {
                                        ts: seen_at,
                                        source: env_source,
                                        sport: payload.sport,
                                        team1: payload.team1,
                                        team2: payload.team2,
                                        match_key: key,
                                        payload_json,
                                    }));

                                    (true, "live_match_ingested".to_string())
                                }
                                FeedMessageType::Odds => {
                                    let payload: OddsPayload = serde_json::from_value(env.payload)
                                        .context("invalid odds payload")?;
                                    let seen_at = parse_ts(&env.ts);
                                    let key = match_key(&payload.sport, &payload.team1, &payload.team2);
                                    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

                                    let odds_key = OddsKey {
                                        match_key: key.clone(),
                                        bookmaker: payload.bookmaker.clone(),
                                    };
                                    state.odds.write().await.insert(
                                        odds_key,
                                        OddsState {
                                            source: env_source.clone(),
                                            seen_at,
                                            payload: payload.clone(),
                                        },
                                    );

                                    let _ = db_tx.try_send(DbMsg::OddsUpsert(DbOddsRow {
                                        ts: seen_at,
                                        source: env_source.clone(),
                                        sport: payload.sport.clone(),
                                        bookmaker: payload.bookmaker.clone(),
                                        market: payload.market.clone(),
                                        team1: payload.team1.clone(),
                                        team2: payload.team2.clone(),
                                        match_key: key.clone(),
                                        odds_team1: payload.odds_team1,
                                        odds_team2: payload.odds_team2,
                                        liquidity_usd: payload.liquidity_usd,
                                        spread_pct: payload.spread_pct,
                                        payload_json,
                                    }));

                                    let (pass, why) = gate_odds(&payload, seen_at);
                                    if pass {
                                        if let Some(live) = state.live.read().await.get(&key).cloned() {
                                            let fusion = LiveFusionReadyEvent {
                                                ts: Utc::now().to_rfc3339(),
                                                event: "LIVE_FUSION_READY",
                                                sport: payload.sport.clone(),
                                                match_key: key,
                                                live_source: live.source,
                                                odds_source: env_source.clone(),
                                                bookmaker: payload.bookmaker.clone(),
                                                market: payload.market.clone(),
                                                liquidity_usd: payload.liquidity_usd,
                                                spread_pct: payload.spread_pct,
                                            };
                                            let _ = logger.log(&fusion);

                                            let _ = db_tx.try_send(DbMsg::Fusion(DbFusionRow {
                                                ts: Utc::now(),
                                                sport: fusion.sport.clone(),
                                                match_key: fusion.match_key.clone(),
                                                live_source: fusion.live_source.clone(),
                                                odds_source: fusion.odds_source.clone(),
                                                bookmaker: fusion.bookmaker.clone(),
                                                market: fusion.market.clone(),
                                                liquidity_usd: fusion.liquidity_usd,
                                                spread_pct: fusion.spread_pct,
                                            }));
                                        }
                                        (true, format!("odds_ingested_gated:{}", why))
                                    } else {
                                        (true, format!("odds_ingested_rejected:{}", why))
                                    }
                                }
                                FeedMessageType::Heartbeat => (true, "heartbeat".to_string()),
                            }
                        }
                    }
                    Err(e) => (false, format!("parse_error:{}", e)),
                };

                let ingest = FeedIngestEvent {
                    ts: Utc::now().to_rfc3339(),
                    event: "FEED_INGEST",
                    source: "ws".to_string(),
                    msg_type: "text".to_string(),
                    ok,
                    note: note.clone(),
                };
                let _ = logger.log(&ingest);

                let _ = db_tx.try_send(DbMsg::Ingest(DbIngestRow {
                    ts: Utc::now(),
                    source: "ws".to_string(),
                    msg_type: "text".to_string(),
                    ok,
                    note: note.clone(),
                    raw_json: Some(txt.clone()),
                }));

                let ack = serde_json::json!({"ok": ok, "note": note});
                let _ = ws_sink.send(Message::Text(ack.to_string().into())).await;
            }
            Message::Ping(payload) => {
                let _ = ws_sink.send(Message::Pong(payload)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    info!("WS client disconnected: {}", peer);
    {
        let mut c = state.connections.write().await;
        *c = c.saturating_sub(1);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let bind = std::env::var("FEED_HUB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse().context("Invalid FEED_HUB_BIND")?;

    let listener = TcpListener::bind(addr).await.context("bind failed")?;
    info!("feed-hub listening on ws://{}/feed", addr);

    let state = FeedHubState::new();
    let logger = Arc::new(EventLogger::new("logs"));

    let db_path = std::env::var("FEED_DB_PATH").unwrap_or_else(|_| "data/feed.db".to_string());
    info!("feed-hub DB: {}", db_path);
    let db_tx = spawn_db_writer(DbConfig { path: db_path });

    // Minimal HTTP read-only state endpoint
    {
        let http_bind = std::env::var("FEED_HTTP_BIND").unwrap_or_else(|_| "127.0.0.1:8081".to_string());
        let http_addr: SocketAddr = http_bind.parse().context("Invalid FEED_HTTP_BIND")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = start_http_server(state, http_addr).await {
                warn!("http server stopped: {e}");
            }
        });
    }

    // Staleness cleanup — remove entries older than 120s
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let cutoff = Utc::now() - chrono::Duration::seconds(120);
                {
                    let mut live = state.live.write().await;
                    let before = live.len();
                    live.retain(|_, v| v.seen_at > cutoff);
                    let removed = before - live.len();
                    if removed > 0 {
                        info!("staleness cleanup: removed {} stale live entries", removed);
                    }
                }
                {
                    let mut odds = state.odds.write().await;
                    let before = odds.len();
                    odds.retain(|_, v| v.seen_at > cutoff);
                    let removed = before - odds.len();
                    if removed > 0 {
                        info!("staleness cleanup: removed {} stale odds entries", removed);
                    }
                }
            }
        });
    }

    // Heartbeat summary
    {
        let state = state.clone();
        let logger = Arc::clone(&logger);
        let db_tx = db_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;

                let connections = *state.connections.read().await;
                let live_items = state.live.read().await.len();
                let odds_items = state.odds.read().await.len();

                // “fused_ready” = kolik odds klíčů má zároveň live
                let fused_ready = {
                    let live = state.live.read().await;
                    let odds = state.odds.read().await;
                    let mut match_keys = std::collections::HashSet::new();
                    for ok in odds.keys() {
                        match_keys.insert(ok.match_key.clone());
                    }
                    match_keys.iter().filter(|k| live.contains_key(k.as_str())).count()
                };

                let hb = FeedHeartbeatEvent {
                    ts: Utc::now().to_rfc3339(),
                    event: "FEED_HUB_HEARTBEAT",
                    connections,
                    live_items,
                    odds_items,
                    fused_ready,
                };

                let _ = logger.log(&hb);
                let _ = db_tx.try_send(DbMsg::Heartbeat(DbHeartbeatRow {
                    ts: Utc::now(),
                    connections: connections as i64,
                    live_items: live_items as i64,
                    odds_items: odds_items as i64,
                    fused_ready: fused_ready as i64,
                }));
                info!(
                    "HB: conns={}, live={}, odds={}, fused_ready={} (see logs/*.jsonl)",
                    connections, live_items, odds_items, fused_ready
                );
            }
        });
    }

    // NOTE: path routing se řeší u higher-level serverů; tady přijímáme WS na jakémkoliv path.
    // Azuro GraphQL poller — periodicky stahuje CS2 odds z on-chain subgraph
    {
        let state = state.clone();
        let db_tx = db_tx.clone();
        tokio::spawn(async move {
            azuro_poller::run_azuro_poller(state, db_tx).await;
        });
    }

    while let Ok((stream, peer)) = listener.accept().await {
        let state = state.clone();
        let logger = Arc::clone(&logger);
        let db_tx = db_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_socket(peer, stream, state, logger, db_tx).await {
                debug!("socket handler err {}: {}", peer, e);
            }
        });
    }

    Ok(())
}
