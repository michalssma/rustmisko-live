# SCORE MODELS — Rozšíření edge situací per sport

> **Verdikt**: Největší loophole systému objeven!
> Edge existuje, ale systém ho využívá jen v extrémních situacích.
> Tenhle dokument odemyká 3× více ziskových scénářů.

## 0. Diagnóza — Proč teď proděláváme

### Problém v číslech
- **Avg win**: $0.89, **avg loss**: $1.71 → 1 loss maže 2 výhry
- **ROI**: −26.7%, profit factor 0.505
- **WR**: 49% (potřeba 70%+ při avg odds 1.43)

### Root Cause: 3 kritické blokery

| # | Problém | Dopad |
|---|---------|-------|
| **B1** | `score_to_win_prob()` vrací `None` pro round scores → match_winner BLOCKED | CS2 round lead 9:4 na mapě 2 s mapami 1-0 = 92% true_p, ALE systém NEUMÍ vsadit match_winner! |
| **B2** | Anomaly guard binární (ano/ne) + flat $0.50 stake | Všechny anomaly bety mají stejný stake bez ohledu na skóre |
| **B3** | `AUTO_BET_STAKE_LOW_USD = 0.0` → tennis + basketball MRTVÉ | Score model pro tenis existuje ale stake = $0 |

### Současný tok pro CS2 round score (např. 9:4):
```
live_score = "9-4" → s1=9, s2=4, diff=5, max_score=9

KROK 1 (MAP WINNER):
  max_score > 3 && diff >= 3 → ANO
  → cs2_map_win_prob(5, 13) = 0.84 ✅
  → IF Azuro má map_winner odds → bet! ✅
  → IF nemá → has_map_winner_edge = false

KROK 2 (MATCH WINNER):
  → score_to_win_prob(9, 4) → max_score=9 > 3 → return None ❌
  → BET SKIPPED! ← TADY ZTRÁCÍME PENÍZE
```

**Řešení**: Nová funkce `cs2_round_to_match_prob()` — kombinuje round lead
s kontextem mapy pro match_winner predikci.

---

## 1. CS2 / Esports — Rozšířený Model

### 1a. MAP WINNER (beze změn — funguje ✅)

`cs2_map_win_prob(diff, total_rounds)` — granulární model, pokrývá 9:4, 9:3, 8:5 atd.
Tier systém ULTRA/HIGH/MEDIUM/LOW pro odds cap.

### 1b. MATCH WINNER z round scores (NOVÉ 🚀)

**Princip**: Round score predikuje výsledek AKTUÁLNÍ MAPY. Pro match_winner
musíme zkombinovat pravděpodobnost výhry mapy s kontextem aktuálního map skóre.

**Formule** (Bo3):
```
P(match_win) = P(win_this_map) × P(match | win_map, current_maps)
             + (1 - P(win_this_map)) × P(match | lose_map, current_maps)
```

**Přechodová tabulka Bo3**:
| Aktuální mapy | Výhra této mapy → | P(match po výhře) | Prohra → | P(match po prohře) |
|:-:|:-:|:-:|:-:|:-:|
| **0-0** | → 1-0 | 0.58 | → 0-1 | 0.42 |
| **1-0** | → 2-0 ✅ | **1.00** | → 1-1 | 0.50 |
| **0-1** | → 1-1 | 0.50 | → 0-2 ❌ | **0.00** |
| **1-1** | → 2-1 ✅ | **1.00** | → 1-2 ❌ | **0.00** |

### Kalkulace příkladů:

#### Příklad: Maps 1-0, Rounds 9:4 (diff=5, total=13)
```
map_prob = cs2_map_win_prob(5, 13) = 0.84
P(match) = 0.84 × 1.00 + 0.16 × 0.50 = 0.92 → StrongEdge!
```

#### Příklad: Maps 0-0, Rounds 9:4 (diff=5, total=13)
```
map_prob = 0.84
P(match) = 0.84 × 0.58 + 0.16 × 0.42 = 0.554 → Slabé, FalseFavorite
→ MAP WINNER bet je lepší (0.84 vs 0.554) → preferuj map_winner
```

#### Příklad: Maps 1-1, Rounds 9:3 (diff=6, total=12)
```
map_prob = cs2_map_win_prob(6, 12) = 0.76
P(match) = 0.76 × 1.00 + 0.24 × 0.00 = 0.76 → StrongEdge!
```

#### Příklad: Maps 0-1, Rounds 9:3 (diff=6, total=12)
```
map_prob = 0.76
P(match) = 0.76 × 0.50 + 0.24 × 0.00 = 0.38 → Pod 50%! → NoBet
```

### Výsledná true_p tabulka pro match_winner:

| Map Score | Round Diff | Total Rds | map_prob | **match true_p** | Režim |
|:-:|:-:|:-:|:-:|:-:|:-:|
| **1-0** | ≥7 | ≥19 | 0.97 | **0.985** | StrongEdge ULTRA |
| **1-0** | ≥5 | ≥13 | 0.84 | **0.920** | StrongEdge HIGH |
| **1-0** | ≥3 | ≥10 | 0.67 | **0.835** | StrongEdge |
| **1-0** | ≥3 | ≥8 | 0.62 | **0.810** | StrongEdge |
| **1-0** | 1-2 | any | 0.57 | **0.785** | FalseFavorite |
| **1-1** | ≥7 | ≥19 | 0.97 | **0.970** | StrongEdge ULTRA |
| **1-1** | ≥5 | ≥13 | 0.84 | **0.840** | StrongEdge HIGH |
| **1-1** | ≥3 | ≥10 | 0.67 | **0.670** | StrongEdge |
| **1-1** | 1-2 | any | 0.57 | **0.570** | FalseFavorite |
| **0-0** | any | any | — | **<0.58** | → Preferuj MAP WINNER |
| **0-1** | any | any | — | **<0.50** | → NoBet pro match_winner |

### Klíčový závěr CS2:
> **Maps 1-0 + round lead ≥3** = match_winner goldmine (true_p 0.81-0.99)
> **Maps 1-1 + round lead ≥3** = rozhodující moment (true_p 0.67-0.97)
> **Maps 0-0** = použi MAP WINNER bet, ne match_winner
> **Maps 0-1** = match_winner nemá edge (pod 50%)

### Implementace — nová funkce:
```rust
fn cs2_round_to_match_prob(
    map_lead: i32,    // maps won by leading team
    map_lose: i32,    // maps won by losing team
    round_lead: i32,  // rounds won by leading team (current map)
    round_lose: i32,  // rounds won by losing team
) -> Option<f64> {
    let diff = round_lead - round_lose;
    let total = round_lead + round_lose;
    if diff <= 0 { return None; }

    let map_prob = cs2_map_win_prob(diff, total);

    // Bo3 transition probabilities
    let (p_win_map, p_lose_map) = match (map_lead, map_lose) {
        (0, 0) => (0.58, 0.42),  // → 1-0 or 0-1
        (1, 0) => (1.00, 0.50),  // → 2-0 win or 1-1
        (0, 1) => (0.50, 0.00),  // → 1-1 or 0-2 loss
        (1, 1) => (1.00, 0.00),  // → 2-1 win or 1-2 loss
        _      => return None,   // match over
    };

    let match_prob = map_prob * p_win_map + (1.0 - map_prob) * p_lose_map;

    // Minimum useful threshold: 55%
    if match_prob < 0.55 { return None; }
    Some(match_prob)
}
```

### Anomaly guard rozšíření — esports:
```rust
// STARÁ verze (binární):
fn esports_anomaly_guard(s1, s2, detailed) -> bool { ... }

// NOVÁ verze (vrací true_p pro režim):
fn esports_score_confidence(s1: i32, s2: i32, detailed: Option<&str>) -> Option<f64> {
    let map_diff = (s1 - s2).abs();
    let (map_leader, map_loser) = if s1 > s2 { (s1, s2) } else { (s2, s1) };

    // Parse round score
    let round_info = detailed.and_then(|d| parse_esports_round_score(d));
    let map_info = detailed.and_then(|d| parse_dust2_map_score(d));

    if let Some((r1, r2)) = round_info {
        let rd = (r1 - r2).abs();
        let rt = r1 + r2;

        // Get map context (z Dust2 "M:X-Y" nebo z live_score)
        let (ml, mm) = map_info.unwrap_or((s1, s2));
        let map_lead = ml.max(mm);
        let map_lose = ml.min(mm);

        // Vrať match true_p pokud round lead produkuje edge
        return cs2_round_to_match_prob(map_lead, map_lose, r1.max(r2), r1.min(r2));
    }

    // Pouze map skóre (bez round info):
    match (map_leader, map_loser) {
        (1, 0) => Some(0.58),  // 1 mapa vedení
        (2, 0) => None,        // match over
        _ => None,             // 0-0 bez round info → no confidence
    }
}
```

---

## 2. Tennis — Game-Level Model 🎾

### Současný stav
- `tennis_score_to_win_prob(1, 0)` → 65% (jediný threshold)
- Parsování game score z detailed_score: **NEEXISTUJE**
- Anomaly guard: vyžaduje set_diff ≥ 1 (OK, ale bez game granularity)

### Kalibrace z dat (PLACED → WON/LOST):

| Sety | Games v aktuálním setu | W | L | WR | Verdikt |
|:--:|:--|:-:|:-:|:-:|:--|
| **1-0** | 0:0 (fresh set) | 4 | 0 | **100%** | ULTRA |
| **1-0** | Leading ≥2 games (3:0, 5:1) | 2 | 0 | **100%** | StrongEdge |
| **1-0** | Leading 1 game (1:0) | 1 | 0 | 100%* | StrongEdge |
| **1-0** | Behind 1 game (1:2) | 1 | 1 | 50% | FalseFav |
| **1-0** | Behind ≥2 games (2:5, 0:1+) | 0 | 2 | **0%** | NoBet |
| **1-0** | Tied (3:3, 4:4) | 0 | 3 | **0%** | NoBet |

*malý vzorek

### Klíčový insight:
> **1-0 sets + tied/losing games = opponent vrací do 1-1 → reset na coinflip**
> **1-0 sets + fresh set nebo leading games = silná pozice**

### Nový model — `tennis_enhanced_prob()`:
```rust
/// Tennis match_win probability with game-level granularity.
/// Required: set_diff ≥ 1.
/// game_diff = (set leader's games) - (opponent's games) in current set.
fn tennis_enhanced_prob(
    set_lead: i32,
    set_lose: i32,
    game_lead: i32,  // games won by the SET LEADER in current set
    game_lose: i32,  // games won by opponent in current set
) -> Option<f64> {
    if set_lead <= set_lose { return None; }

    let game_diff = game_lead - game_lose;
    let total_games = game_lead + game_lose;

    match (set_lead, set_lose) {
        (1, 0) => {
            // Fresh 2nd set start → strong
            if total_games == 0 {
                return Some(0.68);  // Data: 4W/0L
            }
            // Leading in games → about to go 2-0
            if game_diff >= 3 {
                return Some(0.82);  // dominating 2nd set
            }
            if game_diff >= 2 {
                return Some(0.75);  // comfortable lead
            }
            if game_diff >= 1 {
                return Some(0.70);  // slight lead
            }
            // Tied games → opponent fighting back → risky
            if game_diff == 0 {
                // Data: 0W/3L when tied (3:3, 4:4, 6:6)
                return None;  // NoBet — no edge
            }
            // Losing in games → opponent very likely to equalize
            if game_diff <= -2 {
                return None;  // NoBet — opponent coming back
            }
            // Slightly behind (-1): mix
            return Some(0.58);  // marginal, FalseFavorite
        }
        _ => None,  // 2-0 = won, 2-1 = won
    }
}
```

### Tennis detailed_score parser (NOVÝ):
```rust
/// Parse tennis game score from detailed_score.
/// Formats:
///   "1:02.set - 7:6(4), 2:1 (15:40*)" → set_score=(7,6), games=(2,1)
///   ".set - 6:4, 0:0 (00:00*)"         → set_score=(6,4), games=(0,0)
///   "0:12.set - 3:6, 1:0 (00:00*)"     → set_score=(3,6), games=(1,0)
///
/// Returns: (prev_set_score1, prev_set_score2, current_games1, current_games2)
fn parse_tennis_game_score(detailed: &str) -> Option<(i32, i32, i32, i32)> {
    // Extract all X:Y patterns (not point scores like 15:40)
    let scores: Vec<(i32, i32)> = detailed
        .split(|c: char| c == ',' || c == '(' || c == ')')
        .filter_map(|seg| {
            let trimmed = seg.trim();
            let parts: Vec<&str> = trimmed.split(':').collect();
            if parts.len() == 2 {
                let a = parts[0].trim().parse::<i32>().ok()?;
                let b = parts[1].trim().parse::<i32>().ok()?;
                // Game scores are 0-13 (tiebreaks go to 13)
                // Exclude point scores (0, 15, 30, 40)
                if a <= 13 && b <= 13 && !(a > 7 && b > 7) {
                    Some((a, b))
                } else { None }
            } else { None }
        })
        .collect();

    // Need at least 2 scores (previous set + current games)
    if scores.len() >= 2 {
        let (s1, s2) = scores[0]; // previous set score
        let (g1, g2) = scores[1]; // current set game score
        Some((s1, s2, g1, g2))
    } else {
        None
    }
}
```

---

## 3. Basketball — Enable Real Bets 🏀

### Současný stav
- `basketball_score_to_win_prob()` — EXISTUJE, granulární (point_diff × game_phase)
- **PROBLÉM**: `AUTO_BET_STAKE_LOW_USD = 0.0` → nula stake = žádné sázky!
- Historická data: NEMÁME live_score (většina "?")

### Model (beze změn — je OK):
Point diff model je správně kalibrovaný:
- Late game (140+ total), diff 8-12: 82%
- Late game, diff 13-17: 90%
- Late game, diff 18+: 95%

### Nutná změna:
```rust
// STARÉ:
const AUTO_BET_STAKE_LOW_USD: f64 = 0.0;

// NOVÉ: Přes regime systém
// Basketball/tennis dostávají stake z compute_stake() podle true_p
// Minimum: $0.50 (FalseFavorite), Maximum: $3.00 (StrongEdge)
// Odstraňujeme AUTO_BET_STAKE_LOW_USD — místo toho regime rozhoduje
```

### Live score problém:
Basketball live_score z Tipsportu PŘICHÁZÍ (viz recent entries "34-33"),
ale dřívější data nemají score. S novým systémem budeme sbírat
score data → postupně kalibrovat.

### Regime mapa pro basketball:
| Point Diff | Game Phase | true_p | Režim |
|:-:|:-:|:-:|:-:|
| 3-5 | Mid (30-80) | 0.57 | FalseFavorite ($0.50) |
| 6-9 | Mid | 0.63 | FalseFavorite ($0.50) |
| 10-14 | Mid | 0.72 | StrongEdge ($1.50-$3) |
| 15+ | Any | 0.80+ | StrongEdge ($2-$5) |
| 8-12 | Late (140+) | 0.82 | StrongEdge ($2-$5) |
| 13+ | Late | 0.90+ | StrongEdge MAX ($5) |

---

## 4. Football — Anomaly Unlock ⚽

### Současný stav
- `football_score_to_win_prob()` — EXISTUJE, granulární (goal_diff × total)
- Score-edge path: AKTIVNÍ (vyžaduje minute z detailed_score)
- Anomaly path: **DISABLED** (football → false)

### Problémy z dat:
- 0/2 football anomaly → LOST → proto disabled
- ALE: to byly game-start betsy BEZ skóre

### Řešení — Enable anomaly POUZE s goal_diff ≥ 2:
```rust
"football" => {
    if let Some(ref score) = anomaly.live_score {
        let parts: Vec<&str> = score.split('-').collect();
        if parts.len() == 2 {
            let (s1, s2) = (
                parts[0].trim().parse::<i32>().unwrap_or(0),
                parts[1].trim().parse::<i32>().unwrap_or(0),
            );
            let diff = (s1 - s2).abs();
            diff >= 2  // Pouze 2-0, 3-0, 3-1 atd. → silná pozice
        } else { false }
    } else { false }
}
```

### Regime mapa pro football anomaly:
| Goal Diff | true_p | Režim |
|:-:|:-:|:-:|
| 1 | 0.62-0.68 | FalseFavorite ($0.50) — pouze late game |
| 2 | 0.85-0.90 | StrongEdge ($2-$4) |
| 3+ | 0.96 | StrongEdge MAX ($5) |

---

## 5. Regime Systém — Master Tabulka

### Režimy (from approved plan):
| Režim | true_p | Stake Range | Popis |
|:--|:-:|:-:|:--|
| **StrongEdge** | ≥0.70 | $1.50 — $5.00 | Kelly/3 sizing |
| **FalseFavorite** | 0.55 — 0.70 | $0.50 | Test size |
| **Quarantine** | odds ≥ 2.0 | $0 | NoBet |
| **NoBet** | <0.55 or no data | $0 | Skip |

### Stake formula (Kelly/3):
```
stake = (bankroll × f) / 3
  kde f = (true_p × odds - 1) / (odds - 1)

Guardrails:
  - FLOOR: $0.50 (FalseFavorite), $1.50 (StrongEdge)
  - CAP:   $5.00
  - daily_loss_limit: $15
  - inflight_cap: 45% of bankroll
```

### Master score → regime tabulka:

#### CS2 Score-Edge Path (match_winner):
| Map Score | Round Diff | true_p | Regime | Stake |
|:-:|:-:|:-:|:-:|:-:|
| 1-0 | ≥5, late | 0.92+ | StrongEdge | $3-$5 (Kelly/3) |
| 1-0 | ≥3 | 0.81+ | StrongEdge | $2-$4 |
| 1-1 | ≥5, late | 0.84+ | StrongEdge | $2-$5 |
| 1-1 | ≥3 | 0.67 | StrongEdge (low) | $1.50-$2 |
| 0-0 | any | <0.58 | → MAP WINNER instead | via map_winner |
| 0-1 | any | <0.50 | NoBet | $0 |

#### CS2 Map-Winner Path (beze změn):
| Round Diff | Total | map_prob | Regime | Max Odds |
|:-:|:-:|:-:|:-:|:-:|
| ≥9 | ≥19 | 0.97 | ULTRA | 5.00 |
| ≥7 | ≥13 | 0.92 | HIGH | 3.00 |
| ≥5 | ≥10 | 0.84 | MEDIUM | 2.00 |
| ≥3 | ≥8 | 0.67 | LOW | 1.60 |

#### CS2 Anomaly Path:
| Situace | true_p | Regime | Stake |
|:--|:-:|:-:|:-:|
| Maps 1-0, rounds ≥3 diff | 0.81+ | StrongEdge | $1.50-$4 |
| Maps 1-1, rounds ≥5 diff | 0.84+ | StrongEdge | $2-$5 |
| Maps 1-0, no round info | 0.58 | FalseFavorite | $0.50 |
| Maps 0-0, rounds ≥5 diff | 0.55 | FalseFavorite | $0.50 |
| Maps 0-0, rounds <5 | — | NoBet | $0 |

#### Tennis Anomaly/Edge Path:
| Sets | Games | true_p | Regime | Stake |
|:-:|:--|:-:|:-:|:-:|
| 1-0 | 0:0 fresh | 0.68 | StrongEdge (low) | $1.50 |
| 1-0 | leading ≥2 | 0.75-0.82 | StrongEdge | $2-$4 |
| 1-0 | leading 1 | 0.70 | StrongEdge (low) | $1.50 |
| 1-0 | behind 1 | 0.58 | FalseFavorite | $0.50 |
| 1-0 | tied / behind ≥2 | <0.55 | NoBet | $0 |

#### Basketball (edge + anomaly):
| Point Diff | Phase | true_p | Regime | Stake |
|:-:|:-:|:-:|:-:|:-:|
| 10-14 | Mid+ | 0.72 | StrongEdge | $1.50-$3 |
| 15+ | Any | 0.80+ | StrongEdge | $2-$5 |
| 5-9 | Late | 0.66-0.70 | FalseFavorite | $0.50-$1.50 |
| <5 | Any | <0.60 | NoBet | $0 |

#### Football (anomaly):
| Goal Diff | true_p | Regime | Stake |
|:-:|:-:|:-:|:-:|
| ≥3 | 0.96 | StrongEdge MAX | $4-$5 |
| 2 | 0.85 | StrongEdge | $2-$4 |
| 1 (late) | 0.68 | FalseFavorite | $0.50 |
| 1 (early) | 0.62 | NoBet | $0 |

---

## 6. Konkrétní kódové změny (TODO)

### KROK A: Nové funkce (přidat do alert_bot.rs)
1. `cs2_round_to_match_prob()` — round + map context → match true_p
2. `parse_tennis_game_score()` — extract game score z detailed_score
3. `tennis_enhanced_prob()` — set + game → match true_p
4. `score_to_regime()` — master classifier → StrongEdge/FalseFav/NoBet
5. `compute_kelly_stake()` — true_p + odds → Kelly/3 stake

### KROK B: Úpravy stávajících funkcí
1. **`score_to_win_prob()`** (line 1600): Místo `return None` pro round scores →
   zavolat `cs2_round_to_match_prob()` s mapovým kontextem
2. **`esports_anomaly_guard()`** (line 1272): Vrátit `Option<f64>` místo `bool`
3. **Tennis anomaly guard** (line 5460): Přidat game-level check
4. **Football anomaly** (line 5448): Enable pro goal_diff ≥ 2
5. **`anomaly_stake_for_odds()`** (line 581): Nahradit regime-based sizing

### KROK C: Konstanty
```rust
// ODSTRANIT:
const AUTO_BET_STAKE_LOW_USD: f64 = 0.0;  // → regime rozhoduje
const AUTO_BET_ODDS_ANOMALY_STAKE_BASE_USD: f64 = 0.50;  // → Kelly/3
const ANOMALY_DAILY_LIMIT_MULT: f64 = 0.30;  // → společný budget

// PŘIDAT:
const STRONG_EDGE_STAKE_MIN: f64 = 1.50;
const STRONG_EDGE_STAKE_MAX: f64 = 5.00;
const FALSE_FAVORITE_STAKE: f64 = 0.50;
const DAILY_LOSS_LIMIT_USD: f64 = 15.0;  // (snížit z 30)
const INFLIGHT_CAP_PCT: f64 = 0.45;
```

---

## 7. Očekávaný dopad

### Rozšíření situací:

| Sport | Situace (staré) | Situace (nové) | Nárůst |
|:--|:--|:--|:-:|
| CS2 map_winner | round_diff ≥ 3 | beze změn | — |
| CS2 match_winner | POUZE maps 1-0 (58%) | maps 1-0 + round lead, maps 1-1 + round lead | **3×** |
| CS2 anomaly | binární guard, flat $0.50 | graduated true_p → $0.50-$5 | **sizing 10×** |
| Tennis | sets 1-0 → 65% | sets 1-0 + game context (0.58-0.82) | **5 nových tier** |
| Basketball | $0 (mrtvé) | $0.50-$5 (live) | **∞** |
| Football anomaly | disabled | goal_diff ≥ 2 (true_p 0.85+) | **nový zdroj** |

### Projektovaný EV po změnách:
```
Conservative scenario (same WR, better sizing):
  StrongEdge bets (true_p ≥ 0.70):
    avg_stake = $2.50, avg_odds = 1.50, WR = 65%
    EV/bet = $2.50 × (0.65 × 1.50 - 1) = $2.50 × (-0.025) = -$0.06
    → Near break-even!

  StrongEdge bets (true_p ≥ 0.80, CS2 1-0 + round lead):
    avg_stake = $3.50, avg_odds = 1.40, WR = 75%
    EV/bet = $3.50 × (0.75 × 1.40 - 1) = $3.50 × 0.05 = +$0.175
    → PROFITABLE!

  FalseFavorite bets: $0.50 test → data collection, minimal loss
```

---

## ❓ Na schválení od Miši — SCHVÁLENO 2026-03-04

### Fázový rollout (schválený):
| Fáze | Co | Status | FF |
|:-:|:--|:-:|:--|
| **1 (LIVE)** | CS2 match_winner unlock + Football anomaly goal_diff≥2 + Regime Kelly/3 ($1.50-$5) | ✅ Implementováno | `FF_CS2_MATCH_FROM_ROUNDS=true`, `FF_FOOTBALL_ANOMALY_GOALDIFF2=true`, `FF_REGIME_STAKE=true` |
| **2** | Tennis game-level model | ⏳ Po 50+ CS2 betech | `FF_TENNIS_GAME_MODEL=false` |
| **3** | Football anomaly monitoring refinement | ⏳ Po 50+ betech z fáze 1 | — |
| **4** | Basketball live bets | ⏳ Nikdy bez kalibrace | `FF_BASKETBALL_LIVE=false` |

### Rozhodnutí:
1. CS2 match_winner z round scores: **ANO** ✅
2. Tennis game parser: **NE** (Phase 2, FF OFF)
3. Basketball enable: **NE** (Phase 4, bez kalibrace)
4. Football anomaly goal_diff≥2: **ANO** ✅
5. Stake ranges StrongEdge $1.50-$5: **ANO** ✅
6. Daily loss limit: **ZŮSTÁVÁ $30** (risk-of-ruin výpočet bude separátně)
7. Priorita: Fázový rollout, CS2 first

### Implementované kódové změny (Phase 1):
- `cs2_round_to_match_prob()` — Bo3 přechodová tabulka × map_win_prob
- `classify_regime()` — StrongEdge (≥0.70) / FalseFavorite (0.55-0.70) / NoBet
- `compute_regime_stake()` — Kelly/3 s guardrails $1.50-$5.00 / $0.50
- Score-edge path at line ~2170: CS2 round scores → match_prob místo None
- Anomaly path: Regime-based stake místo flat $0.50
- Football anomaly: Enabled s goal_diff ≥ 2 guardem
- Feature flags: `FF_CS2_MATCH_FROM_ROUNDS`, `FF_FOOTBALL_ANOMALY_GOALDIFF2`, `FF_REGIME_STAKE`, `FF_TENNIS_GAME_MODEL`, `FF_BASKETBALL_LIVE`
