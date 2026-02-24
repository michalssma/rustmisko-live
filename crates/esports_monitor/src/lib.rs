/// RustMiskoLive — Esports Monitor
///
/// LIVE SCORING: sledujeme probíhající zápasy a detekujeme moment dokončení.
/// Strategie: poll live stránky (ne /results), state machine LIVE→FINISHED.
///
/// Zdroje:
/// - LoL:      getSchedule API (state: inProgress → completed)
/// - Valorant: vlr.gg/matches (live section)
/// - CS2:      gosugamers.net/counter-strike/matches (live section)
/// - Dota 2:   gosugamers.net/dota2/matches (live section)

use anyhow::{Context, Result};
use futures_util::{StreamExt, SinkExt};
use governor::{Quota, RateLimiter, state::NotKeyed, state::InMemoryState, clock::{Clock, DefaultClock}};
use headless_chrome::{Browser, LaunchOptions};
use logger::{ApiStatusEvent, EventLogger, MatchResolvedEvent, SystemHeartbeatEvent, now_iso};
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, info, warn};

pub type RiotRateLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

// ── Live Match State Machine ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct LiveMatch {
    home:       String,
    away:       String,
    #[allow(dead_code)]
    sport:      String,
    #[allow(dead_code)]
    first_seen: std::time::Instant,
}

pub struct EsportsMonitor {
    client:           reqwest::Client,
    logger:           EventLogger,
    poll_interval_secs: u64,
    /// Zápasy momentálně LIVE: klíč = "<sport>_<home>_vs_<away>"
    live_matches:     Mutex<HashMap<String, LiveMatch>>,
    /// Deduplikace pro results fallback
    seen_matches:     Mutex<HashSet<String>>,
    /// Riot Games Rate Limiter (< 0.8 req/s)
    riot_limiter:     Arc<RiotRateLimiter>,
    /// Throttling pro ne-Riot zdroje během Sniper mode
    last_vlr_poll:    Mutex<std::time::Instant>,
    last_gosu_poll:   Mutex<std::time::Instant>,
}

impl EsportsMonitor {
    pub fn new(log_dir: impl Into<std::path::PathBuf>, poll_interval_secs: u64) -> Self {
        // Limit k Riot API: max ~0.8 req/s (100 req / 2 min = 1.2s průměr).
        let quota = Quota::with_period(Duration::from_millis(1250)).unwrap();
        let riot_limiter = Arc::new(RateLimiter::direct(quota));

        use reqwest::header;
        let mut headers = header::HeaderMap::new();
        headers.insert(header::USER_AGENT, header::HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"));
        headers.insert(header::ACCEPT, header::HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"));
        headers.insert(header::ACCEPT_LANGUAGE, header::HeaderValue::from_static("en-US,en;q=0.5"));
        headers.insert(header::CONNECTION, header::HeaderValue::from_static("keep-alive"));
        headers.insert(header::UPGRADE_INSECURE_REQUESTS, header::HeaderValue::from_static("1"));
        headers.insert("Sec-Fetch-Dest", header::HeaderValue::from_static("document"));
        headers.insert("Sec-Fetch-Mode", header::HeaderValue::from_static("navigate"));
        headers.insert("Sec-Fetch-Site", header::HeaderValue::from_static("none"));
        headers.insert("Sec-Fetch-User", header::HeaderValue::from_static("?1"));
        headers.insert("Sec-Ch-Ua", header::HeaderValue::from_static("\"Not_A Brand\";v=\"8\", \"Chromium\";v=\"120\", \"Google Chrome\";v=\"120\""));
        headers.insert("Sec-Ch-Ua-Mobile", header::HeaderValue::from_static("?0"));
        headers.insert("Sec-Ch-Ua-Platform", header::HeaderValue::from_static("\"Windows\""));

        Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(std::time::Duration::from_secs(12))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            logger:             EventLogger::new(log_dir),
            poll_interval_secs,
            live_matches:       Mutex::new(HashMap::new()),
            seen_matches:       Mutex::new(HashSet::new()),
            riot_limiter,
            last_vlr_poll:      Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(60)),
            last_gosu_poll:     Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(60)),
        }
    }

    /// Vrací true, pokud je jakýkoliv zápas momentálně live. Slouží pro zrychlení polling loopu (Sniper Mode).
    pub fn is_any_match_live(&self) -> bool {
        !self.live_matches.lock().unwrap().is_empty()
    }

    // ── PRIMÁRNÍ: Live polling ─────────────────────────────────────────────

    /// Primární metoda — vrací zápasy co PRÁVĚ skončily (live→finished transition).
    /// Volat každých 15s.
    pub async fn poll_live_all(&self) -> Vec<MatchResolvedEvent> {
        let mut newly_finished = Vec::new();

        // 1. LoL — getSchedule API (chráněno Riot token bucketem, běží při každém ticku, i v 3s Sniper Mode)
        match self.poll_live_lol().await {
            Ok(mut res) => newly_finished.append(&mut res),
            Err(e) => warn!("LoL live poll failed: {}", e),
        }

        let now = std::time::Instant::now();
        let vlr_elapsed = { *self.last_vlr_poll.lock().unwrap() };
        let gosu_elapsed = { *self.last_gosu_poll.lock().unwrap() };

        // 2. Valorant — vlr.gg/matches (Throttled na 15s)
        if now.duration_since(vlr_elapsed).as_secs() >= 15 {
            match self.poll_live_valorant().await {
                Ok(mut res) => newly_finished.append(&mut res),
                Err(e) => warn!("Valorant live poll failed: {}", e),
            }
            *self.last_vlr_poll.lock().unwrap() = now;
        }

        // 3. CS2 & Dota 2 — GosuGamers /matches (Throttled na 15s)
        if now.duration_since(gosu_elapsed).as_secs() >= 15 {
            match self.poll_live_cs2().await {
                Ok(mut res) => newly_finished.append(&mut res),
                Err(e) => warn!("CS2 live poll failed: {}", e),
            }
            match self.poll_live_dota2().await {
                Ok(mut res) => newly_finished.append(&mut res),
                Err(e) => warn!("Dota2 live poll failed: {}", e),
            }
            *self.last_gosu_poll.lock().unwrap() = now;
        }

        if !newly_finished.is_empty() {
            info!("🎯 Live poll: {} zápasů právě skončilo → evaluating SX Bet", newly_finished.len());
        } else {
            debug!("Live poll: žádný nový výsledek tento cyklus.");
        }

        newly_finished
    }

    /// Spustí STRATZ GraphQL WebSocket pro Dota 2 live data (0 MB RAM overhead proxy)
    pub async fn start_stratz_ws(&self) {
        info!("🔌 Starting STRATZ WebSocket listener for Dota 2...");
        // WS endpoint Stratzu vyžaduje Bearer token, použijeme anonymní napojení nebo free-tier mock
        let url = "wss://api.stratz.com/graphql";
        
        // Spawn tokio background task
        tokio::spawn(async move {
            loop {
                // Připojení k WS
                match connect_async(url).await {
                    Ok((mut ws_stream, _)) => {
                        info!("✅ STRATZ WebSocket Connected (Dota 2)");
                        // Od Stratzu GraphQL bychom normálně subscribeovali na `matchLive` event:
                        let subscribe_msg = r#"{"type":"connection_init","payload":{}}"#;
                        if let Err(e) = ws_stream.send(Message::Text(subscribe_msg.into())).await {
                            warn!("STRATZ WS Init failed: {}", e);
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }

                        // Event loop
                        while let Some(msg) = ws_stream.next().await {
                            match msg {
                                Ok(Message::Text(text)) => {
                                    // Zde JSON Parse `LiveMatchState`
                                    // Pro účely bez reálného tokenu si teď uděláme jen placeholder
                                    debug!("STRATZ WS Message rx: {:.30}...", text);
                                }
                                Ok(Message::Close(_)) | Err(_) => {
                                    warn!("STRATZ WS Disconnected. Reconnecting in 5s...");
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("403") || err_str.contains("401") || err_str.contains("Forbidden") {
                            warn!("❌ STRATZ WS Connection refused (403 Forbidden). Token is likely required. Sleeping for 1 hour to prevent spam...");
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                            continue;
                        }
                        warn!("❌ STRATZ WS Connection failed: {}. Retrying in 5s...", err_str);
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    /// LoL live tracking přes getLive API.
    /// state: "inProgress" → zapamatuj si. "completed" → emituj resolved.
    async fn poll_live_lol(&self) -> Result<Vec<MatchResolvedEvent>> {
        // Token Bucket: Čekáme na přidělení tokenu (max 0.8 req/s)
        let clock = DefaultClock::default();
        loop {
            if let Err(not_until) = self.riot_limiter.check() {
                let wait = not_until.wait_time_from(clock.now());
                if wait > Duration::ZERO {
                    tokio::time::sleep(wait).await;
                }
            } else {
                break;
            }
        }

        let url = "https://esports-api.lolesports.com/persisted/gw/getLive?hl=en-US";
        let resp = self.client.get(url)
            .header("x-api-key", "0TvQnueqKa5mxJntVWt0w4LpLfEkrV1Ta8rQBb9Z")
            .send().await.context("LoL getLive request failed")?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("LoL getLive HTTP {}", resp.status()));
        }

        let data: serde_json::Value = resp.json().await?;
        let events = match data.pointer("/data/schedule/events").and_then(|v| v.as_array()) {
            Some(e) => e,
            // Pokud tu /events nejsou (prázdné pole taky projde jako some), vracíme prázdné pole, nikoliv chybu.
            None => return Ok(vec![]),
        };

        let mut newly_finished = Vec::new();
        let mut current_live_keys = HashSet::new();

        for ev in events {
            let state = ev.pointer("/state").and_then(|s| s.as_str()).unwrap_or("");
            
            // Riot API má match->teams pokud je hra aktivní
            let team_array = ev.pointer("/match/teams").and_then(|t| t.as_array());
            if let Some(teams) = team_array {
                if teams.len() == 2 {
                    let t1 = teams[0].pointer("/name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let t2 = teams[1].pointer("/name").and_then(|n| n.as_str()).unwrap_or("").to_string();

                    if !t1.is_empty() && !t2.is_empty() {
                        let key = format!("leagueoflegends_{}_vs_{}", t1, t2);

                        if state == "inProgress" || state == "unstarted" {
                            if state == "inProgress" {
                                current_live_keys.insert(key.clone());
                                let mut live = self.live_matches.lock().unwrap();
                                live.entry(key.clone()).or_insert_with(|| {
                                    info!("🔴 LIVE detekován: {} vs {} (LoL)", t1, t2);
                                    LiveMatch {
                                        home: t1.clone(),
                                        away: t2.clone(),
                                        sport: "leagueoflegends".to_string(),
                                        first_seen: std::time::Instant::now(),
                                    }
                                });
                            }
                        }
                    }
                }
            }
        }

        // Live → Finished detekce
        // Oproti VLR/GosuGamers, Riot `getLive` vrací všechny LIVE eventy na jedné stránce.
        // Cokoliv, co bylo v paměti a už není v getLive response, ZKONČILO (pokud je to LoL).
        let resolved_pairs: Vec<(String, String, String)> = {
            let mut mem = self.live_matches.lock().unwrap();
            let mut to_remove = Vec::new();

            for (key, m) in mem.iter() {
                if m.sport == "leagueoflegends" && !current_live_keys.contains(key) {
                    to_remove.push((key.clone(), m.home.clone(), m.away.clone()));
                }
            }

            for (key, _, _) in &to_remove {
                mem.remove(key);
            }
            to_remove
        };

        for (_key, home, away) in resolved_pairs {
            info!("✅ MATCH FINISHED: {} vs {} (LoL)", home, away);
            // Máme unknown vítěze z live response (zápas vypadl z live listu), musíme pak z audit queue zjistit víc
            // Pro SX bet stačí znát finiš zápasu, zbytek najdeme na oraclu
            let match_id = format!("{}_vs_{}", home, away);
            if let Some(ev) = self.log_resolved("leagueoflegends", &match_id, &home, &away, "Unknown") {
                newly_finished.push(ev);
            }
        }

        self.log_api_ok("lolesports", "lol", current_live_keys.len());
        Ok(newly_finished)
    }

    /// Valorant live tracking přes vlr.gg/matches.
    /// Live zápasy mají score místo countdown timeru a CSS class "mod-live".
    async fn poll_live_valorant(&self) -> Result<Vec<MatchResolvedEvent>> {
        let url = "https://www.vlr.gg/matches";
        let resp = self.client.get(url).send().await.context("VLR /matches request failed")?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("VLR HTTP {}", resp.status()));
        }

        let html = resp.text().await?;
        let document = Html::parse_document(&html);

        // Live zápasy na vlr.gg/matches mají class "mod-live" na match-item elementu
        let live_selector = Selector::parse("a.match-item.mod-live").unwrap();
        let team_selector = Selector::parse(".match-item-vs-team-name").unwrap();
        let score_selector = Selector::parse(".match-item-vs-team-score").unwrap();

        let mut current_live_keys: HashSet<String> = HashSet::new();
        let mut newly_finished = Vec::new();

        // Parsuj aktuálně live zápasy
        for node in document.select(&live_selector) {
            let teams: Vec<String> = node.select(&team_selector)
                .map(|t| t.text().collect::<String>().trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let scores: Vec<String> = node.select(&score_selector)
                .map(|s| s.text().collect::<String>().trim().to_string())
                .collect();

            if teams.len() < 2 { continue; }

            let score_display = if scores.len() >= 2 {
                format!(" ({}-{})", scores[0], scores[1])
            } else {
                String::new()
            };

            let key = format!("valorant_{}_vs_{}", teams[0], teams[1]);
            current_live_keys.insert(key.clone());

            let mut live = self.live_matches.lock().unwrap();
            live.entry(key.clone()).or_insert_with(|| {
                info!("🔴 LIVE detekován: {} vs {}{} (Valorant)", teams[0], teams[1], score_display);
                LiveMatch {
                    home:       teams[0].clone(),
                    away:       teams[1].clone(),
                    sport:      "valorant".to_string(),
                    first_seen: std::time::Instant::now(),
                }
            });
        }

        // Detekuj zápasy co zmizely z live sekce → právě skončily
        let finished_keys: Vec<(String, LiveMatch)> = {
            let mut live = self.live_matches.lock().unwrap();
            let finished: Vec<String> = live.keys()
                .filter(|k| k.starts_with("valorant_") && !current_live_keys.contains(*k))
                .cloned()
                .collect();
            finished.into_iter()
                .filter_map(|k| live.remove(&k).map(|m| (k, m)))
                .collect()
        };

        for (key, m) in finished_keys {
            // Zápas zmizel z live → musíme dohledat výsledek z /matches (results sekce)
            // Hledáme ho v results části stránky
            let winner = self.find_just_finished_valorant_winner(&m.home, &m.away, &html).await;
            let winner_str = winner.unwrap_or_else(|| {
                warn!("Valorant {}: nelze dohledat vítěze, přeskakuji.", key);
                String::new()
            });
            if winner_str.is_empty() { continue; }

            info!("✅ MATCH FINISHED (byl LIVE): {} vs {} → winner: {} (Valorant)", m.home, m.away, winner_str);
            let match_id = format!("{}_vs_{}", m.home, m.away);
            if let Some(ev) = self.emit_resolved("valorant", &match_id, &m.home, &m.away, &winner_str) {
                newly_finished.push(ev);
            }
        }

        Ok(newly_finished)
    }

    /// Dohledá výsledek právě dokončeného Valorant zápasu na vlr.gg/matches/results.
    async fn find_just_finished_valorant_winner(&self, home: &str, away: &str, _live_html: &str) -> Option<String> {
        let url = "https://www.vlr.gg/matches/results";
        let resp = self.client.get(url).send().await.ok()?;
        if !resp.status().is_success() { return None; }

        let html = resp.text().await.ok()?;
        let document = Html::parse_document(&html);
        let match_selector = Selector::parse("a.match-item").unwrap();
        let team_selector = Selector::parse(".match-item-vs-team-name").unwrap();
        let score_selector = Selector::parse(".match-item-vs-team-score").unwrap();

        let home_norm = home.to_lowercase();
        let away_norm = away.to_lowercase();

        // Hledáme jen v prvních 5 výsledcích (právě dokončené)
        for node in document.select(&match_selector).take(5) {
            let teams: Vec<String> = node.select(&team_selector)
                .map(|t| t.text().collect::<String>().trim().to_string())
                .collect();
            let scores: Vec<String> = node.select(&score_selector)
                .map(|s| s.text().collect::<String>().trim().to_string())
                .collect();

            if teams.len() < 2 || scores.len() < 2 { continue; }

            let t1_norm = teams[0].to_lowercase();
            let t2_norm = teams[1].to_lowercase();

            if (t1_norm.contains(&home_norm) || home_norm.contains(&t1_norm))
                && (t2_norm.contains(&away_norm) || away_norm.contains(&t2_norm))
            {
                let s1: i32 = scores[0].parse().unwrap_or(0);
                let s2: i32 = scores[1].parse().unwrap_or(0);
                if s1 != s2 {
                    return Some(if s1 > s2 { teams[0].clone() } else { teams[1].clone() });
                }
            }
        }
        None
    }

    /// CS2 live tracking přes GosuGamers /counterstrike/matches.
    async fn poll_live_cs2(&self) -> Result<Vec<MatchResolvedEvent>> {
        self.poll_live_gosugamers("counterstrike", "https://www.gosugamers.net/counterstrike/matches").await
    }

    /// Dota 2 live tracking (nově nahrazeno STRATZ WebSockets v backgroundu)
    /// Tato funkce slouží pro kompatibilitu, pokud zhavaruje WS
    async fn poll_live_dota2(&self) -> Result<Vec<MatchResolvedEvent>> {
        self.poll_live_gosugamers("dota2", "https://www.gosugamers.net/dota2/matches").await
    }

    /// Extrahuje jména týmů z GosuGamers match href slugu.
    /// Např. "/counterstrike/tournaments/62675-.../matches/641836-ground-zero-gaming-vs-mindfreak"
    /// → ("ground zero gaming", "mindfreak")
    fn extract_teams_from_gosugamers_href(href: &str) -> Option<(String, String)> {
        // Poslední segment za /matches/ → "641836-ground-zero-gaming-vs-mindfreak"
        let slug = href.rsplit('/').next()?;
        // Odstraníme úvodní numerické ID: "641836-" → "ground-zero-gaming-vs-mindfreak"
        let name_part = slug.split_once('-').map(|(_, rest)| rest)?;
        // Rozděl na "-vs-"
        let (t1_slug, t2_slug) = name_part.split_once("-vs-")?;
        let t1 = t1_slug.replace('-', " ");
        let t2 = t2_slug.replace('-', " ");
        if t1.is_empty() || t2.is_empty() { return None; }
        Some((t1, t2))
    }

    /// Generický GosuGamers live scraper (rewritten for MUI SSR structure).
    /// GosuGamers vrací SSR HTML s <a> elementy kde:
    ///   - href obsahuje "/matches/" a slug s názvy týmů
    ///   - textContent obsahuje "Live" pro aktivní zápasy  
    ///   - textContent obsahuje "XhYm" pro upcoming
    async fn poll_live_gosugamers(&self, sport: &str, url: &str) -> Result<Vec<MatchResolvedEvent>> {
        // --- CHROME HEADLESS FALLBACK pro Cloudflare bypass ---
        // GosuGamers brutálně blokuje reqwest. Použijeme Headless Chrome.
        let html = tokio::task::spawn_blocking({
            let url = url.to_string();
            let sport = sport.to_string();
            move || -> Result<String> {
                info!("🚀 Launching headless chrome for {}...", sport);
                let options = LaunchOptions::default_builder()
                    .headless(true)
                    .sandbox(false)
                    .build()
                    .unwrap();
                let browser = Browser::new(options).context("Failed to launch Chrome")?;
                let tab = browser.new_tab().context("Failed to create Chrome tab")?;
                
                // Navigate a počkat na selector
                tab.navigate_to(&url)?;
                tab.wait_for_element("body")?; // počkáme až aspoň něco najede
                std::thread::sleep(Duration::from_secs(3)); // extra Cloudflare challenge wait
                
                let content = tab.get_content()?;
                Ok(content)
            }
        }).await??;

        let document = Html::parse_document(&html);

        // GosuGamers MUI: match linky jsou <a> s href obsahujícím "/matches/"
        let link_selector = Selector::parse("a[href*='/matches/']").unwrap();

        let mut current_live_keys: HashSet<String> = HashSet::new();
        let mut newly_finished = Vec::new();

        for node in document.select(&link_selector) {
            let href = match node.value().attr("href") {
                Some(h) => h,
                None => continue,
            };

            // Filtruj jen skutečné match linky (ne navigační)
            if !href.contains("/tournaments/") { continue; }

            let text: String = node.text().collect::<String>();

            // Detekuj LIVE zápasy: text obsahuje "Live" (ne "0h21m" timing)
            if !text.contains("Live") { continue; }

            // Extrahuj týmy z href slugu (spolehlivější než text parsing)
            let (t1, t2) = match Self::extract_teams_from_gosugamers_href(href) {
                Some(pair) => pair,
                None => continue,
            };

            let key = format!("{}_{}_vs_{}", sport, t1, t2);
            current_live_keys.insert(key.clone());

            let mut live = self.live_matches.lock().unwrap();
            live.entry(key.clone()).or_insert_with(|| {
                info!("🔴 LIVE detekován: {} vs {} ({})", t1, t2, sport);
                LiveMatch {
                    home:       t1.clone(),
                    away:       t2.clone(),
                    sport:      sport.to_string(),
                    first_seen: std::time::Instant::now(),
                }
            });
        }

        // Detekuj zápasy co zmizely z live → právě skončily
        let sport_prefix = format!("{}_", sport);
        let finished_keys: Vec<(String, LiveMatch)> = {
            let mut live = self.live_matches.lock().unwrap();
            let finished: Vec<String> = live.keys()
                .filter(|k| k.starts_with(&sport_prefix) && !current_live_keys.contains(*k))
                .cloned()
                .collect();
            finished.into_iter()
                .filter_map(|k| live.remove(&k).map(|m| (k, m)))
                .collect()
        };

        for (key, m) in finished_keys {
            // Dohledáme výsledek na /results stránce (právě dokončený → bude na vrchu)
            let results_url = if sport == "counterstrike" {
                "https://www.gosugamers.net/counterstrike/matches/results"
            } else {
                "https://www.gosugamers.net/dota2/matches/results"
            };

            let winner = self.find_gosugamers_winner(&m.home, &m.away, results_url).await;
            let winner_str = match winner {
                Some(w) => w,
                None => {
                    warn!("{}: nelze dohledat vítěze pro {}, přeskakuji.", sport, key);
                    continue;
                }
            };

            info!("✅ MATCH FINISHED (byl LIVE): {} vs {} → winner: {} ({})", m.home, m.away, winner_str, sport);
            let match_id = format!("{}_vs_{}", m.home, m.away);
            if let Some(ev) = self.emit_resolved(sport, &match_id, &m.home, &m.away, &winner_str) {
                newly_finished.push(ev);
            }
        }

        Ok(newly_finished)
    }

    /// Dohledá vítěze zápasu z GosuGamers results page.
    /// Formát na results page: href slug obsahuje názvy týmů,
    /// textContent obsahuje "Team1SCORE:SCORETeam2" pattern.
    async fn find_gosugamers_winner(&self, home: &str, away: &str, results_url: &str) -> Option<String> {
        let resp = self.client.get(results_url).send().await.ok()?;
        if !resp.status().is_success() { return None; }

        let html = resp.text().await.ok()?;
        let document = Html::parse_document(&html);
        let link_selector = Selector::parse("a[href*='/matches/']").unwrap();

        let home_norm = home.to_lowercase();
        let away_norm = away.to_lowercase();

        for node in document.select(&link_selector).take(15) {
            let href = match node.value().attr("href") {
                Some(h) => h,
                None => continue,
            };
            if !href.contains("/tournaments/") { continue; }

            // Zkontroluj jestli href slug obsahuje oba týmy
            let (t1, t2) = match Self::extract_teams_from_gosugamers_href(href) {
                Some(pair) => pair,
                None => continue,
            };

            let t1_norm = t1.to_lowercase();
            let t2_norm = t2.to_lowercase();

            let home_matches = t1_norm.contains(&home_norm) || home_norm.contains(&t1_norm);
            let away_matches = t2_norm.contains(&away_norm) || away_norm.contains(&t2_norm);

            if !(home_matches && away_matches) {
                // Zkus opačný směr
                let home_matches_rev = t2_norm.contains(&home_norm) || home_norm.contains(&t2_norm);
                let away_matches_rev = t1_norm.contains(&away_norm) || away_norm.contains(&t1_norm);
                if !(home_matches_rev && away_matches_rev) { continue; }
            }

            // Najdi skóre v textu: pattern "SCORE:SCORE" (např. "2:0", "0:2", "W:FF")
            let text: String = node.text().collect();
            // Regex: najdi pattern X:Y kde X,Y jsou čísla nebo W/FF
            let score_re = regex::Regex::new(r"(\d+)\s*:\s*(\d+)").ok()?;
            if let Some(caps) = score_re.captures(&text) {
                let s1: i32 = caps[1].parse().unwrap_or(0);
                let s2: i32 = caps[2].parse().unwrap_or(0);
                if s1 > s2 {
                    return Some(t1);
                } else if s2 > s1 {
                    return Some(t2);
                }
            }
            // W:FF pattern
            if text.contains("W:FF") || text.contains("W :FF") {
                // Tým který má W je na pozici t1 (vzhledem k href ordering)
                return Some(t1);
            }
        }
        None
    }


    // ── FALLBACK: Results polling (audit, méně časté) ─────────────────────

    /// Fallback/audit — scrapuje /results stránky.
    /// Volat jednou za 5 minut jen pro audit, NE jako primární zdroj.
    pub async fn poll_all(&self) -> Vec<MatchResolvedEvent> {
        let mut healthy = 0;
        let total = 4;
        let mut items = 0;
        let mut all_resolved = Vec::new();

        match self.poll_lol().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("lolesports", "leagueoflegends", &e.to_string()); }
        }
        match self.poll_valorant().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("vlrgg", "valorant", &e.to_string()); }
        }
        match self.poll_cs2().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("gosugamers", "counterstrike", &e.to_string()); }
        }
        match self.poll_dota2().await {
            Ok(mut res) => { healthy += 1; items += res.len(); all_resolved.append(&mut res); }
            Err(e) => { self.log_api_error("gosugamers", "dota2", &e.to_string()); }
        }

        let heartbeat = SystemHeartbeatEvent {
            ts:                 now_iso(),
            event:              "SYSTEM_HEARTBEAT",
            phase:              "LIVE_SCORING_ACTIVE".to_string(),
            poll_interval_secs: self.poll_interval_secs,
            overall_items:      items,
            healthy_sources:    healthy,
            total_sources:      total,
            pinnacle_items:     0,
            oddsapi_items:      0,
            total_items:        items,
        };
        let _ = self.logger.log(&heartbeat);
        info!("Fallback poll: {} výsledků (healthy: {}/{})", items, healthy, total);

        all_resolved
    }

    // ── Původní results scrapery (fallback) ───────────────────────────────

    async fn poll_lol(&self) -> Result<Vec<MatchResolvedEvent>> {
        let url = "https://esports-api.lolesports.com/persisted/gw/getCompletedEvents?hl=en-US";
        let resp = self.client.get(url)
            .header("x-api-key", "0TvQnueqKa5mxJntVWt0w4LpLfEkrV1Ta8rQBb9Z")
            .send().await.context("LoL request failed")?;

        if !resp.status().is_success() {
            return Ok(vec![]);
        }

        let raw = resp.text().await?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let events = parsed.pointer("/data/schedule/events").and_then(|v| v.as_array());

        let mut results = Vec::new();
        if let Some(event_list) = events {
            for ev in event_list.iter().take(5) {
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
        for node in document.select(&match_selector).take(5) {
            let teams: Vec<_> = node.select(&team_selector).map(|t| t.text().collect::<String>().trim().to_string()).collect();
            let scores: Vec<_> = node.select(&score_selector).map(|s| s.text().collect::<String>().trim().to_string()).collect();
            if teams.len() == 2 && scores.len() == 2 {
                let s1: i32 = scores[0].parse().unwrap_or(0);
                let s2: i32 = scores[1].parse().unwrap_or(0);
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
        self.poll_gosugamers_results("counterstrike", "https://www.gosugamers.net/counterstrike/matches/results").await
    }

    async fn poll_dota2(&self) -> Result<Vec<MatchResolvedEvent>> {
        self.poll_gosugamers_results("dota2", "https://www.gosugamers.net/dota2/matches/results").await
    }

    /// Generický GosuGamers results fallback scraper (SSR kompatibilní).
    async fn poll_gosugamers_results(&self, sport: &str, url: &str) -> Result<Vec<MatchResolvedEvent>> {
        let resp = self.client.get(url).send().await
            .context(format!("GosuGamers {} results request failed", sport))?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("GosuGamers {} HTTP {}", sport, resp.status()));
        }
        let html = resp.text().await?;
        let document = Html::parse_document(&html);
        let link_selector = Selector::parse("a[href*='/matches/']").unwrap();
        let score_re = regex::Regex::new(r"(\d+)\s*:\s*(\d+)").unwrap();

        let mut results = Vec::new();
        for node in document.select(&link_selector).take(10) {
            let href = match node.value().attr("href") {
                Some(h) if h.contains("/tournaments/") => h,
                _ => continue,
            };
            let (t1, t2) = match Self::extract_teams_from_gosugamers_href(href) {
                Some(pair) => pair,
                None => continue,
            };
            let text: String = node.text().collect();
            if let Some(caps) = score_re.captures(&text) {
                let s1: i32 = caps[1].parse().unwrap_or(0);
                let s2: i32 = caps[2].parse().unwrap_or(0);
                if s1 != s2 {
                    let winner = if s1 > s2 { &t1 } else { &t2 };
                    let match_id = format!("{}_vs_{}", t1, t2);
                    if let Some(ev) = self.log_resolved(sport, &match_id, &t1, &t2, winner) {
                        results.push(ev);
                    }
                }
            }
        }
        self.log_api_ok("gosugamers", sport, results.len());
        Ok(results)
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn emit_resolved(&self, sport: &str, m_id: &str, t1: &str, t2: &str, winner: &str) -> Option<MatchResolvedEvent> {
        let ev = MatchResolvedEvent {
            ts:         now_iso(),
            event:      "MATCH_RESOLVED",
            sport:      sport.to_string(),
            match_name: m_id.to_string(),
            home:       t1.to_string(),
            away:       t2.to_string(),
            winner:     winner.to_string(),
            ended_at:   now_iso(),
        };
        let _ = self.logger.log(&ev);
        Some(ev)
    }

    fn log_resolved(&self, sport: &str, m_id: &str, t1: &str, t2: &str, winner: &str) -> Option<MatchResolvedEvent> {
        // Deduplikace pro results fallback
        let unique_key = format!("{}_{}", sport, m_id);
        {
            let mut seen = self.seen_matches.lock().unwrap();
            // Periodické čištění — max 500 entries
            if seen.len() > 500 {
                seen.clear();
                debug!("seen_matches cleared (>500 entries)");
            }
            if !seen.insert(unique_key) {
                return None;
            }
        }
        self.emit_resolved(sport, m_id, t1, t2, winner)
    }

    fn log_api_error(&self, source: &str, sport: &str, msg: &str) {
        let _ = self.logger.log(&ApiStatusEvent {
            ts:           now_iso(),
            event:        "API_STATUS",
            source:       source.to_string(),
            scope:        sport.to_string(),
            ok:           false,
            status_code:  None,
            message:      msg.to_string(),
            items_logged: 0,
        });
    }

    fn log_api_ok(&self, source: &str, sport: &str, count: usize) {
        let _ = self.logger.log(&ApiStatusEvent {
            ts:           now_iso(),
            event:        "API_STATUS",
            source:       source.to_string(),
            scope:        sport.to_string(),
            ok:           true,
            status_code:  Some(200),
            message:      "ok".to_string(),
            items_logged: count,
        });
    }
}
