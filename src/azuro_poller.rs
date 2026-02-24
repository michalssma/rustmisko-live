//! Azuro Protocol GraphQL poller — stahuje CS2 odds z on-chain subgraph
//!
//! Azuro je decentralizovaný bookmaker na Polygon/Gnosis/Base.
//! Používáme The Graph subgraph pro structured query CS2 her s aktivními podmínkami.
//! Výsledné odds se injektují do FeedHubState jako bookmaker "azuro".
//!
//! Endpointy:
//!   Polygon: https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-polygon-v3
//!   Gnosis:  https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-gnosis-v3

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
// Azuro GraphQL response types
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
    /// On-chain game ID
    id: String,
    /// Game title (e.g. "FURIA vs M80")
    title: Option<String>,
    /// Unix timestamp
    starts_at: Option<String>,
    /// "Created" | "Resolved" | "Canceled" | "Paused"
    status: Option<String>,
    /// Whether the game has active betting conditions
    has_active_conditions: Option<bool>,
    /// Participants (team names)
    participants: Option<Vec<AzuroParticipant>>,
    /// Betting conditions with outcomes &  odds
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
    condition_id: Option<String>,
    status: Option<String>,
    outcomes: Option<Vec<AzuroOutcome>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzuroOutcome {
    outcome_id: Option<String>,
    /// Raw odds from the AMM pool (string representation of a large integer)
    /// Azuro stores odds as fixed-point: value / 10^12 gives decimal odds
    current_odds: Option<String>,
    /// Sort order: typically 1 = team1, 2 = team2
    sort_order: Option<i64>,
}

// ====================================================================
// Constants
// ====================================================================

const AZURO_POLYGON_SUBGRAPH: &str =
    "https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-polygon-v3";

const AZURO_GNOSIS_SUBGRAPH: &str =
    "https://thegraph.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-api-gnosis-v3";

/// Poll interval in seconds
const AZURO_POLL_INTERVAL_SECS: u64 = 30;

/// GraphQL query to fetch CS2 games with active conditions and their outcomes
const CS2_GAMES_QUERY: &str = r#"
{
  games(
    first: 50
    where: {
      sport_: { slug: "cs2" }
      status: Created
      hasActiveConditions: true
    }
    orderBy: startsAt
    orderDirection: asc
  ) {
    id
    title
    startsAt
    status
    hasActiveConditions
    participants(orderBy: sortOrder) {
      name
      sortOrder
    }
    conditions(
      first: 5
      where: { status: Created }
    ) {
      conditionId
      status
      outcomes(orderBy: sortOrder) {
        outcomeId
        currentOdds
        sortOrder
      }
    }
  }
}
"#;

// ====================================================================
// Poller implementation
// ====================================================================

/// Converts Azuro raw odds (fixed-point 10^12) to decimal odds
fn parse_azuro_odds(raw: &str) -> Option<f64> {
    let val: f64 = raw.parse().ok()?;
    let decimal = val / 1_000_000_000_000.0;
    // Sanity check: decimal odds should be >= 1.0
    if decimal >= 1.01 && decimal <= 100.0 {
        Some(decimal)
    } else {
        None
    }
}

/// Extract team names from AzuroGame, preferring participants over title
fn extract_teams(game: &AzuroGame) -> Option<(String, String)> {
    // Try participants first (ordered by sortOrder)
    if let Some(participants) = &game.participants {
        if participants.len() >= 2 {
            let t1 = participants[0].name.as_deref().unwrap_or("").trim();
            let t2 = participants[1].name.as_deref().unwrap_or("").trim();
            if !t1.is_empty() && !t2.is_empty() {
                return Some((t1.to_string(), t2.to_string()));
            }
        }
    }

    // Fallback: parse title "Team1 vs Team2" or "Team1 - Team2"
    if let Some(title) = &game.title {
        // Try " vs " first, then " - "
        for sep in [" vs ", " - "] {
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

/// Extract match winner odds (the first condition with exactly 2 outcomes)
fn extract_match_winner_odds(game: &AzuroGame) -> Option<(f64, f64)> {
    let conditions = game.conditions.as_ref()?;

    for cond in conditions {
        let outcomes = cond.outcomes.as_ref()?;
        if outcomes.len() == 2 {
            let odds1 = outcomes.iter()
                .find(|o| o.sort_order == Some(1))
                .and_then(|o| o.current_odds.as_ref())
                .and_then(|raw| parse_azuro_odds(raw));

            let odds2 = outcomes.iter()
                .find(|o| o.sort_order == Some(2))
                .and_then(|o| o.current_odds.as_ref())
                .and_then(|raw| parse_azuro_odds(raw));

            if let (Some(o1), Some(o2)) = (odds1, odds2) {
                return Some((o1, o2));
            }
        }
    }
    None
}

/// Poll a single Azuro subgraph endpoint for CS2 games
async fn poll_subgraph(
    client: &reqwest::Client,
    url: &str,
    chain_name: &str,
) -> Vec<(String, String, f64, f64, String)> {
    // Returns Vec of (team1, team2, odds1, odds2, game_id)
    let body = serde_json::json!({ "query": CS2_GAMES_QUERY });

    let resp = match client.post(url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("azuro {} fetch error: {}", chain_name, e);
            return vec![];
        }
    };

    let status = resp.status();
    if !status.is_success() {
        warn!("azuro {} HTTP {}", chain_name, status);
        return vec![];
    }

    let gql: GqlResponse = match resp.json().await {
        Ok(g) => g,
        Err(e) => {
            warn!("azuro {} parse error: {}", chain_name, e);
            return vec![];
        }
    };

    if let Some(errors) = &gql.errors {
        for e in errors {
            warn!("azuro {} GQL error: {}", chain_name, e.message);
        }
    }

    let games = match gql.data.and_then(|d| d.games) {
        Some(g) => g,
        None => {
            debug!("azuro {} no games returned", chain_name);
            return vec![];
        }
    };

    let mut results = Vec::new();

    for game in &games {
        let (team1, team2) = match extract_teams(game) {
            Some(t) => t,
            None => continue,
        };

        let (odds1, odds2) = match extract_match_winner_odds(game) {
            Some(o) => o,
            None => continue,
        };

        results.push((team1, team2, odds1, odds2, game.id.clone()));
    }

    results
}

/// Main polling loop — runs forever, injecting Azuro odds into FeedHubState
pub async fn run_azuro_poller(state: FeedHubState, db_tx: mpsc::Sender<DbMsg>) {
    info!("azuro poller starting — polling every {}s for CS2 odds", AZURO_POLL_INTERVAL_SECS);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("RustMiskoLive/1.0")
        .build()
        .expect("failed to create reqwest client");

    // Initial delay to let the rest of the system start
    tokio::time::sleep(Duration::from_secs(3)).await;

    loop {
        let now = Utc::now();
        let mut total_injected = 0usize;

        // Poll both Polygon and Gnosis subgraphs
        let (polygon_games, gnosis_games) = tokio::join!(
            poll_subgraph(&client, AZURO_POLYGON_SUBGRAPH, "polygon"),
            poll_subgraph(&client, AZURO_GNOSIS_SUBGRAPH, "gnosis"),
        );

        // Merge results — prefer Polygon if same match exists on both
        // Use match_key for dedup
        let mut seen_keys = std::collections::HashSet::new();
        let all_games: Vec<_> = polygon_games.iter().map(|g| ("polygon", g))
            .chain(gnosis_games.iter().map(|g| ("gnosis", g)))
            .collect();

        for (chain, (team1, team2, odds1, odds2, game_id)) in &all_games {
            let key = match_key("cs2", team1, team2);

            // Skip if we already processed this match from another chain
            if !seen_keys.insert(key.clone()) {
                continue;
            }

            let bookmaker_name = format!("azuro_{}", chain);

            let payload = OddsPayload {
                sport: "cs2".to_string(),
                bookmaker: bookmaker_name.clone(),
                market: "match_winner".to_string(),
                team1: team1.clone(),
                team2: team2.clone(),
                odds_team1: *odds1,
                odds_team2: *odds2,
                liquidity_usd: None, // Azuro AMM pool — TODO: extract from subgraph
                spread_pct: None,
                url: Some(format!("https://bookmaker.xyz/esports/cs2/{}", game_id)),
            };

            let odds_key = OddsKey {
                match_key: key.clone(),
                bookmaker: bookmaker_name.clone(),
            };

            // Inject into FeedHubState
            state.odds.write().await.insert(
                odds_key,
                OddsState {
                    source: format!("azuro_{}", chain),
                    seen_at: now,
                    payload: payload.clone(),
                },
            );

            // Log to DB
            let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            let _ = db_tx.try_send(DbMsg::OddsUpsert(DbOddsRow {
                ts: now,
                source: format!("azuro_{}", chain),
                sport: "cs2".to_string(),
                bookmaker: bookmaker_name,
                market: "match_winner".to_string(),
                team1: team1.clone(),
                team2: team2.clone(),
                match_key: key,
                odds_team1: *odds1,
                odds_team2: *odds2,
                liquidity_usd: None,
                spread_pct: None,
                payload_json,
            }));

            total_injected += 1;
        }

        if total_injected > 0 {
            info!(
                "azuro poll: injected {} CS2 odds entries (polygon={}, gnosis={})",
                total_injected,
                polygon_games.len(),
                gnosis_games.len()
            );
        } else {
            debug!(
                "azuro poll: 0 active CS2 games (polygon={}, gnosis={})",
                polygon_games.len(),
                gnosis_games.len()
            );
        }

        tokio::time::sleep(Duration::from_secs(AZURO_POLL_INTERVAL_SECS)).await;
    }
}
