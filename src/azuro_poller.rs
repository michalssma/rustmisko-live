//! Azuro Protocol — MULTI-SPORT data-feed subgraph poller
//!
//! Používáme **data-feed** subgraph (thegraph-1.onchainfeed.org) který má:
//! - Aktuální hry (Live, Prematch) s odds v decimálním formátu
//! - Pole `state` (ne `status`), `conditions` s `outcomes.currentOdds`
//! - Pokrytí: Polygon, Base (Gnosis+Chiliz mrtvé = 0 games)
//!
//! Client subgraph (thegraph.onchainfeed.org) je zastaralý a data nevrací!
//!
//! Endpointy data-feed (production):
//!   Polygon: https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-polygon
//!   Base:    https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-base
//!
//! Sporty: cs2, tennis, football, basketball, dota-2, mma

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn, debug};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{
    match_key, FeedHubState, OddsKey, OddsState, OddsPayload,
    feed_db::{DbMsg, DbOddsRow},
};

// ====================================================================
// GraphQL response types (data-feed subgraph schema)
// ====================================================================

#[derive(Debug, Deserialize)]
struct GqlResponse {
    data: Option<GqlData>,
    errors: Option<Vec<GqlError>>,
}

#[derive(Debug, Deserialize)]
struct GqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GqlData {
    games: Option<Vec<AzuroGame>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzuroGame {
    id: String,
    title: Option<String>,
    starts_at: Option<String>,
    /// "Prematch" | "Live" | "PendingResolution" | "Resolved" | "Canceled"
    state: Option<String>,
    active_conditions_count: Option<i64>,
    participants: Option<Vec<AzuroParticipant>>,
    conditions: Option<Vec<AzuroCondition>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzuroParticipant {
    name: Option<String>,
    sort_order: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzuroCondition {
    id: Option<String>,
    /// "Active" | "Stopped" | "Resolved" | "Canceled"
    state: Option<String>,
    outcomes: Option<Vec<AzuroOutcome>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzuroOutcome {
    id: Option<String>,
    /// Decimal odds as string (e.g. "1.34", "2.86") — already decimal!
    current_odds: Option<String>,
    sort_order: Option<i64>,
}

// ====================================================================
// Data-feed subgraph endpoints (production)
// ====================================================================

/// Data-feed subgraphs — tyto mají aktuální hry a odds!
/// Gnosis + Chiliz = 0 games (mrtvé), pollujeme jen Polygon + Base
const AZURO_FEED_POLYGON: &str =
    "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-polygon";
const AZURO_FEED_BASE: &str =
    "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-base";

/// All sports we poll — FULL coverage of everything Azuro offers LIVE!
/// Added: lol, volleyball, ice-hockey, baseball, cricket, boxing (= +38 LIVE matches)
const AZURO_SPORTS: &[&str] = &[
    "cs2", "tennis", "football", "basketball", "dota-2", "mma",
    "lol", "volleyball", "ice-hockey", "baseball", "cricket", "boxing",
];

/// Poll interval — ULTRA-FAST for live score-edge detection!
/// 3s = 12 sports × 2 chains = 24 parallel queries per cycle = 480 req/min to The Graph.
/// Aggressive but within typical subgraph rate limits.
const AZURO_POLL_INTERVAL_SECS: u64 = 3;

// ====================================================================
// WS Shadow Mode — real-time condition updates from Azuro V3 streams
// ====================================================================

/// Azuro V3 WebSocket streams endpoint (production)
const AZURO_WS_URL: &str = "wss://streams.onchainfeed.org/v1/streams/feed";

/// Minimum interval between SubscribeConditions messages (throttle)
const WS_RESUBSCRIBE_MIN_SECS: u64 = 10;

/// Reconnect backoff sequence (ms)
const WS_RECONNECT_BACKOFF_MS: &[u64] = &[1_000, 2_000, 5_000, 10_000, 30_000];

/// Shadow WS metrics (shared across tasks)
struct WsShadowMetrics {
    updates_received: AtomicU64,
    reconnects: AtomicU64,
    last_update_epoch_ms: AtomicI64,
    subscribes_sent: AtomicU64,
}

/// WS subscribe message format
#[derive(Serialize)]
struct WsSubscribeMsg {
    event: String,
    conditions: Vec<String>,
    environment: String,
}

/// WS incoming message (generic — we match on event field)
#[derive(Deserialize, Debug)]
struct WsIncoming {
    event: Option<String>,
    id: Option<String>,
    data: Option<serde_json::Value>,
}

/// Condition set per environment: (polygon_ids, base_ids)
type ConditionSets = (Vec<String>, Vec<String>);

/// GraphQL query — data-feed schema (state, not status; Active conditions)
fn build_sport_query(sport_slug: &str) -> String {
    let now_unix = Utc::now().timestamp();
    // LIVE ONLY — prematch is useless for our score-edge strategy!
    let from = now_unix - 6 * 3600;
    let to = now_unix + 24 * 3600;

    format!(r#"{{
  games(
    first: 100
    where: {{
      sport_: {{ slug: "{sport_slug}" }}
      state_in: ["Live"]
      startsAt_gte: "{from}"
      startsAt_lte: "{to}"
    }}
    orderBy: startsAt
    orderDirection: asc
  ) {{
    id
    title
    startsAt
    state
    activeConditionsCount
    participants(orderBy: sortOrder) {{
      name
      sortOrder
    }}
    conditions(
      first: 20
      where: {{ state_in: ["Active"] }}
    ) {{
      id
      state
      outcomes(orderBy: sortOrder) {{
        id
        currentOdds
        sortOrder
      }}
    }}
  }}
}}"#)
}

fn build_cs2_query() -> String {
    build_sport_query("cs2")
}

fn build_tennis_query() -> String {
    build_sport_query("tennis")
}

/// Map sport slug → bookmaker.xyz URL path segment
fn sport_url_segment(sport: &str) -> &'static str {
    match sport {
        "cs2" => "esports/cs2",
        "tennis" => "sports/tennis",
        "football" => "sports/football",
        "basketball" => "sports/basketball",
        "dota-2" => "esports/dota-2",
        "mma" => "sports/mma",
        "lol" => "esports/lol",
        "volleyball" => "sports/volleyball",
        "ice-hockey" => "sports/ice-hockey",
        "baseball" => "sports/baseball",
        "cricket" => "sports/cricket",
        "boxing" => "sports/boxing",
        _ => "sports/other",
    }
}

// ====================================================================
// Odds parsing
// ====================================================================

/// Parse decimal odds string — data-feed already gives decimal (e.g. "1.34")
fn parse_decimal_odds(raw: &str) -> Option<f64> {
    let val: f64 = raw.parse().ok()?;
    if val >= 1.01 && val <= 100.0 {
        Some(val)
    } else {
        None
    }
}

/// Extract team names from participants, fallback to title parsing
fn extract_teams(game: &AzuroGame) -> Option<(String, String)> {
    if let Some(participants) = &game.participants {
        if participants.len() >= 2 {
            let t1 = participants[0].name.as_deref().unwrap_or("").trim();
            let t2 = participants[1].name.as_deref().unwrap_or("").trim();
            if !t1.is_empty() && !t2.is_empty() {
                return Some((t1.to_string(), t2.to_string()));
            }
        }
    }

    // Fallback: title contains unicode separator or " vs "
    if let Some(title) = &game.title {
        // Azuro uses \u{2013} (–) as separator in titles
        for sep in [" \u{2013} ", " – ", " vs ", " - "] {
            if let Some(pos) = title.find(sep) {
                let t1 = title[..pos].trim();
                let t2 = title[(pos + sep.len())..].trim();
                if !t1.is_empty() && !t2.is_empty() {
                    return Some((t1.to_string(), t2.to_string()));
                }
            }
        }
    }

    None
}

/// Parsed condition with market type info
#[derive(Debug, Clone)]
struct ParsedCondition {
    odds1: f64,
    odds2: f64,
    condition_id: Option<String>,
    outcome1_id: Option<String>,
    outcome2_id: Option<String>,
    /// "match_winner", "map1_winner", "map2_winner", "map3_winner", "winner" (tennis), etc.
    market: String,
}

/// Known Azuro outcome IDs for various sports (from dictionaries)
/// CS2: Match Winner 6995/6996, Map Winners 7009-7014
/// Tennis: Match Winner 6977/6978
/// Generic: 10009/10010, 7/8
/// Football/Basketball/MMA/Dota2: various per-sport IDs — treated as match_winner
fn classify_market_by_outcome_ids(oid1: &str, oid2: &str) -> String {
    match (oid1, oid2) {
        // CS2 Match Winner
        ("6995", "6996") | ("6996", "6995") => "match_winner".to_string(),
        ("10009", "10010") | ("10010", "10009") => "match_winner".to_string(),
        // CS2 Map Winners
        ("7009", "7010") | ("7010", "7009") => "map1_winner".to_string(),
        ("7011", "7012") | ("7012", "7011") => "map2_winner".to_string(),
        ("7013", "7014") | ("7014", "7013") => "map3_winner".to_string(),
        // Tennis match winner
        ("6977", "6978") | ("6978", "6977") => "match_winner".to_string(),
        // Generic 2-way (H1/H2)
        ("7", "8") | ("8", "7") => "match_winner".to_string(),
        // Fallback: treat any 2-outcome condition as match_winner
        // Different sports use different outcome IDs — we accept them all
        // and rely on deduplication (first condition per game = primary market)
        _ => {
            let n1: u64 = oid1.parse().unwrap_or(99999);
            let n2: u64 = oid2.parse().unwrap_or(99999);
            // CS2 range: try to identify specific map markets
            if (6995..=7014).contains(&n1) && (6995..=7014).contains(&n2) {
                let min_id = n1.min(n2);
                match min_id {
                    7009 => "map1_winner".to_string(),
                    7011 => "map2_winner".to_string(),
                    7013 => "map3_winner".to_string(),
                    _ => "match_winner".to_string(),
                }
            } else {
                // Accept as match_winner — works for basketball, MMA, football 2-way,
                // dota-2, and any future sport. First 2-way condition on Azuro is
                // typically the primary winner market.
                debug!("unknown outcome IDs {}/{} — accepting as match_winner", oid1, oid2);
                "match_winner".to_string()
            }
        }
    }
}

/// Extract ALL 2-way winner conditions from a game (match winner + map winners)
/// For each game, only ONE match_winner is kept (first = primary market on Azuro).
/// CS2 map winners (map1/map2/map3) are kept separately.
fn extract_all_winner_odds(game: &AzuroGame) -> Vec<ParsedCondition> {
    let mut results = Vec::new();
    let conditions = match game.conditions.as_ref() {
        Some(c) => c,
        None => return results,
    };

    let mut has_match_winner = false;

    for cond in conditions {
        let cond_state = cond.state.as_deref().unwrap_or("");
        // ONLY Active conditions — Stopped means paused/can't bet!
        if cond_state != "Active" {
            continue;
        }

        let outcomes = match cond.outcomes.as_ref() {
            Some(o) => o,
            None => continue,
        };

        // Only 2-way markets (winner bets)
        if outcomes.len() != 2 {
            continue;
        }

        let out1 = outcomes.iter().find(|o| o.sort_order == Some(0));
        let out2 = outcomes.iter().find(|o| o.sort_order == Some(1));

        let odds1 = out1
            .and_then(|o| o.current_odds.as_ref())
            .and_then(|raw| parse_decimal_odds(raw));
        let odds2 = out2
            .and_then(|o| o.current_odds.as_ref())
            .and_then(|raw| parse_decimal_odds(raw));

        if let (Some(o1), Some(o2)) = (odds1, odds2) {
            let cond_id = cond.id.clone();
            let oid1 = out1.and_then(|o| o.id.as_ref().map(|id| {
                id.rsplit('_').next().unwrap_or(id).to_string()
            }));
            let oid2 = out2.and_then(|o| o.id.as_ref().map(|id| {
                id.rsplit('_').next().unwrap_or(id).to_string()
            }));

            let market = classify_market_by_outcome_ids(
                oid1.as_deref().unwrap_or(""),
                oid2.as_deref().unwrap_or(""),
            );

            // Only keep ONE match_winner per game (first found = primary)
            if market == "match_winner" {
                if has_match_winner {
                    continue; // Skip duplicate match_winner conditions
                }
                has_match_winner = true;
            }

            results.push(ParsedCondition {
                odds1: o1,
                odds2: o2,
                condition_id: cond_id,
                outcome1_id: oid1,
                outcome2_id: oid2,
                market,
            });
        }
    }

    results
}

/// Extract match winner odds + condition/outcome IDs (legacy compatible)
/// Returns (odds1, odds2, condition_id, outcome1_id, outcome2_id)
fn extract_match_winner_odds(game: &AzuroGame) -> Option<(f64, f64, Option<String>, Option<String>, Option<String>)> {
    let all = extract_all_winner_odds(game);
    // Prefer match_winner, fallback to first available
    let mw = all.iter().find(|c| c.market == "match_winner")
        .or_else(|| all.first());
    mw.map(|c| (c.odds1, c.odds2, c.condition_id.clone(), c.outcome1_id.clone(), c.outcome2_id.clone()))
}

// ====================================================================
// Subgraph polling
// ====================================================================

struct PollResult {
    chain: &'static str,
    games: Vec<GameOdds>,
}

struct GameOdds {
    team1: String,
    team2: String,
    odds1: f64,
    odds2: f64,
    game_id: String,
    state: String,
    sport: String,
    /// Market type: "match_winner", "map1_winner", etc.
    market: String,
    /// Azuro condition ID for this market
    condition_id: Option<String>,
    /// Outcome ID for team1 win (sortOrder=0)
    outcome1_id: Option<String>,
    /// Outcome ID for team2 win (sortOrder=1)
    outcome2_id: Option<String>,
}

async fn poll_subgraph(
    client: &reqwest::Client,
    url: &str,
    chain: &'static str,
    sport: &'static str,
) -> PollResult {
    let query = build_sport_query(sport);
    let body = serde_json::json!({ "query": query });

    let resp = match client.post(url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("azuro {} fetch error: {}", chain, e);
            return PollResult { chain, games: vec![] };
        }
    };

    if !resp.status().is_success() {
        warn!("azuro {} HTTP {}", chain, resp.status());
        return PollResult { chain, games: vec![] };
    }

    let gql: GqlResponse = match resp.json().await {
        Ok(g) => g,
        Err(e) => {
            warn!("azuro {} json parse error: {}", chain, e);
            return PollResult { chain, games: vec![] };
        }
    };

    if let Some(errors) = &gql.errors {
        for e in errors {
            warn!("azuro {} GQL error: {}", chain, e.message);
        }
    }

    let api_games = match gql.data.and_then(|d| d.games) {
        Some(g) => g,
        None => {
            return PollResult { chain, games: vec![] };
        }
    };

    let mut games = Vec::new();
    for g in &api_games {
        let (team1, team2) = match extract_teams(g) {
            Some(t) => t,
            None => continue,
        };
        let state = g.state.as_deref().unwrap_or("?").to_string();

        // Extract ALL winner conditions (match + map winners)
        let all_conditions = extract_all_winner_odds(g);
        if all_conditions.is_empty() {
            continue;
        }

        for parsed in &all_conditions {
            games.push(GameOdds {
                team1: team1.clone(),
                team2: team2.clone(),
                odds1: parsed.odds1,
                odds2: parsed.odds2,
                game_id: g.id.clone(),
                state: state.clone(),
                sport: sport.to_string(),
                market: parsed.market.clone(),
                condition_id: parsed.condition_id.clone(),
                outcome1_id: parsed.outcome1_id.clone(),
                outcome2_id: parsed.outcome2_id.clone(),
            });
        }
    }

    PollResult { chain, games }
}

// ====================================================================
// WS Shadow Mode — daemon task
// ====================================================================

/// Run the WS shadow observer. Receives condition ID sets via watch channel,
/// subscribes to Azuro WS stream, and logs all ConditionUpdated events
/// with timing delta vs GQL polling for comparison.
///
/// This task does NOT modify FeedHubState — it's purely observational.
async fn run_shadow_ws(
    mut condition_rx: watch::Receiver<ConditionSets>,
    metrics: Arc<WsShadowMetrics>,
) {
    let mut backoff_idx: usize = 0;

    loop {
        info!("[SHADOW-WS] Connecting to {}", AZURO_WS_URL);

        let ws_stream = match tokio_tungstenite::connect_async(AZURO_WS_URL).await {
            Ok((stream, resp)) => {
                info!("[SHADOW-WS] Connected! HTTP {}", resp.status());
                backoff_idx = 0; // Reset backoff on successful connect
                stream
            }
            Err(e) => {
                let delay = WS_RECONNECT_BACKOFF_MS
                    .get(backoff_idx)
                    .copied()
                    .unwrap_or(30_000);
                warn!(
                    "[SHADOW-WS] Connect failed: {} — reconnecting in {}ms (attempt #{})",
                    e,
                    delay,
                    backoff_idx + 1
                );
                metrics.reconnects.fetch_add(1, Ordering::Relaxed);
                backoff_idx = (backoff_idx + 1).min(WS_RECONNECT_BACKOFF_MS.len() - 1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
        };

        let (mut ws_sink, mut ws_read) = ws_stream.split();

        // Track what we've subscribed to, for diffing
        let mut subscribed_polygon: HashSet<String> = HashSet::new();
        let mut subscribed_base: HashSet<String> = HashSet::new();
        let mut last_subscribe_ts = std::time::Instant::now()
            .checked_sub(Duration::from_secs(WS_RESUBSCRIBE_MIN_SECS + 1))
            .unwrap_or_else(std::time::Instant::now);

        // Inner loop: read messages + periodic resubscribe
        let disconnect_reason: String;
        loop {
            // Check if we need to (re)subscribe (new conditions from GQL poller)
            if condition_rx.has_changed().unwrap_or(false)
                && last_subscribe_ts.elapsed() >= Duration::from_secs(WS_RESUBSCRIBE_MIN_SECS)
            {
                let (poly_ids, base_ids) = condition_rx.borrow_and_update().clone();
                let poly_set: HashSet<String> = poly_ids.into_iter().collect();
                let base_set: HashSet<String> = base_ids.into_iter().collect();

                // Diff: only subscribe to NEW conditions (Azuro docs: additive subscribe)
                let new_poly: Vec<String> = poly_set.difference(&subscribed_polygon).cloned().collect();
                let new_base: Vec<String> = base_set.difference(&subscribed_base).cloned().collect();

                if !new_poly.is_empty() {
                    let msg = serde_json::to_string(&WsSubscribeMsg {
                        event: "SubscribeConditions".to_string(),
                        conditions: new_poly.clone(),
                        environment: "polygon".to_string(),
                    })
                    .unwrap();
                    if let Err(e) = ws_sink.send(WsMessage::Text(msg.into())).await {
                        disconnect_reason = format!("send error (polygon subscribe): {}", e);
                        break;
                    }
                    info!(
                        "[SHADOW-WS] Subscribed {} new Polygon conditions (total: {})",
                        new_poly.len(),
                        poly_set.len()
                    );
                    subscribed_polygon = poly_set;
                    metrics.subscribes_sent.fetch_add(1, Ordering::Relaxed);
                }

                if !new_base.is_empty() {
                    let msg = serde_json::to_string(&WsSubscribeMsg {
                        event: "SubscribeConditions".to_string(),
                        conditions: new_base.clone(),
                        environment: "base".to_string(),
                    })
                    .unwrap();
                    if let Err(e) = ws_sink.send(WsMessage::Text(msg.into())).await {
                        disconnect_reason = format!("send error (base subscribe): {}", e);
                        break;
                    }
                    info!(
                        "[SHADOW-WS] Subscribed {} new Base conditions (total: {})",
                        new_base.len(),
                        base_set.len()
                    );
                    subscribed_base = base_set;
                    metrics.subscribes_sent.fetch_add(1, Ordering::Relaxed);
                }

                last_subscribe_ts = std::time::Instant::now();
            }

            // Read next WS message with timeout (so we can check for resubscribe)
            let msg = tokio::time::timeout(Duration::from_secs(5), ws_read.next()).await;

            match msg {
                Ok(Some(Ok(WsMessage::Text(txt)))) => {
                    let now_ms = Utc::now().timestamp_millis();
                    metrics.last_update_epoch_ms.store(now_ms, Ordering::Relaxed);

                    // Parse incoming JSON
                    match serde_json::from_str::<WsIncoming>(&txt) {
                        Ok(incoming) => {
                            let event = incoming.event.as_deref().unwrap_or("?");
                            match event {
                                "ConditionUpdated" => {
                                    metrics.updates_received.fetch_add(1, Ordering::Relaxed);
                                    let cid = incoming.id.as_deref().unwrap_or("?");

                                    // Extract odds from data.outcomes
                                    let odds_str = if let Some(data) = &incoming.data {
                                        if let Some(outcomes) = data.get("outcomes").and_then(|v| v.as_array()) {
                                            outcomes
                                                .iter()
                                                .filter_map(|o| {
                                                    let oid = o.get("outcomeId").and_then(|v| v.as_u64());
                                                    let odds = o.get("currentOdds").and_then(|v| v.as_str());
                                                    match (oid, odds) {
                                                        (Some(id), Some(o)) => Some(format!("{}={}", id, o)),
                                                        _ => None,
                                                    }
                                                })
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        } else {
                                            "no-outcomes".to_string()
                                        }
                                    } else {
                                        "no-data".to_string()
                                    };

                                    let state_str = incoming.data
                                        .as_ref()
                                        .and_then(|d| d.get("state"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?");

                                    let total = metrics.updates_received.load(Ordering::Relaxed);
                                    debug!(
                                        "[SHADOW-WS] ConditionUpdated cid={} state={} odds=[{}] (total: {})",
                                        &cid[..cid.len().min(20)],
                                        state_str,
                                        odds_str,
                                        total
                                    );
                                }
                                "SubscribedToConditions" => {
                                    let count = incoming.data
                                        .as_ref()
                                        .and_then(|d| d.as_array())
                                        .map(|a| a.len())
                                        .unwrap_or(0);
                                    info!(
                                        "[SHADOW-WS] SubscribedToConditions: {} IDs confirmed",
                                        count
                                    );
                                }
                                _ => {
                                    debug!("[SHADOW-WS] Unknown event: {} data={:?}",
                                        event,
                                        incoming.data.as_ref().map(|d| {
                                            let s = d.to_string();
                                            if s.len() > 100 { format!("{}...", &s[..100]) } else { s }
                                        })
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            debug!("[SHADOW-WS] JSON parse error: {} raw={}", e, &txt[..txt.len().min(200)]);
                        }
                    }
                }
                Ok(Some(Ok(WsMessage::Ping(payload)))) => {
                    let _ = ws_sink.send(WsMessage::Pong(payload)).await;
                }
                Ok(Some(Ok(WsMessage::Close(frame)))) => {
                    disconnect_reason = format!(
                        "server close: {:?}",
                        frame.map(|f| format!("{} {}", f.code, f.reason))
                    );
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
                    // Timeout — normal, loop back to check resubscribe
                    continue;
                }
                _ => {}
            }
        }

        // Disconnected — reconnect with backoff
        metrics.reconnects.fetch_add(1, Ordering::Relaxed);
        let delay = WS_RECONNECT_BACKOFF_MS
            .get(backoff_idx)
            .copied()
            .unwrap_or(30_000);
        warn!(
            "[SHADOW-WS] Disconnected: {} — reconnecting in {}ms (reconnects: {})",
            disconnect_reason,
            delay,
            metrics.reconnects.load(Ordering::Relaxed)
        );
        backoff_idx = (backoff_idx + 1).min(WS_RECONNECT_BACKOFF_MS.len() - 1);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

// ====================================================================
// Main polling loop
// ====================================================================

pub async fn run_azuro_poller(state: FeedHubState, db_tx: mpsc::Sender<DbMsg>) {
    info!(
        "azuro data-feed poller starting — polling every {}s for {} sports (Polygon+Base)",
        AZURO_POLL_INTERVAL_SECS, AZURO_SPORTS.len()
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("RustMiskoLive/1.0")
        .pool_max_idle_per_host(6)
        .build()
        .expect("failed to create reqwest client");

    // ── Shadow WS spawn ──
    let ws_metrics = Arc::new(WsShadowMetrics {
        updates_received: AtomicU64::new(0),
        reconnects: AtomicU64::new(0),
        last_update_epoch_ms: AtomicI64::new(0),
        subscribes_sent: AtomicU64::new(0),
    });
    let (condition_tx, condition_rx) = watch::channel::<ConditionSets>((vec![], vec![]));
    let ws_metrics_clone = ws_metrics.clone();
    tokio::spawn(async move {
        run_shadow_ws(condition_rx, ws_metrics_clone).await;
    });
    info!("[SHADOW-WS] Shadow WebSocket daemon spawned");

    // Minimal startup delay
    tokio::time::sleep(Duration::from_secs(1)).await;

    loop {
        let now = Utc::now();
        let mut total_injected = 0usize;
        let mut total_live = 0usize;
        let mut total_prematch = 0usize;
        let mut total_map_markets = 0usize;
        let mut sport_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        // Poll ALL 12 sports on Polygon + Base in parallel (24 queries)
        // Gnosis + Chiliz are dead (0 games) — skip them to save latency
        let (
            cs2_poly, cs2_base,
            ten_poly, ten_base,
            fb_poly, fb_base,
            bb_poly, bb_base,
            dota_poly, dota_base,
            mma_poly, mma_base,
            lol_poly, lol_base,
            vb_poly, vb_base,
            ih_poly, ih_base,
            base_poly, base_base,
            cri_poly, cri_base,
            box_poly, box_base,
        ) = tokio::join!(
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "cs2"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "cs2"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "tennis"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "tennis"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "football"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "football"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "basketball"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "basketball"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "dota-2"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "dota-2"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "mma"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "mma"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "lol"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "lol"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "volleyball"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "volleyball"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "ice-hockey"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "ice-hockey"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "baseball"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "baseball"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "cricket"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "cricket"),
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon", "boxing"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base", "boxing"),
        );

        // Merge results — dedup by match_key+market (prefer polygon > base)
        let mut seen_keys = std::collections::HashSet::new();
        let all_results = [
            cs2_poly, cs2_base,
            ten_poly, ten_base,
            fb_poly, fb_base,
            bb_poly, bb_base,
            dota_poly, dota_base,
            mma_poly, mma_base,
            lol_poly, lol_base,
            vb_poly, vb_base,
            ih_poly, ih_base,
            base_poly, base_base,
            cri_poly, cri_base,
            box_poly, box_base,
        ];

        for result in &all_results {
            for game in &result.games {
                let sport = &game.sport;
                let base_key = match_key(sport, &game.team1, &game.team2);
                // Dedup key includes market type to allow map1/map2/map3 + match_winner
                let dedup_key = format!("{}::{}", base_key, game.market);

                if !seen_keys.insert(dedup_key.clone()) {
                    continue; // Already processed from higher-priority chain
                }

                match game.state.as_str() {
                    "Live" => total_live += 1,
                    "Prematch" => total_prematch += 1,
                    _ => {}
                }

                if game.market.starts_with("map") {
                    total_map_markets += 1;
                }
                *sport_counts.entry(sport.clone()).or_insert(0) += 1;

                let bookmaker_name = if game.market == "match_winner" {
                    format!("azuro_{}", result.chain)
                } else {
                    // Map winner markets get a distinct bookmaker name
                    format!("azuro_{}_{}", result.chain, game.market)
                };

                let url_path = format!("https://bookmaker.xyz/{}/{}", sport_url_segment(sport), game.game_id);

                let payload = OddsPayload {
                    sport: sport.clone(),
                    bookmaker: bookmaker_name.clone(),
                    market: game.market.clone(),
                    team1: game.team1.clone(),
                    team2: game.team2.clone(),
                    odds_team1: game.odds1,
                    odds_team2: game.odds2,
                    liquidity_usd: None,
                    spread_pct: None,
                    url: Some(url_path),
                    game_id: Some(game.game_id.clone()),
                    condition_id: game.condition_id.clone(),
                    outcome1_id: game.outcome1_id.clone(),
                    outcome2_id: game.outcome2_id.clone(),
                    chain: Some(result.chain.to_string()),
                };

                let odds_key = OddsKey {
                    match_key: base_key.clone(),
                    bookmaker: bookmaker_name.clone(),
                };

                state.odds.write().await.insert(
                    odds_key,
                    OddsState {
                        source: format!("azuro_{}", result.chain),
                        seen_at: now,
                        payload: payload.clone(),
                    },
                );

                let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
                let _ = db_tx.try_send(DbMsg::OddsUpsert(DbOddsRow {
                    ts: now,
                    source: format!("azuro_{}", result.chain),
                    sport: sport.clone(),
                    bookmaker: bookmaker_name,
                    market: game.market.clone(),
                    team1: game.team1.clone(),
                    team2: game.team2.clone(),
                    match_key: base_key,
                    odds_team1: game.odds1,
                    odds_team2: game.odds2,
                    liquidity_usd: None,
                    spread_pct: None,
                    payload_json,
                }));

                total_injected += 1;
            }
        }

        if total_injected > 0 {
            let sport_summary: String = sport_counts.iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect::<Vec<_>>()
                .join(", ");
            info!(
                "azuro poll OK: {} total ({} live, {} prematch, {} map mkts) [{}]",
                total_injected, total_live, total_prematch, total_map_markets, sport_summary
            );
        } else {
            debug!("azuro poll: 0 games with odds right now");
        }

        // ── Feed active condition IDs to WS Shadow daemon ──
        {
            let mut poly_conds: Vec<String> = Vec::new();
            let mut base_conds: Vec<String> = Vec::new();
            for result in &all_results {
                for game in &result.games {
                    if let Some(cid) = &game.condition_id {
                        match result.chain {
                            "polygon" => poly_conds.push(cid.clone()),
                            "base" => base_conds.push(cid.clone()),
                            _ => {}
                        }
                    }
                }
            }
            poly_conds.sort();
            poly_conds.dedup();
            base_conds.sort();
            base_conds.dedup();
            let total_ws = poly_conds.len() + base_conds.len();
            if total_ws > 0 {
                let _ = condition_tx.send((poly_conds, base_conds));
            }
            // Periodic shadow metrics log (every ~30s = every 10th cycle)
            let ws_updates = ws_metrics.updates_received.load(Ordering::Relaxed);
            let ws_reconnects = ws_metrics.reconnects.load(Ordering::Relaxed);
            let ws_subs = ws_metrics.subscribes_sent.load(Ordering::Relaxed);
            if ws_updates > 0 || ws_subs > 0 {
                debug!(
                    "[SHADOW-WS] stats: {} updates, {} reconnects, {} subscribes, {} active conditions",
                    ws_updates, ws_reconnects, ws_subs, total_ws
                );
            }
        }

        tokio::time::sleep(Duration::from_secs(AZURO_POLL_INTERVAL_SECS)).await;
    }
}
