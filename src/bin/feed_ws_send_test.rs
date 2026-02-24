use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("FEED_HUB_URL").unwrap_or_else(|_| "ws://127.0.0.1:8080/feed".to_string());
    let source = std::env::var("FEED_SOURCE").unwrap_or_else(|_| "test".to_string());

    let (ws, _resp) = connect_async(&url)
        .await
        .with_context(|| format!("connect to {url}"))?;
    let (mut sink, mut stream) = ws.split();

    let ts = Utc::now().to_rfc3339();

    // 1) live_match
    let live = json!({
        "v": 1,
        "type": "live_match",
        "source": source,
        "ts": ts,
        "payload": {
            "sport": "cs2",
            "team1": "Alpha",
            "team2": "Beta",
            "score1": 1,
            "score2": 0,
            "status": "LIVE",
            "url": "https://example.invalid/match"
        }
    });

    sink.send(Message::Text(live.to_string().into())).await?;
    if let Some(msg) = stream.next().await {
        if let Ok(Message::Text(t)) = msg {
            println!("ack1={}", t);
        }
    }

    // 2) odds (passes gating by default)
    let odds = json!({
        "v": 1,
        "type": "odds",
        "source": "zebra",
        "ts": Utc::now().to_rfc3339(),
        "payload": {
            "sport": "cs2",
            "bookmaker": "pinnacle",
            "market": "match_winner",
            "team1": "Alpha",
            "team2": "Beta",
            "odds_team1": 1.95,
            "odds_team2": 1.95,
            "liquidity_usd": 5000.0,
            "spread_pct": 0.8,
            "url": "https://example.invalid/odds"
        }
    });

    sink.send(Message::Text(odds.to_string().into())).await?;
    if let Some(msg) = stream.next().await {
        if let Ok(Message::Text(t)) = msg {
            println!("ack2={}", t);
        }
    }

    // 3) heartbeat
    let hb = json!({
        "v": 1,
        "type": "heartbeat",
        "source": "client",
        "ts": Utc::now().to_rfc3339(),
        "payload": {}
    });

    sink.send(Message::Text(hb.to_string().into())).await?;
    if let Some(msg) = stream.next().await {
        if let Ok(Message::Text(t)) = msg {
            println!("ack3={}", t);
        }
    }

    let _ = sink.send(Message::Close(None)).await;
    Ok(())
}
