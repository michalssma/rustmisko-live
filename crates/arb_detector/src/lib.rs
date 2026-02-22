/// RustMiskoLive — Arb Detector
/// Porovnává Pinnacle fair value vs Polymarket cenu
/// Fáze 1: OBSERVE only — loguje, neobchoduje

use logger::{EventLogger, ArbOpportunityEvent, now_iso};
use tracing::info;

pub struct ArbDetector {
    logger:       EventLogger,
    observe_only: bool,
    min_edge_pct: f64,
}

impl ArbDetector {
    pub fn new(log_dir: impl Into<std::path::PathBuf>, observe_only: bool) -> Self {
        Self {
            logger:       EventLogger::new(log_dir),
            observe_only,
            min_edge_pct: 0.03, // 3% minimum edge
        }
    }

    /// Porovnej Pinnacle implied prob vs Polymarket price
    /// pinnacle_prob: 0.0–1.0 (fair value bez vigu)
    /// polymarket_price: 0.0–1.0 (YES cena na CLOB)
    pub fn evaluate_pinnacle_vs_polymarket(
        &self,
        home:             &str,
        away:             &str,
        sport:            &str,
        pinnacle_prob:    f64,  // fair value
        polymarket_price: f64,  // aktuální tržní cena
        _condition_id:     &str,
    ) {
        // Edge = fair value - market price
        // Pokud Polymarket podhodnotí (cena < fair value) → edge na BUY
        let edge = pinnacle_prob - polymarket_price;

        if edge < self.min_edge_pct {
            return; // pod threshold → ticho
        }

        let action = if self.observe_only { "OBSERVE" } else { "BUY" };

        let ev = ArbOpportunityEvent {
            ts:               now_iso(),
            event:            "ARB_OPPORTUNITY",
            source:           "pinnacle_vs_polymarket".to_string(),
            home:             home.to_string(),
            away:             away.to_string(),
            sport:            sport.to_string(),
            edge_pct:         edge,
            pinnacle_prob,
            polymarket_price,
            action:           action.to_string(),
        };

        info!(
            edge = format!("{:.1}%", edge * 100.0),
            pinnacle_prob = format!("{:.2}", pinnacle_prob),
            polymarket   = format!("{:.2}", polymarket_price),
            "{} vs {} — edge found",
            home, away
        );

        let _ = self.logger.log(&ev);
    }
}
