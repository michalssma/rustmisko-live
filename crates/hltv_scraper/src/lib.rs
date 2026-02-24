//! HLTV.org rapid scraper pro CS2 live scoring
//! Latence target: <15s (vs. 60-120s u GosuGamers)
//!
//! Struktura HLTV match page:
//! https://www.hltv.org/matches/<match_id>/<team1>-vs-<team2>
//!
//! Live skóre elementy:
//! <div class="team1-gradient"> <div class="score">13</div> </div>
//! <div class="team2-gradient"> <div class="score">8</div> </div>

use anyhow::{Context, Result};
use headless_chrome::{Browser, LaunchOptions};
use reqwest::StatusCode;
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Live match stav z HLTV
#[derive(Debug, Clone)]
pub struct HltvLiveMatch {
    pub match_id: u64,
    pub team1: String,
    pub team2: String,
    pub score1: u8,
    pub score2: u8,
    pub is_live: bool,
    pub map_name: Option<String>,
    pub series_format: String, // "bo1", "bo3", "bo5"
    pub last_update: Instant,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct HltvEndpointProbe {
    pub url: String,
    pub html_len: usize,
    pub match_id_count: usize,
    pub looks_like_challenge_page: bool,
}

/// Stav predikce pro sniper mode
#[derive(Debug, PartialEq)]
pub enum MatchPrediction {
    Team1Win(f32), // confidence 0.0-1.0
    Team2Win(f32),
    Uncertain,
}

impl HltvLiveMatch {
    /// Predikuje výsledek na základě aktuálního skóre
    pub fn predict(&self) -> MatchPrediction {
        // CS2: vyhrává se na 13 vítězných roundů
        if self.score1 >= 13 && self.score1 - self.score2 >= 2 {
            MatchPrediction::Team1Win(1.0)
        } else if self.score2 >= 13 && self.score2 - self.score1 >= 2 {
            MatchPrediction::Team2Win(1.0)
        } else if self.score1 == 12 && self.score2 <= 10 {
            // 12:10 → velmi vysoká šance
            MatchPrediction::Team1Win(0.95)
        } else if self.score2 == 12 && self.score1 <= 10 {
            MatchPrediction::Team2Win(0.95)
        } else if self.score1 >= 11 && self.score1 - self.score2 >= 5 {
            // Např. 11:6 → ~85% šance
            MatchPrediction::Team1Win(0.85)
        } else if self.score2 >= 11 && self.score2 - self.score1 >= 5 {
            MatchPrediction::Team2Win(0.85)
        } else {
            MatchPrediction::Uncertain
        }
    }
    
    /// Vrací true pokud je zápas prakticky ukončen
    pub fn is_conclusive(&self) -> bool {
        matches!(self.predict(), MatchPrediction::Team1Win(_) | MatchPrediction::Team2Win(_))
    }
    
    /// Vítěz podle predikce
    pub fn predicted_winner(&self) -> Option<(&str, f32)> {
        match self.predict() {
            MatchPrediction::Team1Win(conf) => Some((&self.team1, conf)),
            MatchPrediction::Team2Win(conf) => Some((&self.team2, conf)),
            MatchPrediction::Uncertain => None,
        }
    }
}

/// HLTV scraper s cache a rate limiting
pub struct HltvScraper {
    client: reqwest::Client,
    /// Cache živých zápasů: match_id → HltvLiveMatch
    live_cache: Arc<Mutex<HashMap<u64, HltvLiveMatch>>>,
    /// User-agent rotace
    user_agents: Vec<String>,
    current_ua_index: usize,
    last_request: Instant,
    min_request_interval: Duration,
    last_browser_fetch: Instant,
    min_browser_interval: Duration,
}

impl HltvScraper {
    pub fn new() -> Self {
        let user_agents = vec![
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string(),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36".to_string(),
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/118.0.0.0 Safari/537.36".to_string(),
        ];
        
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8".parse().unwrap());
        headers.insert("Accept-Language", "en-US,en;q=0.5".parse().unwrap());
        headers.insert("Connection", "keep-alive".parse().unwrap());
        headers.insert("Upgrade-Insecure-Requests", "1".parse().unwrap());
        
        Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(10))
                .gzip(true)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            live_cache: Arc::new(Mutex::new(HashMap::new())),
            user_agents,
            current_ua_index: 0,
            last_request: Instant::now() - Duration::from_secs(60),
            min_request_interval: Duration::from_secs(3), // Respektuj robots.txt
            last_browser_fetch: Instant::now() - Duration::from_secs(300),
            min_browser_interval: Duration::from_secs(6),
        }
    }

    fn parse_match_ids_from_html(html: &str) -> Vec<u64> {
        let mut ids = HashSet::new();
        let mut search_from = 0usize;

        while let Some(found) = html[search_from..].find("/matches/") {
            let start = search_from + found + "/matches/".len();
            let bytes = html.as_bytes();
            let mut end = start;

            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }

            if end > start {
                if let Ok(id) = html[start..end].parse::<u64>() {
                    ids.insert(id);
                }
            }

            search_from = end.min(bytes.len());
            if search_from >= bytes.len() {
                break;
            }
        }

        let mut out: Vec<u64> = ids.into_iter().collect();
        out.sort_unstable();
        out
    }

    async fn fetch_html_http(&mut self, url: &str) -> Result<String> {
        self.wait_for_rate_limit().await;

        let resp = self.client.get(url)
            .header("User-Agent", self.current_user_agent())
            .send()
            .await
            .context(format!("HLTV request failed for {}", url))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("HLTV HTTP {}", status));
        }

        Ok(resp.text().await?)
    }

    async fn fetch_html_browser(&mut self, url: &str) -> Result<String> {
        let elapsed = self.last_browser_fetch.elapsed();
        if elapsed < self.min_browser_interval {
            return Err(anyhow::anyhow!(
                "Browser fallback cooldown active ({}s remaining)",
                (self.min_browser_interval - elapsed).as_secs()
            ));
        }

        self.last_browser_fetch = Instant::now();
        let url = url.to_string();

        let html = task::spawn_blocking(move || -> Result<String> {
            let options = LaunchOptions::default_builder()
                .headless(true)
                .sandbox(false)
                .build()
                .context("Failed to build Chrome launch options")?;

            let browser = Browser::new(options).context("Failed to launch Chrome")?;
            let tab = browser.new_tab().context("Failed to create browser tab")?;

            tab.navigate_to(&url).context("Chrome navigate failed")?;
            tab.wait_for_element("body").context("Chrome wait_for_element(body) failed")?;
            std::thread::sleep(Duration::from_secs(2));

            tab.get_content().context("Failed to read HTML from browser tab")
        }).await??;

        Ok(html)
    }

    async fn fetch_html_with_fallback(&mut self, url: &str) -> Result<String> {
        match self.fetch_html_http(url).await {
            Ok(html) => {
                self.rotate_user_agent();
                Ok(html)
            }
            Err(err) => {
                let is_403 = err.to_string().contains(&StatusCode::FORBIDDEN.to_string());
                if !is_403 {
                    return Err(err);
                }

                warn!("HLTV HTTP 403 on {}, trying browser fallback", url);
                let html = self.fetch_html_browser(url).await?;
                self.rotate_user_agent();
                Ok(html)
            }
        }
    }

    pub async fn fetch_recent_match_ids(&mut self, limit: usize) -> Result<Vec<u64>> {
        let html = self.fetch_html_with_fallback("https://www.hltv.org/results").await?;
        let mut ids = Self::parse_match_ids_from_html(&html);
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.truncate(limit);
        Ok(ids)
    }

    pub async fn probe_endpoint(&mut self, url: &str) -> Result<HltvEndpointProbe> {
        let html = self.fetch_html_with_fallback(url).await?;
        let match_ids = Self::parse_match_ids_from_html(&html);

        let lower = html.to_lowercase();
        let looks_like_challenge_page =
            lower.contains("just a moment") ||
            lower.contains("cf-challenge") ||
            lower.contains("captcha") ||
            lower.contains("cloudflare");

        Ok(HltvEndpointProbe {
            url: url.to_string(),
            html_len: html.len(),
            match_id_count: match_ids.len(),
            looks_like_challenge_page,
        })
    }
    
    /// Rotace user-agent pro prevenci blokování
    fn rotate_user_agent(&mut self) {
        self.current_ua_index = (self.current_ua_index + 1) % self.user_agents.len();
    }
    
    /// Získá aktuální user-agent
    fn current_user_agent(&self) -> &str {
        &self.user_agents[self.current_ua_index]
    }
    
    /// Rate limiting helper
    async fn wait_for_rate_limit(&mut self) {
        let elapsed = self.last_request.elapsed();
        if elapsed < self.min_request_interval {
            let wait_time = self.min_request_interval - elapsed;
            sleep(wait_time).await;
        }
        self.last_request = Instant::now();
    }
    
    /// Získa seznam aktuálních live zápasů z HLTV homepage
    pub async fn fetch_live_matches(&mut self) -> Result<Vec<u64>> {
        let html = self.fetch_html_with_fallback("https://www.hltv.org/live").await?;
        let match_ids = Self::parse_match_ids_from_html(&html);

        debug!("HLTV live matches found: {:?}", match_ids);
        Ok(match_ids)
    }
    
    /// Získá detailní informace o konkrétním zápasu
    pub async fn fetch_match_details(&mut self, match_id: u64) -> Result<Option<HltvLiveMatch>> {
        // URL pattern: /matches/2365125/natus-vincere-vs-faze
        // Pro detail potřebujeme nejdřív zjistit slug z homepage
        // Prozatím použijeme generickou URL
        let url = format!("https://www.hltv.org/matches/{}", match_id);

        let html = match self.fetch_html_with_fallback(&url).await {
            Ok(html) => html,
            Err(err) => {
                if err.to_string().contains("404") {
                    debug!("HLTV match {} not found", match_id);
                    return Ok(None);
                }
                warn!("HLTV match {} fetch failed: {}", match_id, err);
                return Ok(None);
            }
        };

        let document = Html::parse_document(&html);
        
        // Extrahuj jména týmů z titulku nebo speciálních elementů
        let team1_selector = Selector::parse(".team1-gradient .teamName").unwrap_or_else(|_| {
            Selector::parse(".team1 .teamName").unwrap()
        });
        let team2_selector = Selector::parse(".team2-gradient .teamName").unwrap_or_else(|_| {
            Selector::parse(".team2 .teamName").unwrap()
        });
        
        let score1_selector = Selector::parse(".team1-gradient .score").unwrap_or_else(|_| {
            Selector::parse(".team1 .score").unwrap()
        });
        let score2_selector = Selector::parse(".team2-gradient .score").unwrap_or_else(|_| {
            Selector::parse(".team2 .score").unwrap()
        });
        
        let team1 = document.select(&team1_selector)
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .unwrap_or_else(|| "Team1".to_string());
            
        let team2 = document.select(&team2_selector)
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .unwrap_or_else(|| "Team2".to_string());
        
        let score1 = document.select(&score1_selector)
            .next()
            .and_then(|e| e.text().collect::<String>().trim().parse::<u8>().ok())
            .unwrap_or(0);
            
        let score2 = document.select(&score2_selector)
            .next()
            .and_then(|e| e.text().collect::<String>().trim().parse::<u8>().ok())
            .unwrap_or(0);
        
        // Detekuj zda je zápas live
        let live_selector = Selector::parse(".countdown").unwrap_or_else(|_| Selector::parse("div").unwrap());
        let is_live = document.select(&live_selector)
            .any(|e| e.text().collect::<String>().to_lowercase().contains("live"));
        
        let match_data = HltvLiveMatch {
            match_id,
            team1,
            team2,
            score1,
            score2,
            is_live,
            map_name: None, // TODO: extrahovat z .map-name elementu
            series_format: "bo1".to_string(), // TODO: detekovat z kontextu
            last_update: Instant::now(),
            url,
        };
        
        self.rotate_user_agent();
        Ok(Some(match_data))
    }
    
    /// Main loop pro sledování live zápasů
    pub async fn monitor_live_matches(&mut self, callback: impl Fn(HltvLiveMatch) + Send + 'static) -> Result<()> {
        let mut previous_live_ids = Vec::new();
        
        loop {
            match self.fetch_live_matches().await {
                Ok(current_live_ids) => {
                    // Aktualizuj cache
                    for &match_id in &current_live_ids {
                        if let Ok(Some(match_data)) = self.fetch_match_details(match_id).await {
                            let mut cache = self.live_cache.lock().unwrap();
                            cache.insert(match_id, match_data.clone());
                            
                            // Pokud je to nový live zápas, informuj
                            if !previous_live_ids.contains(&match_id) {
                                info!("🔴 HLTV LIVE: {} vs {} ({}-{})", 
                                    match_data.team1, match_data.team2, 
                                    match_data.score1, match_data.score2);
                                
                                // Check prediction
                                if let Some((winner, confidence)) = match_data.predicted_winner() {
                                    if confidence >= 0.9 {
                                        info!("🔥 PREDICTION: {} wins with {:.0}% confidence", winner, confidence * 100.0);
                                    }
                                }
                                
                                callback(match_data);
                            }
                        }
                    }
                    
                    // Detekuj ukončené zápasy (zmizely z live listu)
                    for &old_id in &previous_live_ids {
                        if !current_live_ids.contains(&old_id) {
                            if let Some(finished_match) = self.live_cache.lock().unwrap().remove(&old_id) {
                                info!("✅ HLTV FINISHED: {} vs {} ({}-{})", 
                                    finished_match.team1, finished_match.team2,
                                    finished_match.score1, finished_match.score2);
                                // TODO: Emitovat událost pro arb detektor
                            }
                        }
                    }
                    
                    previous_live_ids = current_live_ids;
                }
                Err(e) => {
                    warn!("HLTV live fetch failed: {}", e);
                }
            }
            
            // Poll interval: 10 sekund v normálním režimu, 2 sekundy v sniper mode
            let has_conclusive = {
                let cache = self.live_cache.lock().unwrap();
                cache.values().any(|m| m.is_conclusive())
            };
            
            let sleep_time = if has_conclusive {
                Duration::from_secs(2) // Sniper mode
            } else {
                Duration::from_secs(10) // Normal mode
            };
            
            sleep(sleep_time).await;
        }
    }
}
