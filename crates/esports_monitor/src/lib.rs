/// RustMiskoLive — Esports Monitor
///
/// Monitoruje vyřízené zápasy (CS2, LoL, Valorant) pomocí free zdrojů.
/// - LoL: esports-api.lolesports.com (API Key)
/// - Valorant: vlr.gg/matches/results (HTML Scraping)
/// - CS2: gosugamers.net/counter-strike/matches/results (HTML Scraping)
///
/// Hlásí "MATCH_RESOLVED" přes Logger.

use anyhow::{Context, Result};
use logger::{ApiStatusEvent, EventLogger, MatchResolvedEvent, SystemHeartbeatEvent, now_iso};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tracing::{debug, info, warn};

pub struct EsportsMonitor {
    client: reqwest::Client,
    logger: EventLogger,
    poll_interval_secs: u64,
    seen_matches: Mutex<HashSet<String>>,
}

impl EsportsMonitor {
    pub fn new(log_dir: impl Into<std::path::PathBuf>, poll_interval_secs: u64) -> Self {
        Self {
            client: reqwest::Client::builder()
                // Imitujeme prohlížeč kvůli anti-bot ochranám na parsovaných webech
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            logger: EventLogger::new(log_dir),
            poll_interval_secs,
            seen_matches: Mutex::new(HashSet::new()),
        }
    }

    pub async fn poll_all(&self) -> Vec<MatchResolvedEvent> {
        let mut healthy = 0;
        let total = 3; // CS2, LoL, Valorant
        let mut items = 0;
        let mut all_resolved = Vec::new();

        // 1. League of Legends (API)
        match self.poll_lol().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("lolesports", "leagueoflegends", &e.to_string()); }
        }

        // 2. Valorant (vlr.gg Scraping)
        match self.poll_valorant().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("vlrgg", "valorant", &e.to_string()); }
        }

        // 3. CS2 (GosuGamers Scraping)
        match self.poll_cs2().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("gosugamers", "counterstrike", &e.to_string()); }
        }

        // 4. Dota 2 (GosuGamers Scraping)
        match self.poll_dota2().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("gosugamers", "dota2", &e.to_string()); }
        }

        let heartbeat = SystemHeartbeatEvent {
            ts: now_iso(),
            event: "SYSTEM_HEARTBEAT",
            phase: "ESPORTS_LOGGING_ONLY".to_string(),
            poll_interval_secs: self.poll_interval_secs,
            overall_items: items,
            healthy_sources: healthy,
            total_sources: 4, // Upraveno na 4 zdroje
            pinnacle_items: 0,
            oddsapi_items: 0,
            total_items: items,
        };

        let _ = self.logger.log(&heartbeat);

        info!("Cycle completed. Logged {} newly resolved matches (healthy: {}/{}).", items, healthy, total);
        
        all_resolved
    }

    async fn poll_lol(&self) -> Result<Vec<MatchResolvedEvent>> {
        let url = "https://esports-api.lolesports.com/persisted/gw/getCompletedEvents?hl=en-US";
        
        let resp = self.client.get(url)
            .header("x-api-key", "0TvQnueqKa5mxJntVWt0w4LpLfEkrV1Ta8rQBb9Z")
            .send().await.context("LoL request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("LoL API failed {status}: {}", &body[..body.len().min(100)]);
            self.log_api_error("lolesports", "leagueoflegends", &format!("http_{status}"));
            return Ok(vec![]);
        }

        let raw = resp.text().await?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let events = parsed.pointer("/data/schedule/events").and_then(|v| v.as_array());

        let mut results = Vec::new();
        if let Some(event_list) = events {
            for ev in event_list.iter().take(15) { // Omezime se jen na poslednich 15 abychom nespamovali logy ze vcera
                let state = ev.pointer("/state").and_then(|s| s.as_str()).unwrap_or("");
                if state == "completed" {
                    let match_id = ev.pointer("/match/id").and_then(|i| i.as_str()).unwrap_or("?");
                    let t1 = ev.pointer("/match/teams/0/name").and_then(|n| n.as_str()).unwrap_or("T1");
                    let t2 = ev.pointer("/match/teams/1/name").and_then(|n| n.as_str()).unwrap_or("T2");
                    
                    let w1 = ev.pointer("/match/teams/0/result/outcome").and_then(|n| n.as_str()).unwrap_or("");
                    let winner = if w1 == "win" { t1.to_string() } else { t2.to_string() };

                    if let Some(ev) = self.log_resolved("leagueoflegends", match_id, t1, t2, &winner) {
                        results.push(ev);
                    }
                }
            }
        }
        self.log_api_ok("lolesports", "leagueoflegends", results.len());
        Ok(results)
    }

    async fn poll_valorant(&self) -> Result<Vec<MatchResolvedEvent>> {
        let url = "https://www.vlr.gg/matches/results";
        let resp = self.client.get(url).send().await.context("VLR request failed")?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("VLR HTTP {}", resp.status()));
        }

        let html = resp.text().await?;
        let document = Html::parse_document(&html);
        let match_selector = Selector::parse("a.match-item").unwrap();
        let team_selector = Selector::parse(".match-item-vs-team-name").unwrap();
        let score_selector = Selector::parse(".match-item-vs-team-score").unwrap();

        let mut results = Vec::new();
        for node in document.select(&match_selector).take(15) {
            let teams: Vec<_> = node.select(&team_selector).map(|t| t.text().collect::<String>().trim().to_string()).collect();
            let scores: Vec<_> = node.select(&score_selector).map(|s| s.text().collect::<String>().trim().to_string()).collect();

            if teams.len() == 2 && scores.len() == 2 {
                let s1: i32 = scores[0].parse().unwrap_or(0);
                let s2: i32 = scores[1].parse().unwrap_or(0);
                
                // Zajistime ze nekdo vazne vyhral
                if s1 != s2 {
                    let winner = if s1 > s2 { &teams[0] } else { &teams[1] };
                    let match_id = teams[0].clone() + "_vs_" + &teams[1];
                    if let Some(ev) = self.log_resolved("valorant", &match_id, &teams[0], &teams[1], winner) {
                        results.push(ev);
                    }
                }
            }
        }
        self.log_api_ok("vlrgg", "valorant", results.len());
        Ok(results)
    }

    async fn poll_cs2(&self) -> Result<Vec<MatchResolvedEvent>> {
        let url = "https://www.gosugamers.net/counter-strike/matches/results";
        let resp = self.client.get(url).send().await.context("GosuGamers request failed")?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("GosuGamers HTTP {}", resp.status()));
        }

        let html = resp.text().await?;
        let document = Html::parse_document(&html);
        // Omezime se na zapasy s "Match is over" v title atributu uvnitr status tagu
        // Na gosugamers maji radky .match-list-item  
        let match_selector = Selector::parse(".match-list-item").unwrap();
        let name_selector = Selector::parse(".team-name").unwrap();
        let score_selector = Selector::parse(".score").unwrap();

        let mut results = Vec::new();
        for node in document.select(&match_selector).take(15) {
            let teams: Vec<_> = node.select(&name_selector).map(|t| t.text().collect::<String>().trim().to_string()).collect();
            let scores: Vec<_> = node.select(&score_selector).map(|s| s.text().collect::<String>().trim().to_string()).collect();

            if teams.len() >= 2 && scores.len() >= 2 {
                let s1: i32 = scores[0].parse().unwrap_or(0);
                let s2: i32 = scores[1].parse().unwrap_or(0);

                if s1 != s2 {
                    let winner = if s1 > s2 { &teams[0] } else { &teams[1] };
                    let match_id = teams[0].clone() + "_vs_" + &teams[1];
                    if let Some(ev) = self.log_resolved("counterstrike", &match_id, &teams[0], &teams[1], winner) {
                        results.push(ev);
                    }
                }
            }
        }

        self.log_api_ok("gosugamers", "counterstrike", results.len());
        Ok(results)
    }

    async fn poll_dota2(&self) -> Result<Vec<MatchResolvedEvent>> {
        let url = "https://www.gosugamers.net/dota2/matches/results";
        let resp = self.client.get(url).send().await.context("GosuGamers dota2 request failed")?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("GosuGamers DOTA HTTP {}", resp.status()));
        }

        let html = resp.text().await?;
        let document = Html::parse_document(&html);
        let match_selector = Selector::parse(".match-list-item").unwrap();
        let name_selector = Selector::parse(".team-name").unwrap();
        let score_selector = Selector::parse(".score").unwrap();

        let mut results = Vec::new();
        for node in document.select(&match_selector).take(15) {
            let teams: Vec<_> = node.select(&name_selector).map(|t| t.text().collect::<String>().trim().to_string()).collect();
            let scores: Vec<_> = node.select(&score_selector).map(|s| s.text().collect::<String>().trim().to_string()).collect();

            if teams.len() >= 2 && scores.len() >= 2 {
                let s1: i32 = scores[0].parse().unwrap_or(0);
                let s2: i32 = scores[1].parse().unwrap_or(0);

                if s1 != s2 {
                    let winner = if s1 > s2 { &teams[0] } else { &teams[1] };
                    let match_id = teams[0].clone() + "_vs_" + &teams[1];
                    if let Some(ev) = self.log_resolved("dota2", &match_id, &teams[0], &teams[1], winner) {
                        results.push(ev);
                    }
                }
            }
        }

        Ok(results)
    }

    fn log_resolved(&self, sport: &str, m_id: &str, t1: &str, t2: &str, winner: &str) -> Option<MatchResolvedEvent> {
        let unique_key = format!("{}_{}", sport, m_id);
        {
            let mut seen = self.seen_matches.lock().unwrap();
            if !seen.insert(unique_key) {
                return None; // Already logged and processed
            }
        }

        let ev = MatchResolvedEvent {
            ts: now_iso(),
            event: "MATCH_RESOLVED",
            sport: sport.to_string(),
            match_name: m_id.to_string(),
            home: t1.to_string(),
            away: t2.to_string(),
            winner: winner.to_string(),
            ended_at: now_iso(), // Jelikož už taháme jen nedávno ukončené
        };
        let _ = self.logger.log(&ev);
        Some(ev)
    }

    fn log_api_error(&self, source: &str, sport: &str, msg: &str) {
        let _ = self.logger.log(&ApiStatusEvent {
            ts: now_iso(),
            event: "API_STATUS",
            source: source.to_string(),
            scope: sport.to_string(),
            ok: false,
            status_code: None,
            message: msg.to_string(),
            items_logged: 0,
        });
    }

    fn log_api_ok(&self, source: &str, sport: &str, count: usize) {
        let _ = self.logger.log(&ApiStatusEvent {
            ts: now_iso(),
            event: "API_STATUS",
            source: source.to_string(),
            scope: sport.to_string(),
            ok: true,
            status_code: Some(200),
            message: "ok".to_string(),
            items_logged: count,
        });
    }
}
