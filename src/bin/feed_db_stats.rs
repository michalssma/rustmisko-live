use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Default)]
struct QueryArgs {
    from: Option<String>,
    to: Option<String>,
    contains: Option<String>,
    limit: i64,
}

fn parse_args() -> QueryArgs {
    let mut out = QueryArgs {
        limit: 200,
        ..Default::default()
    };

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => out.from = it.next(),
            "--to" => out.to = it.next(),
            "--contains" => out.contains = it.next(),
            "--limit" => {
                if let Some(v) = it.next() {
                    if let Ok(n) = v.parse::<i64>() {
                        out.limit = n.max(1).min(5000);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn run_stats(conn: &Connection, db_path: &str) -> Result<()> {
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

fn run_query(conn: &Connection, q: &QueryArgs) -> Result<()> {
    let from = q.from.as_deref().unwrap_or("0000-00-00T00:00:00");
    let to = q.to.as_deref().unwrap_or("9999-99-99T99:99:99");
    let contains = q.contains.as_deref().unwrap_or("");
    let like = format!("%{}%", contains);

    println!(
        "query: ingest_events ts in [{from}, {to}] contains='{}' limit={}",
        contains, q.limit
    );

    let mut stmt = conn
        .prepare(
            r#"
            SELECT ts, source, msg_type, ok, note, COALESCE(raw_json, '')
            FROM ingest_events
            WHERE ts >= ?1 AND ts <= ?2
              AND COALESCE(raw_json, '') LIKE ?3
            ORDER BY ts ASC
            LIMIT ?4
            "#,
        )
        .context("prepare query")?;

    let rows = stmt
        .query_map(params![from, to, like, q.limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .context("query_map")?;

    let mut n = 0usize;
    for row in rows {
        let (ts, source, msg_type, ok, note, raw_json) = row?;
        n += 1;
        println!("--- #{n} ts={ts} source={source} msg_type={msg_type} ok={ok} note={note}");
        if !raw_json.is_empty() {
            println!("{raw_json}");
        }
    }
    println!("query_rows={n}");
    Ok(())
}

fn main() -> Result<()> {
    let q = parse_args();
    let db_path = std::env::var("FEED_DB_PATH").unwrap_or_else(|_| "data/feed.db".to_string());
    let conn = Connection::open(&db_path).with_context(|| format!("open db at {db_path}"))?;

    if q.from.is_some() || q.to.is_some() || q.contains.is_some() {
        run_query(&conn, &q)
    } else {
        run_stats(&conn, &db_path)
    }
}
