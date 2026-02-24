use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

fn main() -> Result<()> {
    let db_path = std::env::var("FEED_DB_PATH").unwrap_or_else(|_| "data/feed.db".to_string());
    let conn = Connection::open(&db_path).with_context(|| format!("open db at {db_path}"))?;

    let tables = [
        "ingest_events",
        "live_state",
        "odds_state",
        "fusion_ready",
        "hub_heartbeat",
    ];

    println!("db_path={db_path}");
    for t in tables {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(1) FROM {t}"), [], |r| r.get(0))
            .with_context(|| format!("count {t}"))?;
        println!("{t}: {count}");
    }

    let last_hb: Option<(String, i64, i64, i64, i64)> = conn
        .query_row(
            "SELECT ts, connections, live_items, odds_items, fused_ready FROM hub_heartbeat ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .context("read last heartbeat")?;

    if let Some((ts, connections, live_items, odds_items, fused_ready)) = last_hb {
        println!(
            "last_heartbeat: ts={ts} connections={connections} live_items={live_items} odds_items={odds_items} fused_ready={fused_ready}"
        );
    } else {
        println!("last_heartbeat: <none>");
    }

    Ok(())
}
