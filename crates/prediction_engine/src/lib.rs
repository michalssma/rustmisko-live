//! Prediction Engine pro early detection konce zápasů
//! Heuristika místo AI/ML - jednoduché pravidlové systémy

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Stav zápasu pro predikci
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchState {
    pub sport: String,           // "cs2", "valorant", "lol", "dota2"
    pub score_team1: u8,
    pub score_team2: u8,
    pub map_number: u8,          // 1, 2, 3 (pro Bo3)
    pub total_maps: u8,          // 3 pro Bo3, 5 pro Bo5
    pub is_live: bool,
    pub last_update: DateTime<Utc>,
    // Volitelné: time series pro momentum tracking
    pub history: Vec<(DateTime<Utc>, u8, u8)>, // timestamp, score1, score2
}

/// Výsledek predikce s confidence score
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Prediction {
    Team1Win(f32),  // confidence 0.0-1.0
    Team2Win(f32),
    Uncertain,
}

impl Prediction {
    /// Vrátí confidence pokud je predikce určitá
    pub fn confidence(&self) -> Option<f32> {
        match self {
            Prediction::Team1Win(conf) | Prediction::Team2Win(conf) => Some(*conf),
            Prediction::Uncertain => None,
        }
    }
    
    /// Vrátí vítěze pokud je predikce určitá
    pub fn winner(&self) -> Option<&str> {
        match self {
            Prediction::Team1Win(_) => Some("team1"),
            Prediction::Team2Win(_) => Some("team2"),
            Prediction::Uncertain => None,
        }
    }
    
    /// Je predikce s vysokou jistotou (>0.9)?
    pub fn is_high_confidence(&self) -> bool {
        self.confidence().map_or(false, |c| c >= 0.9)
    }
}

/// Engine pro predikci výsledků zápasů
pub struct PredictionEngine {
    // Cache historických predikcí pro kalibraci
    predictions_cache: HashMap<String, Vec<(DateTime<Utc>, Prediction)>>,
}

impl Default for PredictionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PredictionEngine {
    pub fn new() -> Self {
        Self {
            predictions_cache: HashMap::new(),
        }
    }
    
    /// Predikuje výsledek na základě stavu zápasu
    pub fn predict(&self, state: &MatchState) -> Prediction {
        match state.sport.as_str() {
            "cs2" => self.predict_cs2(state),
            "valorant" => self.predict_valorant(state),
            "lol" => self.predict_lol(state),
            "dota2" => self.predict_dota2(state),
            _ => Prediction::Uncertain,
        }
    }
    
    /// CS2 predikce - vyhrává se na 13 roundů
    fn predict_cs2(&self, state: &MatchState) -> Prediction {
        let score_diff = state.score_team1 as i16 - state.score_team2 as i16;
        let total_rounds = state.score_team1 + state.score_team2;
        
        // Definitive výhra (13+ a rozdíl >=2)
        if state.score_team1 >= 13 && score_diff >= 2 {
            return Prediction::Team1Win(1.0);
        }
        if state.score_team2 >= 13 && score_diff <= -2 {
            return Prediction::Team2Win(1.0);
        }
        
        // Vysoká confidence (12:10 a podobně)
        if state.score_team1 == 12 && state.score_team2 <= 10 {
            return Prediction::Team1Win(0.95);
        }
        if state.score_team2 == 12 && state.score_team1 <= 10 {
            return Prediction::Team2Win(0.95);
        }
        
        // Střední confidence (velký náskok)
        if state.score_team1 >= 11 && score_diff >= 5 {
            return Prediction::Team1Win(0.85);
        }
        if state.score_team2 >= 11 && score_diff <= -5 {
            return Prediction::Team2Win(0.85);
        }
        
        // Momentum based (pokud máme historii)
        if !state.history.is_empty() {
            let last_score = state.history.last().unwrap();
            let rounds_diff = (state.score_team1 as i16 - last_score.1 as i16) - 
                             (state.score_team2 as i16 - last_score.2 as i16);
            
            // Tým získal 3+ roundy za sebou
            if rounds_diff >= 3 && total_rounds > 15 {
                return Prediction::Team1Win(0.75);
            }
            if rounds_diff <= -3 && total_rounds > 15 {
                return Prediction::Team2Win(0.75);
            }
        }
        
        Prediction::Uncertain
    }
    
    /// Valorant predikce - vyhrává se na 13 roundů, podobné CS2
    fn predict_valorant(&self, state: &MatchState) -> Prediction {
        let score_diff = state.score_team1 as i16 - state.score_team2 as i16;
        
        // Definitive výhra
        if state.score_team1 >= 13 && score_diff >= 2 {
            return Prediction::Team1Win(1.0);
        }
        if state.score_team2 >= 13 && score_diff <= -2 {
            return Prediction::Team2Win(1.0);
        }
        
        // Valorant má často 12:9, 12:8 situace
        if state.score_team1 == 12 && state.score_team2 <= 9 {
            return Prediction::Team1Win(0.98); // Větší confidence než CS2
        }
        if state.score_team2 == 12 && state.score_team1 <= 9 {
            return Prediction::Team2Win(0.98);
        }
        
        // Economic round advantage tracking
        if state.score_team1 >= 10 && score_diff >= 4 {
            return Prediction::Team1Win(0.88);
        }
        if state.score_team2 >= 10 && score_diff <= -4 {
            return Prediction::Team2Win(0.88);
        }
        
        Prediction::Uncertain
    }
    
    /// LoL predikce - komplexnější kvůli drakonům, baronech, atd.
    /// Pro zjednodušení: gold lead > 8k = vysoká šance
    fn predict_lol(&self, state: &MatchState) -> Prediction {
        // Pro LoL bychom potřebovali gold lead, tower count, dragon soul
        // Prozatím vracíme uncertain
        Prediction::Uncertain
    }
    
    /// Dota 2 predikce - based on networth lead
    fn predict_dota2(&self, state: &MatchState) -> Prediction {
        // Pro Dota 2 bychom potřebovali networth, barracks status
        Prediction::Uncertain
    }
    
    /// Predikce pro sérii (Bo3, Bo5)
    pub fn predict_series(&self, matches: &[MatchState]) -> Prediction {
        if matches.is_empty() {
            return Prediction::Uncertain;
        }
        
        let sport = &matches[0].sport;
        let mut team1_wins = 0;
        let mut team2_wins = 0;
        let total_maps_needed = matches[0].total_maps / 2 + 1; // Výherní threshold
        
        // Spočítej vyhrané mapy
        for match_state in matches {
            match self.predict(match_state) {
                Prediction::Team1Win(conf) if conf >= 0.7 => team1_wins += 1,
                Prediction::Team2Win(conf) if conf >= 0.7 => team2_wins += 1,
                _ => {}
            }
        }
        
        // Pokud už tým vyhrál potřebný počet map
        if team1_wins >= total_maps_needed {
            return Prediction::Team1Win(0.9);
        }
        if team2_wins >= total_maps_needed {
            return Prediction::Team2Win(0.9);
        }
        
        // Analýza aktuální mapy v sérii
        if let Some(current_match) = matches.last() {
            let map_prediction = self.predict(current_match);
            if let Some(conf) = map_prediction.confidence() {
                if conf >= 0.85 {
                    // Pokud tým vede 1:0 a je 11:3 na 2. mapě, série je prakticky vyhraná
                    if sport == "cs2" || sport == "valorant" {
                        let series_score_diff = team1_wins as i16 - team2_wins as i16;
                        let map_score_diff = current_match.score_team1 as i16 - current_match.score_team2 as i16;
                        
                        if series_score_diff == 1 && map_score_diff >= 8 && current_match.map_number > 1 {
                            // Tým vyhrál 1. mapu a vede o 8+ na 2. mapě
                            return if map_score_diff > 0 {
                                Prediction::Team1Win(0.92)
                            } else {
                                Prediction::Team2Win(0.92)
                            };
                        }
                    }
                }
            }
        }
        
        Prediction::Uncertain
    }
    
    /// Log predikci pro pozdější analýzu a kalibraci
    pub fn log_prediction(&mut self, match_id: &str, prediction: Prediction) {
        let entry = self.predictions_cache.entry(match_id.to_string())
            .or_insert_with(Vec::new);
        entry.push((Utc::now(), prediction));
        
        // Omez cache velikost
        if entry.len() > 100 {
            entry.remove(0);
        }
    }
    
    /// Získá úspěšnost predikcí pro kalibraci
    pub fn get_accuracy_stats(&self) -> (usize, usize) {
        // Toto by vyžadovalo ground truth data
        // Prozatím vracíme dummy stats
        (0, 0)
    }
}

/// Helper funkce pro vytvoření sniper triggeru
pub fn should_trigger_sniper(prediction: &Prediction) -> bool {
    prediction.is_high_confidence()
}

/// Vytvoří match state z HLTV data
pub fn match_state_from_hltv(
    sport: &str,
    team1: &str,
    team2: &str,
    score1: u8,
    score2: u8,
    map_number: u8,
    total_maps: u8,
    is_live: bool,
) -> MatchState {
    MatchState {
        sport: sport.to_string(),
        score_team1: score1,
        score_team2: score2,
        map_number,
        total_maps,
        is_live,
        last_update: Utc::now(),
        history: vec![(Utc::now(), score1, score2)],
    }
}