/// RustMiskoLive — Price Monitor
///
/// Dva zdroje dat (48h observe only):
///   A) Pinnacle Lines API — sharp line benchmark (fair value)
///   B) odds-api.io /arbitrage-bets — hotové arb příležitosti
///
/// Fáze 1 (48h): pouze loguje, nevydává signály k obchodování.

use anyhow::{Context, Result};
use logger::{EventLogger, PinnacleLineEvent, OddsApiArbEvent, now_iso};
use serde::Deserialize;
use tracing::{info, warn, debug};

// ── Pinnacle structs ─────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct PinnacleFixtures {
    pub league:    Vec<PinnacleLeague>,
}

#[derive(Deserialize, Debug)]
pub struct PinnacleLeague {
    pub id:     u64,
    pub name:   String,
    pub events: Vec<PinnacleEvent>,
}

#[derive(Deserialize, Debug)]
pub struct PinnacleEvent {
    pub id:        u64,
    pub home:      String,
    pub away:      String,
    pub starts:    String,
}

#[derive(Deserialize, Debug)]
pub struct PinnacleOdds {
    pub leagues: Vec<PinnacleLeagueOdds>,
}

#[derive(Deserialize, Debug)]
pub struct PinnacleLeagueOdds {
    pub id:     u64,
    pub events: Vec<PinnacleEventOdds>,
}

#[derive(Deserialize, Debug)]
pub struct PinnacleEventOdds {
    pub id:      u64,
    pub periods: Vec<PinnaclePeriod>,
}

#[derive(Deserialize, Debug)]
pub struct PinnaclePeriod {
    pub number:    u8,
    pub money_line: Option<PinnacleMoneyLine>,
}

#[derive(Deserialize, Debug)]
pub struct PinnacleMoneyLine {
    pub home:  Option<f64>,
    pub away:  Option<f64>,
    pub draw:  Option<f64>,
}

// ── odds-api.io arb structs ──────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct OddsApiArbResponse {
    pub arb_bets: Vec<OddsApiArbBet>,
}

#[derive(Deserialize, Debug)]
pub struct OddsApiArbBet {
    pub sport:        String,
    pub home_team:    String,
    pub away_team:    String,
    pub roi:          f64,
    pub outcome_a:    OddsApiOutcome,
    pub outcome_b:    OddsApiOutcome,
}

#[derive(Deserialize, Debug)]
pub struct OddsApiOutcome {
    pub outcome:    String,
    pub odds:       f64,
    pub bookmaker:  String,
}

// ── PriceMonitor ─────────────────────────────────────────────────────────────

pub struct PriceMonitor {
    client:       reqwest::Client,
    logger:       EventLogger,
    pinnacle_key: Option<String>,   // None = Pinnacle bez auth (free)
    oddsapi_key:  Option<String>,   // odds-api.io klíč
}

impl PriceMonitor {
    pub fn new(
        log_dir:      impl Into<std::path::PathBuf>,
        pinnacle_key: Option<String>,
        oddsapi_key:  Option<String>,
    ) -> Self {
        Self {
            client:       reqwest::Client::new(),
            logger:       EventLogger::new(log_dir),
            pinnacle_key,
            oddsapi_key,
        }
    }

    /// Hlavní poll — zavolej periodicky (každých 60s)
    pub async fn poll_all(&self) {
        // A) Pinnacle
        if let Err(e) = self.poll_pinnacle().await {
            warn!("Pinnacle poll failed: {}", e);
        }

        // B) odds-api.io arbitrage-bets
        if let Err(e) = self.poll_oddsapi_arb().await {
            warn!("odds-api.io arb poll failed: {}", e);
        }
    }

    // ── A) Pinnacle ──────────────────────────────────────────────────────────

    async fn poll_pinnacle(&self) -> Result<()> {
        // Sports IDs na Pinnacle: 29=soccer, 4=basketball, 3=baseball, 6=hockey, 33=tennis
        let sport_ids = vec![
            (29u32, "soccer"),
            (4,  "basketball"),
            (3,  "baseball"),
            (6,  "hockey"),
            (33, "tennis"),
        ];

        for (sport_id, sport_name) in sport_ids {
            match self.fetch_pinnacle_sport(sport_id, sport_name).await {
                Ok(count) => info!("Pinnacle {sport_name}: {count} lines logged"),
                Err(e)    => warn!("Pinnacle {sport_name} failed: {e}"),
            }
        }
        Ok(())
    }

    async fn fetch_pinnacle_sport(&self, sport_id: u32, sport_name: &str) -> Result<usize> {
        // Pinnacle public API (free, bez auth pro read-only odds)
        // Docs: https://pinnacleapi.github.io/linesapi
        let url = format!(
            "https://api.pinnacle.com/v1/odds?sportId={}&oddsFormat=Decimal&toOddsFormat=Decimal",
            sport_id
        );

        let mut req = self.client.get(&url)
            .header("Accept", "application/json");

        // Pinnacle může vyžadovat auth pro některé endpointy
        if let Some(ref key) = self.pinnacle_key {
            req = req.header("Authorization", format!("Basic {}", key));
        }

        let resp = req.send().await.context("Pinnacle request failed")?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("Pinnacle API {sport_name} status {status}: {}", &body[..body.len().min(200)]);
            return Ok(0);
        }

        let raw = resp.text().await.context("Pinnacle body read failed")?;
        debug!("Pinnacle {sport_name} raw (first 300): {}", &raw[..raw.len().min(300)]);

        // Pinnacle vrací: { "leagues": [ { "id": ..., "events": [ { "id": ..., "periods": [...] } ] } ] }
        let data: serde_json::Value = serde_json::from_str(&raw)
            .context("Pinnacle JSON parse failed")?;

        let mut count = 0usize;

        if let Some(leagues) = data["leagues"].as_array() {
            for league in leagues {
                let league_name = league["id"].to_string();
                if let Some(events) = league["events"].as_array() {
                    for ev in events {
                        let ev_id = ev["id"].as_u64().unwrap_or(0);
                        // Zkus získat moneyline z period 0
                        if let Some(periods) = ev["periods"].as_array() {
                            for period in periods {
                                if period["number"].as_u64() != Some(0) { continue; }
                                if let Some(ml) = period.get("moneyline") {
                                    let home_odds = ml["home"].as_f64();
                                    let away_odds = ml["away"].as_f64();
                                    let draw_odds = ml["draw"].as_f64();

                                    if let (Some(h), Some(a)) = (home_odds, away_odds) {
                                        // Převod decimal odds → implied prob (bez vigu)
                                        let raw_home = 1.0 / h;
                                        let raw_away = 1.0 / a;
                                        let raw_draw = draw_odds.map(|d| 1.0 / d).unwrap_or(0.0);
                                        let total = raw_home + raw_away + raw_draw;
                                        let prob_home = raw_home / total;
                                        let prob_away = raw_away / total;

                                        let line_ev = PinnacleLineEvent {
                                            ts:                 now_iso(),
                                            event:              "PINNACLE_LINE",
                                            sport:              sport_name.to_string(),
                                            home:               format!("event_{ev_id}"),
                                            away:               league_name.clone(),
                                            home_odds:          h,
                                            away_odds:          a,
                                            draw_odds,
                                            pinnacle_prob_home: prob_home,
                                            pinnacle_prob_away: prob_away,
                                        };

                                        if let Err(e) = self.logger.log(&line_ev) {
                                            warn!("Log write failed: {e}");
                                        }
                                        count += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(count)
    }

    // ── B) odds-api.io /arbitrage-bets ───────────────────────────────────────

    async fn poll_oddsapi_arb(&self) -> Result<()> {
        let key = match &self.oddsapi_key {
            Some(k) => k.clone(),
            None => {
                // odds-api.io free tier nevyžaduje klíč pro základní endpointy
                // Zkusíme bez klíče
                String::new()
            }
        };

        // odds-api.io — sports supported: americanfootball_nfl, basketball_nba, soccer_epl, ...
        let sports = vec![
            "basketball_nba",
            "soccer_epl",
            "soccer_uefa_champs_league",
            "americanfootball_nfl",
            "baseball_mlb",
            "icehockey_nhl",
            "tennis_atp_french_open",
        ];

        for sport in sports {
            if let Err(e) = self.fetch_arb_for_sport(sport, &key).await {
                warn!("odds-api.io arb for {sport}: {e}");
            }
        }
        Ok(())
    }

    async fn fetch_arb_for_sport(&self, sport: &str, api_key: &str) -> Result<()> {
        // odds-api.io free tier: 100 req/hour
        // Endpoint: GET https://odds-api.io/v1/arbitrage-bets?sport={sport}&apiKey={key}
        let base_url = if api_key.is_empty() {
            format!("https://api.the-odds-api.com/v4/sports/{}/odds/?regions=eu&markets=h2h&oddsFormat=decimal&apiKey=PLACEHOLDER", sport)
        } else {
            format!("https://odds-api.io/v1/arbitrage-bets?sport={}&apiKey={}", sport, api_key)
        };

        let resp = self.client
            .get(&base_url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("odds-api.io request failed")?;

        let status = resp.status();
        let raw = resp.text().await.context("odds-api.io body read failed")?;

        if !status.is_success() {
            debug!("odds-api.io {sport} status {status}: {}", &raw[..raw.len().min(200)]);
            return Ok(());
        }

        // Parsuj arb bets pokud jsou
        let data: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();

        // Formát závisí na tom zda používáme odds-api.io nebo the-odds-api fallback
        // Pro odds-api.io: { "arb_bets": [...] }
        if let Some(arbs) = data["arb_bets"].as_array() {
            for arb in arbs {
                let roi = arb["roi"].as_f64().unwrap_or(0.0);
                if roi < 0.01 { continue; } // skip <1%

                let ev = OddsApiArbEvent {
                    ts:              now_iso(),
                    event:           "ODDS_API_ARB",
                    sport:           sport.to_string(),
                    home:            arb["home_team"].as_str().unwrap_or("?").to_string(),
                    away:            arb["away_team"].as_str().unwrap_or("?").to_string(),
                    roi_pct:         roi * 100.0,
                    outcome_a:       arb["outcome_a"]["outcome"].as_str().unwrap_or("?").to_string(),
                    outcome_a_odds:  arb["outcome_a"]["odds"].as_f64().unwrap_or(0.0),
                    bookmaker_a:     arb["outcome_a"]["bookmaker"].as_str().unwrap_or("?").to_string(),
                    outcome_b:       arb["outcome_b"]["outcome"].as_str().unwrap_or("?").to_string(),
                    outcome_b_odds:  arb["outcome_b"]["odds"].as_f64().unwrap_or(0.0),
                    bookmaker_b:     arb["outcome_b"]["bookmaker"].as_str().unwrap_or("?").to_string(),
                };

                info!(
                    sport = %sport,
                    roi = format!("{:.2}%", roi * 100.0),
                    "{} vs {} — ARB found",
                    ev.home, ev.away
                );

                let _ = self.logger.log(&ev);
            }
        } else {
            debug!("odds-api.io {sport}: no arb_bets in response (format may differ)");
        }

        Ok(())
    }
}
