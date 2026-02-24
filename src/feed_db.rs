use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::Path;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct DbConfig {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct DbIngestRow {
    pub ts: DateTime<Utc>,
    pub source: String,
    pub msg_type: String,
    pub ok: bool,
    pub note: String,
    pub raw_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DbLiveRow {
    pub ts: DateTime<Utc>,
    pub source: String,
    pub sport: String,
    pub team1: String,
    pub team2: String,
    pub match_key: String,
    pub payload_json: String,
}

#[derive(Debug, Clone)]
pub struct DbOddsRow {
    pub ts: DateTime<Utc>,
    pub source: String,
    pub sport: String,
    pub bookmaker: String,
    pub market: String,
    pub team1: String,
    pub team2: String,
    pub match_key: String,
    pub odds_team1: f64,
    pub odds_team2: f64,
    pub liquidity_usd: Option<f64>,
    pub spread_pct: Option<f64>,
    pub payload_json: String,
}

#[derive(Debug, Clone)]
pub struct DbFusionRow {
    pub ts: DateTime<Utc>,
    pub sport: String,
    pub match_key: String,
    pub live_source: String,
    pub odds_source: String,
    pub bookmaker: String,
    pub market: String,
    pub liquidity_usd: Option<f64>,
    pub spread_pct: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct DbHeartbeatRow {
    pub ts: DateTime<Utc>,
    pub connections: i64,
    pub live_items: i64,
    pub odds_items: i64,
    pub fused_ready: i64,
}

#[derive(Debug)]
pub enum DbMsg {
    Ingest(DbIngestRow),
    LiveUpsert(DbLiveRow),
    OddsUpsert(DbOddsRow),
    Fusion(DbFusionRow),
    Heartbeat(DbHeartbeatRow),
}

pub fn spawn_db_writer(cfg: DbConfig) -> mpsc::Sender<DbMsg> {
    let (tx, mut rx) = mpsc::channel::<DbMsg>(10_000);

    std::thread::spawn(move || {
        let result: Result<()> = (|| {
            let db_path = Path::new(&cfg.path);
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }

            let conn = Connection::open(db_path).context("open sqlite db")?;
            conn.pragma_update(None, "journal_mode", "WAL")
                .ok();
            conn.pragma_update(None, "synchronous", "NORMAL")
                .ok();

            init_schema(&conn)?;

            while let Some(msg) = rx.blocking_recv() {
                if let Err(e) = apply_msg(&conn, msg) {
                    // silent-ish: DB should not kill ingest pipeline
                    eprintln!("[feed-db] write failed: {e}");
                }
            }

            Ok(())
        })();

        if let Err(e) = result {
            eprintln!("[feed-db] fatal: {e}");
        }
    });

    tx
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS ingest_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            source TEXT NOT NULL,
            msg_type TEXT NOT NULL,
            ok INTEGER NOT NULL,
            note TEXT NOT NULL,
            raw_json TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_ingest_ts ON ingest_events(ts);

        CREATE TABLE IF NOT EXISTS live_state (
            match_key TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            source TEXT NOT NULL,
            sport TEXT NOT NULL,
            team1 TEXT NOT NULL,
            team2 TEXT NOT NULL,
            payload_json TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_live_sport ON live_state(sport);

        CREATE TABLE IF NOT EXISTS odds_state (
            match_key TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            source TEXT NOT NULL,
            sport TEXT NOT NULL,
            bookmaker TEXT NOT NULL,
            market TEXT NOT NULL,
            team1 TEXT NOT NULL,
            team2 TEXT NOT NULL,
            odds_team1 REAL NOT NULL,
            odds_team2 REAL NOT NULL,
            liquidity_usd REAL,
            spread_pct REAL,
            payload_json TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_odds_sport ON odds_state(sport);
        CREATE INDEX IF NOT EXISTS idx_odds_book ON odds_state(bookmaker);

        CREATE TABLE IF NOT EXISTS fusion_ready (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            sport TEXT NOT NULL,
            match_key TEXT NOT NULL,
            live_source TEXT NOT NULL,
            odds_source TEXT NOT NULL,
            bookmaker TEXT NOT NULL,
            market TEXT NOT NULL,
            liquidity_usd REAL,
            spread_pct REAL
        );

        CREATE INDEX IF NOT EXISTS idx_fusion_ts ON fusion_ready(ts);

        CREATE TABLE IF NOT EXISTS hub_heartbeat (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            connections INTEGER NOT NULL,
            live_items INTEGER NOT NULL,
            odds_items INTEGER NOT NULL,
            fused_ready INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_hb_ts ON hub_heartbeat(ts);
        "#,
    ).context("init schema")?;

    Ok(())
}

fn apply_msg(conn: &Connection, msg: DbMsg) -> Result<()> {
    match msg {
        DbMsg::Ingest(r) => {
            conn.execute(
                "INSERT INTO ingest_events(ts, source, msg_type, ok, note, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![r.ts.to_rfc3339(), r.source, r.msg_type, if r.ok { 1 } else { 0 }, r.note, r.raw_json],
            )?;
        }
        DbMsg::LiveUpsert(r) => {
            conn.execute(
                r#"
                INSERT INTO live_state(match_key, ts, source, sport, team1, team2, payload_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(match_key) DO UPDATE SET
                    ts=excluded.ts,
                    source=excluded.source,
                    sport=excluded.sport,
                    team1=excluded.team1,
                    team2=excluded.team2,
                    payload_json=excluded.payload_json
                "#,
                params![
                    r.match_key,
                    r.ts.to_rfc3339(),
                    r.source,
                    r.sport,
                    r.team1,
                    r.team2,
                    r.payload_json,
                ],
            )?;
        }
        DbMsg::OddsUpsert(r) => {
            conn.execute(
                r#"
                INSERT INTO odds_state(match_key, ts, source, sport, bookmaker, market, team1, team2, odds_team1, odds_team2, liquidity_usd, spread_pct, payload_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(match_key) DO UPDATE SET
                    ts=excluded.ts,
                    source=excluded.source,
                    sport=excluded.sport,
                    bookmaker=excluded.bookmaker,
                    market=excluded.market,
                    team1=excluded.team1,
                    team2=excluded.team2,
                    odds_team1=excluded.odds_team1,
                    odds_team2=excluded.odds_team2,
                    liquidity_usd=excluded.liquidity_usd,
                    spread_pct=excluded.spread_pct,
                    payload_json=excluded.payload_json
                "#,
                params![
                    r.match_key,
                    r.ts.to_rfc3339(),
                    r.source,
                    r.sport,
                    r.bookmaker,
                    r.market,
                    r.team1,
                    r.team2,
                    r.odds_team1,
                    r.odds_team2,
                    r.liquidity_usd,
                    r.spread_pct,
                    r.payload_json,
                ],
            )?;
        }
        DbMsg::Fusion(r) => {
            conn.execute(
                "INSERT INTO fusion_ready(ts, sport, match_key, live_source, odds_source, bookmaker, market, liquidity_usd, spread_pct) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    r.ts.to_rfc3339(),
                    r.sport,
                    r.match_key,
                    r.live_source,
                    r.odds_source,
                    r.bookmaker,
                    r.market,
                    r.liquidity_usd,
                    r.spread_pct,
                ],
            )?;
        }
        DbMsg::Heartbeat(r) => {
            conn.execute(
                "INSERT INTO hub_heartbeat(ts, connections, live_items, odds_items, fused_ready) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![r.ts.to_rfc3339(), r.connections, r.live_items, r.odds_items, r.fused_ready],
            )?;
        }
    }

    Ok(())
}

pub fn json_string(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}
