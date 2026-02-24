//! Telegram Alert Bot pro CS2 odds anomálie
//!
//! Standalone binary — polluje feed-hub /opportunities endpoint,
//! detekuje odds discrepancy mezi Azuro a trhem, posílá Telegram alerty.
//! Miša odpoví YES $X / NO a bot (budoucí fáze) umístí sázku.
//!
//! Spuštění:
//!   $env:TELEGRAM_BOT_TOKEN="7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk"
//!   $env:TELEGRAM_CHAT_ID="<tvoje chat id>"
//!   $env:FEED_HUB_URL="http://127.0.0.1:8081"
//!   cargo run --bin alert_bot

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn, error};
use tracing_subscriber::{EnvFilter, fmt};

// ====================================================================
// Config
// ====================================================================

const POLL_INTERVAL_SECS: u64 = 30;
/// Minimum edge % to trigger alert (all tiers)
const MIN_EDGE_PCT: f64 = 5.0;
/// Don't re-alert same match within this window
const ALERT_COOLDOWN_SECS: i64 = 300; // 5 min

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
}

fn find_odds_anomalies(state: &StateResponse) -> Vec<OddsAnomaly> {
    // Group odds by match_key
    let mut by_match: std::collections::HashMap<String, Vec<&StateOddsItem>> = std::collections::HashMap::new();
    for item in &state.odds {
        by_match.entry(item.match_key.clone()).or_default().push(item);
    }

    let mut anomalies = Vec::new();

    for (match_key, items) in &by_match {
        // Find Azuro odds and market odds
        let azuro_items: Vec<&&StateOddsItem> = items.iter().filter(|i| i.payload.bookmaker.starts_with("azuro_")).collect();
        let market_items: Vec<&&StateOddsItem> = items.iter().filter(|i| !i.payload.bookmaker.starts_with("azuro_")).collect();

        if azuro_items.is_empty() || market_items.is_empty() {
            continue; // Need both sides to compare
        }

        // Use first Azuro source and average market odds
        let azuro = &azuro_items[0].payload;
        
        // Average market odds across all non-azuro bookmakers
        let avg_w1: f64 = market_items.iter().map(|i| i.payload.odds_team1).sum::<f64>() / market_items.len() as f64;
        let avg_w2: f64 = market_items.iter().map(|i| i.payload.odds_team2).sum::<f64>() / market_items.len() as f64;

        let market_bookie = market_items.iter().map(|i| i.payload.bookmaker.as_str()).collect::<Vec<_>>().join("+");

        // Check discrepancy: Azuro odds vs market
        // If Azuro offers HIGHER odds than market → potential value on Azuro
        let disc_w1 = (azuro.odds_team1 / avg_w1 - 1.0) * 100.0;
        let disc_w2 = (azuro.odds_team2 / avg_w2 - 1.0) * 100.0;

        // Report the bigger discrepancy (both directions matter)
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
            });
        }
    }

    // Sort by discrepancy desc
    anomalies.sort_by(|a, b| b.discrepancy_pct.partial_cmp(&a.discrepancy_pct).unwrap_or(std::cmp::Ordering::Equal));
    anomalies
}

fn format_anomaly_alert(a: &OddsAnomaly) -> String {
    let value_team = if a.value_side == 1 { &a.team1 } else { &a.team2 };
    let azuro_odds = if a.value_side == 1 { a.azuro_w1 } else { a.azuro_w2 };
    let market_odds = if a.value_side == 1 { a.market_w1 } else { a.market_w2 };
    
    let url_line = a.azuro_url.as_ref()
        .map(|u| format!("\n🔗 <a href=\"{}\">Azuro link</a>", u))
        .unwrap_or_default();

    format!(
        "🎯 <b>ODDS ANOMALY</b>\n\
         \n\
         <b>{}</b> vs <b>{}</b>\n\
         \n\
         📊 Azuro ({}):\n\
         {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         \n\
         📊 Trh ({}):\n\
         {} <b>{:.2}</b> | {} <b>{:.2}</b>\n\
         \n\
         ⚡ <b>{}</b> na Azuru o <b>{:.1}%</b> VYŠŠÍ než trh\n\
         Azuro: {:.2} vs Trh: {:.2}\n\
         \n\
         💡 Suggestion: BET <b>{}</b> @ <b>{:.2}</b>{}\n\
         \n\
         Reply: <code>YES $5</code> / <code>NO</code> / <code>SKIP</code>",
        a.team1, a.team2,
        a.azuro_bookmaker,
        a.team1, a.azuro_w1, a.team2, a.azuro_w2,
        a.market_bookmaker,
        a.team1, a.market_w1, a.team2, a.market_w2,
        value_team, a.discrepancy_pct,
        azuro_odds, market_odds,
        value_team, azuro_odds, url_line
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

#[tokio::main]
async fn main() -> Result<()> {
    fmt().with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?)).init();

    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .unwrap_or_else(|_| "7611316975:AAG_bStGX283uHCdog96y".to_string());
    let feed_hub_url = std::env::var("FEED_HUB_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    
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
                                        "🤖 <b>RustMisko Alert Bot</b> activated!\n\n\
                                         Budu ti posílat CS2 odds anomálie z Azuro vs trh.\n\
                                         Odpověz <code>YES $5</code> pro sázku nebo <code>NO</code> pro skip.\n\n\
                                         ⚙️ Min edge: 5%\n\
                                         📡 Polling interval: 30s\n\
                                         🏠 Feed Hub: {}", feed_hub_url
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
    info!("Alert bot running. chat_id={}, feed_hub={}", chat_id, feed_hub_url);

    // Startup message
    tg_send_message(&client, &token, chat_id,
        "🟢 <b>Alert Bot Online</b>\n\n\
         Monitoruji Azuro vs 1xbit/HLTV odds discrepancy.\n\
         Pošlu alert když najdu >5% edge.\n\n\
         Commands:\n\
         /status — aktuální stav\n\
         /odds — top odds anomálie teď\n\
         /help — nápověda"
    ).await?;

    let mut poll_ticker = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = poll_ticker.tick() => {
                // === POLL feed-hub for anomalies ===
                
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
                                let anomalies = find_odds_anomalies(&state);
                                for anomaly in &anomalies {
                                    let alert_key = format!("{}:{}:{}", anomaly.match_key, anomaly.value_side, anomaly.azuro_bookmaker);
                                    if already_alerted.contains(&alert_key) {
                                        continue;
                                    }

                                    let msg = format_anomaly_alert(anomaly);
                                    if let Err(e) = tg_send_message(&client, &token, chat_id, &msg).await {
                                        error!("Failed to send alert: {}", e);
                                    } else {
                                        info!("Alert sent: {} side={} disc={:.1}%", anomaly.match_key, anomaly.value_side, anomaly.discrepancy_pct);
                                        sent_alerts.push(SentAlert {
                                            match_key: alert_key,
                                            sent_at: Utc::now(),
                                        });
                                    }
                                }

                                if anomalies.is_empty() {
                                    info!("Poll: {} odds items, 0 anomalies >{}%", state.odds_items, MIN_EDGE_PCT);
                                } else {
                                    info!("Poll: {} anomalies found, {} sent", anomalies.len(), 
                                        anomalies.len().saturating_sub(
                                            anomalies.iter().filter(|a| {
                                                let k = format!("{}:{}:{}", a.match_key, a.value_side, a.azuro_bookmaker);
                                                already_alerted.contains(&k)
                                            }).count()
                                        )
                                    );
                                }
                            }
                            Err(e) => warn!("Failed to parse /state: {}", e),
                        }
                    }
                    Err(e) => warn!("Failed to fetch /state: {}", e),
                }

                // 2. Also check /opportunities for arb_cross_book specifically
                match client.get(format!("{}/opportunities", feed_hub_url)).send().await {
                    Ok(resp) => {
                        match resp.json::<OpportunitiesResponse>().await {
                            Ok(opps) => {
                                for opp in &opps.opportunities {
                                    // Only alert arb_cross_book with significant edge
                                    if opp.opp_type != "arb_cross_book" { continue; }
                                    if opp.edge_pct < MIN_EDGE_PCT { continue; }
                                    
                                    let alert_key = format!("opp:{}:{}", opp.match_key, opp.bookmaker);
                                    if already_alerted.contains(&alert_key) { continue; }

                                    let msg = format_opportunity_alert(opp);
                                    if let Err(e) = tg_send_message(&client, &token, chat_id, &msg).await {
                                        error!("Failed to send opp alert: {}", e);
                                    } else {
                                        info!("Opp alert sent: {} edge={:.1}%", opp.match_key, opp.edge_pct);
                                        sent_alerts.push(SentAlert {
                                            match_key: alert_key,
                                            sent_at: Utc::now(),
                                        });
                                    }
                                }
                            }
                            Err(e) => warn!("Failed to parse /opportunities: {}", e),
                        }
                    }
                    Err(e) => warn!("Failed to fetch /opportunities: {}", e),
                }
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

                                match text {
                                    "/status" => {
                                        let status = match client.get(format!("{}/health", feed_hub_url)).send().await {
                                            Ok(r) => {
                                                let health = r.text().await.unwrap_or_default();
                                                match client.get(format!("{}/state", feed_hub_url)).send().await {
                                                    Ok(sr) => {
                                                        match sr.json::<StateResponse>().await {
                                                            Ok(s) => {
                                                                let azuro_count = s.odds.iter().filter(|o| o.payload.bookmaker.starts_with("azuro_")).count();
                                                                let market_count = s.odds.iter().filter(|o| !o.payload.bookmaker.starts_with("azuro_")).count();
                                                                format!(
                                                                    "📊 <b>Status</b>\n\n\
                                                                     Feed Hub: {}\n\
                                                                     Connections: {}\n\
                                                                     Live matches: {}\n\
                                                                     Azuro odds: {}\n\
                                                                     Market odds: {}\n\
                                                                     Fused (matchable): {}\n\
                                                                     Alerts sent: {} (cooldown {}s)",
                                                                    health, s.connections, s.live_items,
                                                                    azuro_count, market_count, s.fused_ready,
                                                                    sent_alerts.len(), ALERT_COOLDOWN_SECS
                                                                )
                                                            }
                                                            Err(_) => "Feed Hub /state error".to_string(),
                                                        }
                                                    }
                                                    Err(_) => format!("Feed Hub health: {} (state err)", health),
                                                }
                                            }
                                            Err(e) => format!("❌ Feed Hub offline: {}", e),
                                        };
                                        let _ = tg_send_message(&client, &token, chat_id, &status).await;
                                    }

                                    "/odds" => {
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
                                                            let msg = format!("📊 <b>Top {} anomálií:</b>\n\n{}", anomalies.len().min(5), summary);
                                                            let _ = tg_send_message(&client, &token, chat_id, &msg).await;
                                                            // Send top anomaly as full alert
                                                            if let Some(top) = anomalies.first() {
                                                                let _ = tg_send_message(&client, &token, chat_id, &format_anomaly_alert(top)).await;
                                                            }
                                                        }
                                                    }
                                                    Err(_) => { let _ = tg_send_message(&client, &token, chat_id, "❌ /state parse error").await; }
                                                }
                                            }
                                            Err(e) => { let _ = tg_send_message(&client, &token, chat_id, &format!("❌ Feed Hub offline: {}", e)).await; }
                                        }
                                    }

                                    "/help" => {
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            "🤖 <b>RustMisko Alert Bot</b>\n\n\
                                             Automaticky monitoruji Azuro vs trh (1xbit, HLTV).\n\
                                             Když najdu >5% odds discrepancy, pošlu alert.\n\n\
                                             <b>Commands:</b>\n\
                                             /status — stav systému\n\
                                             /odds — aktuální anomálie\n\
                                             /help — tato zpráva\n\n\
                                             <b>Na alert odpověz:</b>\n\
                                             <code>YES $5</code> — sázka $5 (budoucí fáze)\n\
                                             <code>NO</code> — skip\n\n\
                                             Všechny tiers CS2 zápasů jsou monitorovány."
                                        ).await;
                                    }

                                    t if t.to_uppercase().starts_with("YES") => {
                                        // Parse amount: "YES $5" or "YES 5"
                                        let amount_str = t[3..].trim().trim_start_matches('$').trim();
                                        let amount: f64 = amount_str.parse().unwrap_or(5.0);
                                        let _ = tg_send_message(&client, &token, chat_id,
                                            &format!(
                                                "🔧 <b>BET ACKNOWLEDGED</b>\n\
                                                 Amount: ${:.2} USDC\n\n\
                                                 ⚠️ Executor modul ještě není implementován.\n\
                                                 Toto bude: EIP712 sign → Azuro Relayer → on-chain bet.\n\
                                                 Prozatím: otevři Azuro link a vsaď manuálně.", amount
                                            )
                                        ).await;
                                    }

                                    "NO" | "no" | "SKIP" | "skip" => {
                                        let _ = tg_send_message(&client, &token, chat_id, "⏭️ Skipped.").await;
                                    }

                                    _ => {
                                        // Ignore unknown messages
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Quiet — might be network blip
                        if Utc::now().timestamp() % 60 == 0 {
                            warn!("getUpdates err: {}", e);
                        }
                    }
                }
            }
        }
    }
}
