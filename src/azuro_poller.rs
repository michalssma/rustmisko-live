//! Azuro Protocol — data-feed subgraph poller pro CS2 odds
//!
//! Používáme **data-feed** subgraph (thegraph-1.onchainfeed.org) který má:
//! - Aktuální hry (Live, Prematch) s odds v decimálním formátu
//! - Pole `state` (ne `status`), `conditions` s `outcomes.currentOdds`
//! - Pokrytí: Polygon, Gnosis, Base, Chiliz
//!
//! Client subgraph (thegraph.onchainfeed.org) je zastaralý a data nevrací!
//!
//! Endpointy data-feed (production):
//!   Polygon: https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-polygon
//!   Gnosis:  https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-gnosis
//!   Base:    https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-base
//!   Chiliz:  https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-chiliz

use chrono::Utc;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn, debug};

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
const AZURO_FEED_POLYGON: &str =
    "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-polygon";
const AZURO_FEED_GNOSIS: &str =
    "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-gnosis";
const AZURO_FEED_BASE: &str =
    "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-base";
const AZURO_FEED_CHILIZ: &str =
    "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-chiliz";

/// Poll interval
const AZURO_POLL_INTERVAL_SECS: u64 = 30;

/// GraphQL query — data-feed schema (state, not status; Active conditions)
fn build_cs2_query() -> String {
    let now_unix = Utc::now().timestamp();
    // Fetch games starting in the past 6h (live) through next 24h (prematch)
    let from = now_unix - 6 * 3600;
    let to = now_unix + 24 * 3600;

    format!(r#"{{
  games(
    first: 50
    where: {{
      sport_: {{ slug: "cs2" }}
      state_in: ["Prematch", "Live"]
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
      first: 5
      where: {{ state_in: ["Active", "Stopped"] }}
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

/// Extract match winner odds — first condition with exactly 2 outcomes
fn extract_match_winner_odds(game: &AzuroGame) -> Option<(f64, f64)> {
    let conditions = game.conditions.as_ref()?;

    for cond in conditions {
        // Prefer Active conditions
        let cond_state = cond.state.as_deref().unwrap_or("");
        if cond_state != "Active" && cond_state != "Stopped" {
            continue;
        }

        let outcomes = cond.outcomes.as_ref()?;
        if outcomes.len() == 2 {
            let odds1 = outcomes.iter()
                .find(|o| o.sort_order == Some(0))
                .and_then(|o| o.current_odds.as_ref())
                .and_then(|raw| parse_decimal_odds(raw));

            let odds2 = outcomes.iter()
                .find(|o| o.sort_order == Some(1))
                .and_then(|o| o.current_odds.as_ref())
                .and_then(|raw| parse_decimal_odds(raw));

            if let (Some(o1), Some(o2)) = (odds1, odds2) {
                return Some((o1, o2));
            }
        }
    }
    None
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
}

async fn poll_subgraph(
    client: &reqwest::Client,
    url: &str,
    chain: &'static str,
) -> PollResult {
    let query = build_cs2_query();
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
        let (odds1, odds2) = match extract_match_winner_odds(g) {
            Some(o) => o,
            None => continue,
        };
        let state = g.state.as_deref().unwrap_or("?").to_string();

        games.push(GameOdds {
            team1, team2, odds1, odds2,
            game_id: g.id.clone(),
            state,
        });
    }

    PollResult { chain, games }
}

// ====================================================================
// Main polling loop
// ====================================================================

pub async fn run_azuro_poller(state: FeedHubState, db_tx: mpsc::Sender<DbMsg>) {
    info!(
        "azuro data-feed poller starting — polling every {}s for CS2 odds (Polygon+Gnosis+Base+Chiliz)",
        AZURO_POLL_INTERVAL_SECS
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("RustMiskoLive/1.0")
        .build()
        .expect("failed to create reqwest client");

    // Initial delay
    tokio::time::sleep(Duration::from_secs(3)).await;

    loop {
        let now = Utc::now();
        let mut total_injected = 0usize;
        let mut total_live = 0usize;
        let mut total_prematch = 0usize;

        // Poll all 4 chains in parallel
        let (polygon, gnosis, base, chiliz) = tokio::join!(
            poll_subgraph(&client, AZURO_FEED_POLYGON, "polygon"),
            poll_subgraph(&client, AZURO_FEED_GNOSIS, "gnosis"),
            poll_subgraph(&client, AZURO_FEED_BASE, "base"),
            poll_subgraph(&client, AZURO_FEED_CHILIZ, "chiliz"),
        );

        // Merge results — dedup by match_key (prefer polygon > gnosis > base > chiliz)
        let mut seen_keys = std::collections::HashSet::new();
        let all_results = [polygon, gnosis, base, chiliz];

        for result in &all_results {
            for game in &result.games {
                let key = match_key("cs2", &game.team1, &game.team2);

                if !seen_keys.insert(key.clone()) {
                    continue; // Already processed this match from higher-priority chain
                }

                match game.state.as_str() {
                    "Live" => total_live += 1,
                    "Prematch" => total_prematch += 1,
                    _ => {}
                }

                let bookmaker_name = format!("azuro_{}", result.chain);

                let payload = OddsPayload {
                    sport: "cs2".to_string(),
                    bookmaker: bookmaker_name.clone(),
                    market: "match_winner".to_string(),
                    team1: game.team1.clone(),
                    team2: game.team2.clone(),
                    odds_team1: game.odds1,
                    odds_team2: game.odds2,
                    liquidity_usd: None,
                    spread_pct: None,
                    url: Some(format!("https://bookmaker.xyz/esports/cs2/{}", game.game_id)),
                };

                let odds_key = OddsKey {
                    match_key: key.clone(),
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
                    sport: "cs2".to_string(),
                    bookmaker: bookmaker_name,
                    market: "match_winner".to_string(),
                    team1: game.team1.clone(),
                    team2: game.team2.clone(),
                    match_key: key,
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
            info!(
                "azuro poll OK: {} CS2 games injected ({} live, {} prematch) — poly={} gno={} base={} chz={}",
                total_injected, total_live, total_prematch,
                all_results[0].games.len(),
                all_results[1].games.len(),
                all_results[2].games.len(),
                all_results[3].games.len(),
            );
        } else {
            info!(
                "azuro poll: 0 CS2 games with odds right now (poly={} gno={} base={} chz={})",
                all_results[0].games.len(),
                all_results[1].games.len(),
                all_results[2].games.len(),
                all_results[3].games.len(),
            );
        }

        tokio::time::sleep(Duration::from_secs(AZURO_POLL_INTERVAL_SECS)).await;
    }
}
