//! feed-hub — WS ingest pro headful browser/Android feeds
//!
//! Cíl: přijímat realtime JSON z Lenovo (Tampermonkey) / Zebra (Android) a v Rustu
//! udržovat „co je LIVE“ + „kde jsou LIVE odds“, s gatingem a audit logy.
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

#[derive(Clone)]
struct FeedHubState {
    live: Arc<RwLock<HashMap<String, LiveMatchState>>>,
    odds: Arc<RwLock<HashMap<String, OddsState>>>,
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

fn normalize_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn match_key(sport: &str, team1: &str, team2: &str) -> String {
    format!(
        "{}::{}_vs_{}",
        sport.to_lowercase(),
        normalize_name(team1),
        normalize_name(team2)
    )
}

fn parse_ts(ts: &Option<String>) -> DateTime<Utc> {
    ts.as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn gate_odds(odds: &OddsPayload, seen_at: DateTime<Utc>) -> (bool, String) {
    let liquidity_ok = odds.liquidity_usd.map_or(false, |l| l >= 2000.0);
    let spread_ok = odds.spread_pct.map_or(false, |s| s <= 1.5);

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

async fn build_state_snapshot(state: &FeedHubState) -> HttpStateResponse {
    let connections = *state.connections.read().await;
    let live_map = state.live.read().await;
    let odds_map = state.odds.read().await;

    let live_items = live_map.len();
    let odds_items = odds_map.len();

    let mut fused_keys = Vec::new();
    for k in odds_map.keys() {
        if live_map.contains_key(k) {
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
            match_key: k.clone(),
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
    info!("feed-hub http listening on http://{} (GET /health, /state)", bind);

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

                                    state.odds.write().await.insert(
                                        key.clone(),
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
                    odds.keys().filter(|k| live.contains_key(*k)).count()
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
