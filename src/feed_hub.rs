//! feed-hub — WS ingest pro headful browser/Android feeds + Azuro GraphQL poller
//!
//! Cíl: přijímat realtime JSON z Lenovo (Tampermonkey) / Zebra (Android) a v Rustu
//! udržovat „co je LIVE" + „kde jsou LIVE odds", s gatingem a audit logy.
//! Navíc: periodicky polluje Azuro Protocol (The Graph) pro CS2 on-chain odds.
//!
//! Spuštění:
//!   $env:FEED_HUB_BIND="0.0.0.0:8080"; cargo run --bin feed-hub
//!
//! Tampermonkey (příklad):
//!   const ws = new WebSocket('ws://10.107.109.85:8080/feed');
//!   ws.send(JSON.stringify({v:1, type:'live_match', source:'hltv', ...}))

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use logger::EventLogger;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use unicode_normalization::UnicodeNormalization;

mod feed_db;
mod azuro_poller;
use feed_db::{
    spawn_db_writer,
    DbConfig,
    DbFusionRow,
    DbHeartbeatRow,
    DbIngestRow,
    DbLiveRow,
    DbMsg,
    DbOddsRow,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedMessageType {
    LiveMatch,
    Odds,
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEnvelope {
    pub v: u32,
    #[serde(rename = "type")]
    pub msg_type: FeedMessageType,
    pub source: String,
    pub ts: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveMatchPayload {
    pub sport: String,
    pub team1: String,
    pub team2: String,
    pub score1: Option<i64>,
    pub score2: Option<i64>,
    pub detailed_score: Option<String>,
    pub status: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OddsPayload {
    pub sport: String,
    pub bookmaker: String,
    pub market: String,
    pub team1: String,
    pub team2: String,

    pub odds_team1: f64,
    pub odds_team2: f64,

    /// Odhadovaná likvidita v USD (nebo ekvivalent) — pro gating
    pub liquidity_usd: Option<f64>,
    /// Spread v procentech (např. 1.2 znamená 1.2%) — pro gating
    pub spread_pct: Option<f64>,

    pub url: Option<String>,

    // === Azuro execution data (pro BUY + cashout) ===
    /// Azuro game ID (subgraph)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    /// Azuro condition ID (pro bet placement)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition_id: Option<String>,
    /// Azuro outcome ID pro team1 win
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome1_id: Option<String>,
    /// Azuro outcome ID pro team2 win
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome2_id: Option<String>,
    /// Chain name (polygon, gnosis, base, chiliz)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
}

#[derive(Debug, Clone)]
struct LiveMatchState {
    source: String,
    seen_at: DateTime<Utc>,
    payload: LiveMatchPayload,
}

#[derive(Debug, Clone)]
struct OddsState {
    source: String,
    seen_at: DateTime<Utc>,
    payload: OddsPayload,
}

#[derive(Debug, Clone, Serialize)]
struct FeedIngestEvent {
    ts: String,
    event: &'static str,
    source: String,
    msg_type: String,
    ok: bool,
    note: String,
}

#[derive(Debug, Clone, Serialize)]
struct LiveFusionReadyEvent {
    ts: String,
    event: &'static str, // "LIVE_FUSION_READY"
    sport: String,
    match_key: String,
    live_source: String,
    odds_source: String,
    bookmaker: String,
    market: String,
    liquidity_usd: Option<f64>,
    spread_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct FeedHeartbeatEvent {
    ts: String,
    event: &'static str, // "FEED_HUB_HEARTBEAT"
    connections: usize,
    live_items: usize,
    odds_items: usize,
    fused_ready: usize,
}

/// Key for multi-bookmaker odds: match_key + bookmaker
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct OddsKey {
    match_key: String,
    bookmaker: String,
}

#[derive(Clone)]
struct FeedHubState {
    live: Arc<RwLock<HashMap<String, LiveMatchState>>>,
    odds: Arc<RwLock<HashMap<OddsKey, OddsState>>>,
    connections: Arc<RwLock<usize>>,
    alias_cache: AliasCache,
}

impl FeedHubState {
    fn new() -> Self {
        Self {
            live: Arc::new(RwLock::new(HashMap::new())),
            odds: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(0)),
            alias_cache: new_alias_cache(),
        }
    }
}

/// For tennis: extract SURNAME portion for cross-platform matching.
/// FlashScore format: "Blanchet U." → "blanchet"
/// Azuro format: "Ugo Blanchet" → "blanchet"
/// Handles particles: "De Stefano S." → "destefano", "Samira De Stefano" → "destefano"

// ====================================================================
// Feature flags (env kill-switches) + global counters
// ====================================================================
/// NORM_TRACE: detailed raw→normalized logging (default OFF, I/O heavy)
static FF_NORM_TRACE: AtomicBool = AtomicBool::new(false);
/// Sampling counter for NORM_TRACE: log every Nth ingest (default 20 = 5%)
const NORM_TRACE_SAMPLE_EVERY: u64 = 20;
static NORM_TRACE_COUNTER: AtomicU64 = AtomicU64::new(0);
/// EXTENDED_SUFFIX_STRIP: extra suffix list (default ON)
static FF_EXTENDED_SUFFIX_STRIP: AtomicBool = AtomicBool::new(true);
/// TOKEN_SUBSET_PAIR_ALIAS: write-time fuzzy token-subset matching (default ON)
static FF_TOKEN_SUBSET_PAIR_ALIAS: AtomicBool = AtomicBool::new(true);
/// COUNTRY_TRANSLATE sampling counter (1 in 20 = 5%)
static COUNTRY_TRANSLATE_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Max alias cache entries (LRU eviction beyond this)
const ALIAS_CACHE_MAX: usize = 1000;
/// Alias cache TTL in seconds (12h)
const ALIAS_CACHE_TTL_SECS: i64 = 43200;

/// Load feature flags from environment variables (call once at startup)
fn load_feature_flags() {
    if let Ok(v) = std::env::var("FF_NORM_TRACE") {
        FF_NORM_TRACE.store(v == "true" || v == "1", Ordering::Relaxed);
    }
    if let Ok(v) = std::env::var("FF_EXTENDED_SUFFIX_STRIP") {
        if v == "false" || v == "0" {
            FF_EXTENDED_SUFFIX_STRIP.store(false, Ordering::Relaxed);
        }
    }
    if let Ok(v) = std::env::var("FF_TOKEN_SUBSET_PAIR_ALIAS") {
        if v == "false" || v == "0" {
            FF_TOKEN_SUBSET_PAIR_ALIAS.store(false, Ordering::Relaxed);
        }
    }
    info!(
        "Feature flags: NORM_TRACE={}, EXTENDED_SUFFIX_STRIP={}, TOKEN_SUBSET_PAIR_ALIAS={}",
        FF_NORM_TRACE.load(Ordering::Relaxed),
        FF_EXTENDED_SUFFIX_STRIP.load(Ordering::Relaxed),
        FF_TOKEN_SUBSET_PAIR_ALIAS.load(Ordering::Relaxed),
    );
}

/// Normalize sport label: unify variant names to canonical form
/// hockey → ice-hockey (FlashScore, Tipsport, Chance send "hockey"; Azuro sends "ice-hockey")
/// lol → league-of-legends, csgo → cs2, esport → esports
fn normalize_sport(sport: &str) -> String {
    let s = sport.to_lowercase();
    match s.as_str() {
        "hockey" | "hokej" | "ledni-hokej" | "lední hokej" => "ice-hockey".to_string(),
        "lol" => "league-of-legends".to_string(),
        "csgo" | "cs:go" | "counter-strike" => "cs2".to_string(),
        "esport" | "e-sporty" | "e-sport" => "esports".to_string(),
        "fotbal" | "soccer" => "football".to_string(),
        "basketbal" => "basketball".to_string(),
        "volejbal" => "volleyball".to_string(),
        "házenou" | "handbal" => "handball".to_string(),
        _ => s,
    }
}

/// Translate Czech country/team names to English equivalents.
/// Called BEFORE normalize_name, operates on the NFKD-stripped lowercase string.
/// Only maps well-known country names that actually appear in Czech scrapers.
fn translate_country_name(name: &str) -> String {
    // All inputs should already be lowercase + diacritics stripped
    let translations: &[(&str, &str)] = &[
        ("novyzeland", "newzealand"),
        ("novykorejsko", "southkorea"),
        ("jiznikorejsko", "southkorea"),
        ("korejskarepublika", "southkorea"),
        ("severnikorejsko", "northkorea"),
        ("cina", "china"),
        ("japonsko", "japan"),
        ("nemecko", "germany"),
        ("rakousko", "austria"),
        ("svycarsko", "switzerland"),
        ("francouz", "france"),    // francouzsko = France (adj)
        ("francie", "france"),
        ("spanelsko", "spain"),
        ("italie", "italy"),
        ("portugalsko", "portugal"),
        ("recko", "greece"),
        ("turecko", "turkey"),
        ("polsko", "poland"),
        ("madarsko", "hungary"),
        ("rumunsko", "romania"),
        ("bulharsko", "bulgaria"),
        ("chorvatsko", "croatia"),
        ("srbsko", "serbia"),
        ("slovinsko", "slovenia"),
        ("slovensko", "slovakia"),
        ("cesko", "czechia"),
        ("ceskarepublika", "czechia"),
        ("rusko", "russia"),
        ("ukrajina", "ukraine"),
        ("belgie", "belgium"),
        ("nizozemsko", "netherlands"),
        ("holandsko", "netherlands"),
        ("dansko", "denmark"),
        ("norsko", "norway"),
        ("svedsko", "sweden"),
        ("finsko", "finland"),
        ("irsko", "ireland"),
        ("skotsko", "scotland"),
        ("brazilie", "brazil"),
        ("argentina", "argentina"),
        ("mexiko", "mexico"),
        ("kanada", "canada"),
        ("australie", "australia"),
        ("indie", "india"),
        ("jihoafrickarepublika", "southafrica"),
        // Tchaj-wan / Chinese Taipei variants
        ("tchajwan", "chinesetaipei"),
        ("tchajpej", "chinesetaipei"),
        ("cinskatajpej", "chinesetaipei"),
    ];
    let mut s = name.to_string();
    let original = s.clone();
    for (cz, en) in translations {
        if s.contains(cz) {
            s = s.replace(cz, en);
        }
    }
    // Sampled debug log: 5% of calls where a translation actually fired
    if s != original {
        let n = COUNTRY_TRANSLATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        if n % 20 == 0 {
            tracing::debug!("COUNTRY_TRANSLATE raw={:?} -> {:?}", original, s);
        }
    }
    s
}

/// Strip Unicode diacritics via NFKD decomposition + filtering combining marks.
/// "Nový Zéland" → "Novy Zeland", "München" → "Munchen"
fn strip_diacritics(s: &str) -> String {
    s.nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect()
}

/// Check if NORM_TRACE should fire (sampling: every NORM_TRACE_SAMPLE_EVERY calls)
fn should_norm_trace() -> bool {
    if !FF_NORM_TRACE.load(Ordering::Relaxed) {
        return false;
    }
    let n = NORM_TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    n % NORM_TRACE_SAMPLE_EVERY == 0
}

// ====================================================================
// Alias cache for token-subset matching (Phase 3.2)
// ====================================================================
#[derive(Clone, Debug)]
struct AliasCacheEntry {
    target_key: String,
    created_at: DateTime<Utc>,
}

type AliasCache = Arc<RwLock<HashMap<String, AliasCacheEntry>>>;

fn new_alias_cache() -> AliasCache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Ultra-common tokens that alone are NOT sufficient for a match
const ULTRA_COMMON_TOKENS: &[&str] = &[
    "fc", "sc", "afc", "bfc", "rc", "ac", "cf", "cd", "sd", "ud",
    "club", "united", "city", "town", "real", "sporting",
    "dynamo", "spartak", "lokomotiv", "olympic", "national",
];

/// Token-subset pair matching: match if BOTH teams have ≥2 meaningful tokens
/// and both team pairs overlap (subset). Returns the matched live_key.
fn token_subset_pair_match<'a>(
    odds_key: &str,
    live_keys: &'a [&str],
) -> Option<&'a str> {
    if !FF_TOKEN_SUBSET_PAIR_ALIAS.load(Ordering::Relaxed) {
        return None;
    }

    let parts: Vec<&str> = odds_key.splitn(2, "::").collect();
    if parts.len() != 2 { return None; }
    let sport = parts[0];
    let teams = parts[1];
    let vs_parts: Vec<&str> = teams.splitn(2, "_vs_").collect();
    if vs_parts.len() != 2 { return None; }
    let (ot_a, ot_b) = (vs_parts[0], vs_parts[1]);

    // Each team needs ≥2 meaningful tokens (chars ≥3, not ultra-common)
    let ot_a_tokens: Vec<&str> = split_to_tokens(ot_a);
    let ot_b_tokens: Vec<&str> = split_to_tokens(ot_b);

    let ot_a_meaningful: Vec<&str> = ot_a_tokens.iter()
        .filter(|t| t.len() >= 3 && !ULTRA_COMMON_TOKENS.contains(t))
        .copied().collect();
    let ot_b_meaningful: Vec<&str> = ot_b_tokens.iter()
        .filter(|t| t.len() >= 3 && !ULTRA_COMMON_TOKENS.contains(t))
        .copied().collect();

    // Guardrail: both teams need ≥2 meaningful tokens
    if ot_a_meaningful.len() < 2 || ot_b_meaningful.len() < 2 {
        return None;
    }

    for &cand in live_keys {
        let cparts: Vec<&str> = cand.splitn(2, "::").collect();
        if cparts.len() != 2 { continue; }
        if cparts[0] != sport { continue; }
        let cteams: Vec<&str> = cparts[1].splitn(2, "_vs_").collect();
        if cteams.len() != 2 { continue; }
        let (ct_a, ct_b) = (cteams[0], cteams[1]);

        let ct_a_tokens: Vec<&str> = split_to_tokens(ct_a);
        let ct_b_tokens: Vec<&str> = split_to_tokens(ct_b);

        // Try both orderings (keys are alphabetically sorted, but check both)
        let fwd = token_pair_overlaps(&ot_a_meaningful, &ot_b_meaningful, &ct_a_tokens, &ct_b_tokens);
        let rev = token_pair_overlaps(&ot_a_meaningful, &ot_b_meaningful, &ct_b_tokens, &ct_a_tokens);

        if fwd || rev {
            return Some(cand);
        }
    }
    None
}

/// Split a normalized team name into tokens (split on non-alphanumeric boundaries)
fn split_to_tokens(name: &str) -> Vec<&str> {
    // Names are already alphanumeric-only, so we can't split on delimiters.
    // Instead, return the name as a single token since it's concatenated.
    // For token-subset, we need at least partial substring matching.
    if name.is_empty() { return vec![]; }
    vec![name]
}

/// Check if odds team tokens overlap with candidate team tokens (both teams simultaneously)
/// Uses substring containment since names are concatenated alphanumeric strings.
fn token_pair_overlaps(
    ot_a: &[&str], ot_b: &[&str],
    ct_a_tokens: &[&str], ct_b_tokens: &[&str],
) -> bool {
    // For concatenated names, check if one contains the other (or significant overlap)
    let ot_a_str: String = ot_a.join("");
    let ot_b_str: String = ot_b.join("");
    let ct_a_str: String = ct_a_tokens.join("");
    let ct_b_str: String = ct_b_tokens.join("");

    if ot_a_str.is_empty() || ot_b_str.is_empty() || ct_a_str.is_empty() || ct_b_str.is_empty() {
        return false;
    }

    // Both teams must match: A↔A' and B↔B' (or A↔B' and B↔A')
    let a_match = substring_overlap(&ot_a_str, &ct_a_str);
    let b_match = substring_overlap(&ot_b_str, &ct_b_str);

    a_match && b_match
}

/// Check if two concatenated team names have significant substring overlap.
/// At least 60% of the shorter name must be contained in the longer name.
fn substring_overlap(a: &str, b: &str) -> bool {
    if a == b { return true; }
    // One contains the other
    if a.contains(b) || b.contains(a) { return true; }

    // Check longest common substring (simplified: prefix/suffix overlap)
    let (shorter, longer) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let min_overlap = (shorter.len() * 60 / 100).max(3);

    // Check if any substring of `shorter` with length >= min_overlap exists in `longer`
    for start in 0..shorter.len() {
        let remaining = shorter.len() - start;
        if remaining < min_overlap { break; }
        let chunk = &shorter[start..start + min_overlap];
        if longer.contains(chunk) {
            return true;
        }
    }
    false
}

fn normalize_tennis_name(name: &str) -> String {
    let name = name.trim();
    let parts: Vec<&str> = name.split_whitespace().collect();

    if parts.len() <= 1 {
        // Single word — just lowercase + alphanumeric
        return name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    }

    // Detect FlashScore format: last part is an INITIAL (has period or ≤1 alphanum char)
    // e.g. "Blanchet U.", "De Stefano S.", "Jimenez Kasintseva V."
    let last = parts.last().unwrap();
    let last_clean: String = last.chars().filter(|c| c.is_alphanumeric()).collect();
    let is_initial = (last.contains('.') && last_clean.len() <= 2)
        || last_clean.len() <= 1;

    if is_initial && parts.len() >= 2 {
        // FlashScore format: take SECOND-TO-LAST word as the surname.
        let surname_word = parts[parts.len() - 2];
        return surname_word.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    }

    // ORDER-INVARIANT approach for Tipsport vs Azuro:
    // Tipsport sends "Surname Firstname" (e.g. "Bicknell Blaise")
    // Azuro sends "Firstname Surname" (e.g. "Blaise Bicknell")
    // Solution: strip particles, sort remaining words alphabetically, take first 2.
    // This produces IDENTICAL output regardless of word order.
    let particles = ["de", "la", "van", "von", "el", "al", "da", "di", "del", "dos", "le", "du"];
    let mut significant: Vec<String> = parts.iter()
        .map(|w| w.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|w| w.len() > 1 && !particles.contains(&w.as_str()))
        .collect();
    significant.sort();
    
    // Take at most 2 significant words for compact keys
    significant.truncate(2);
    if significant.is_empty() {
        // Fallback: just use everything
        return name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    }
    significant.join("")
}

fn normalize_name(name: &str) -> String {
    // Step 0: Strip diacritics via NFKD decomposition
    //  "Nový Zéland" → "Novy Zeland", "München" → "Munchen"
    let stripped = strip_diacritics(name);

    // Strip ALL non-alphanumeric chars so "Thunder Downunder" == "THUNDERdOWNUNDER"
    let mut s: String = stripped.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();

    // Step 1: Translate Czech country names to English
    s = translate_country_name(&s);

    // Strip common prefixes that differ between sources
    // HLTV: "Nemesis", Azuro: "Team Nemesis" → both → "nemesis"
    // Also: "Clan X" vs "X", "FC X" vs "X", "Borussia Dortmund" vs "Dortmund"
    let prefixes = ["team", "clan", "fc", "pro", "cf", "ac", "as", "cd", "rc", "rcd", "sd", "ud",
                    // Football club prefixes (Azuro uses full names, Tipsport abbreviates)
                    "borussia", "real", "sporting", "atletico", "athletic",
                    "dynamo", "lokomotiv", "spartak", "cska", "zenit",
                    "olympique", "olympiacos", "panathinaikos",
                    "besiktas", "galatasaray", "fenerbahce",
                    "alahly", "al", "est",
                    // German club prefixes
                    "vfb", "vfl", "tsv", "sv", "sc",
                    // Country prefixes for national teams
                    "republic",
                    ];
    for prefix in &prefixes {
        if s.len() > prefix.len() + 2 && s.starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            break;
        }
    }

    // Strip common suffixes that differ between sources
    // "Newells Old Boys" → "newells", "Celtic FC" → "celtic", "Corinthians SP" → "corinthians"
    // "Corinthians MG" → "corinthians", "Flamengo RJ" → "flamengo"
    let suffixes = ["gaming", "esports", "esport", "gg", "club", "org",
                    "academy", "rising", "fe",
                    // Club suffixes
                    "fc", "cf", "sc", "ac",
                    // Brazilian state abbreviations (appear in Azuro names)
                    "sp", "mg", "rj", "rs", "ba", "pr", "ce", "go", "pe",
                    // Other common suffixes
                    "oldboys", "united", "city", "wanderers",
                    // Full-name suffixes that Azuro appends
                    "turin", "madrid", "münchen", "munchen",
                    "london", "paris", "milan", "rome", "roma",
                    // Azuro specific
                    "whitecapsfc", "whitecaps",
                    // National team suffixes (order matters — longest first)
                    "u20w", "u23w", "u21w", "u20", "u21", "u23", "women", "w",
                    // Azuro-specific name suffixes
                    "fotball", "fotbal", "football",
                    ];
    for suffix in &suffixes {
        if s.len() > suffix.len() + 3 && s.ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            break;
        }
    }

    // Extended suffixes (behind kill-switch FF_EXTENDED_SUFFIX_STRIP)
    if FF_EXTENDED_SUFFIX_STRIP.load(Ordering::Relaxed) {
        let ext_suffixes = [
            "utd", "town", "youth", "npl", "reserves", "junior",
            "afc", "bfc", "rovers", "athletic",
            "hotspurs", "albion", "argyle", "county",
        ];
        for suffix in &ext_suffixes {
            if s.len() > suffix.len() + 3 && s.ends_with(suffix) {
                s.truncate(s.len() - suffix.len());
                break;
            }
        }
    }

    // Spelling aliases: normalize variant spellings to a single canonical form
    // "Athletico Paranaense" (Azuro) vs "Atletico-PR" (Tipsport) — 'th' vs 't'
    if s.starts_with("athletico") { s = "atletico".to_string() + &s["athletico".len()..]; }
    // "Al Jazeera" vs "Al Jazira" — same club, different transliteration
    s = s.replace("jazeera", "jazira");
    // Compound country names: strip "and" connector that some sources include
    // "trinidadandtobago" → "trinidadtobago", "antiguaandbarbuda" → "antiguabarbuda"
    // "bosniaandherzegovina" → "bosniaherzegovina"
    // SAFE: only target known patterns, NOT generic "and" removal (would break "anderson" etc.)
    for compound in &["trinidadand", "antiguaand", "bosniaand", "saintvincentand"] {
        if s.contains(*compound) {
            s = s.replace(&format!("{}",*compound), &compound.replace("and", ""));
        }
    }

    // Strip trailing digits that some sources append (e.g. team name duplicates)
    while s.len() > 3 {
        if let Some(last) = s.chars().last() {
            if last.is_ascii_digit() {
                s.pop();
            } else {
                break;
            }
        } else {
            break;
        }
    }

    s
}

fn normalize_esports_token(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn strip_esports_tournament_tail(name: &str) -> String {
    let raw: Vec<String> = name
        .split(|c: char| !c.is_alphanumeric())
        .map(normalize_esports_token)
        .filter(|t| !t.is_empty())
        .collect();

    if raw.is_empty() {
        return String::new();
    }

    let anchors = [
        "pgl", "blast", "iem", "esl", "cct", "betboom", "rush", "rushb", "summit",
        "qualifier", "qualifiers", "playoff", "playoffs", "season", "masters", "bucharest",
        "cracovia", "open", "closed", "major", "minor",
    ];

    let mut cut_idx: Option<usize> = None;
    for (idx, token) in raw.iter().enumerate() {
        if idx == 0 {
            continue;
        }
        if anchors.contains(&token.as_str()) {
            cut_idx = Some(idx);
            break;
        }
    }

    let kept = match cut_idx {
        Some(idx) => raw.into_iter().take(idx).collect::<Vec<_>>(),
        None => raw,
    };

    if kept.is_empty() {
        return String::new();
    }

    kept.join(" ")
}

fn esports_team_signature(name: &str) -> Option<String> {
    let stopwords = [
        "team", "clan", "esports", "esport", "gaming", "club", "academy",
        "qualifier", "open", "closed", "playoff", "playoffs", "group", "stage",
        "pgl", "blast", "iem", "esl", "season", "cup", "masters", "summit",
        "rush", "rushb", "betboom", "bucharest", "cracovia", "south", "north",
        "america", "europe", "final", "finals", "regional", "major", "minor",
    ];

    let stripped = strip_esports_tournament_tail(name);
    let source = if stripped.is_empty() { name } else { stripped.as_str() };

    let mut tokens: Vec<String> = source
        .split(|c: char| !c.is_alphanumeric())
        .map(normalize_esports_token)
        .filter(|t| !t.is_empty())
        .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
        .filter(|t| t.len() >= 2)
        .filter(|t| !stopwords.contains(&t.as_str()))
        .collect();

    if tokens.is_empty() {
        return None;
    }

    tokens.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));

    let first = tokens[0].clone();
    let second = tokens.get(1).cloned();

    if let Some(second) = second {
        if second.len() >= 3 {
            return Some(format!("{}{}", first, second));
        }
    }

    Some(first)
}

fn normalize_esports_name(name: &str) -> String {
    let mut s = esports_team_signature(name).unwrap_or_else(|| normalize_name(name));

    // Some scrapers occasionally append tournament tail into team name
    // e.g. "bountyhunterspglbucharest2026southamerica", "galorysbetboomrushbsummit".
    // Strip only known esports-event suffixes and keep core team token.
    let tail_suffixes = [
        "pglbucharest2026southamerica",
        "pglbucharest2026",
        "digitalcracovianseason5",
        "betboomrushbsummit",
        "closedqualifier",
        "openqualifier",
        "southamerica",
        "northamerica",
        "qualifier",
    ];

    for suffix in &tail_suffixes {
        if s.len() > suffix.len() + 4 && s.ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            break;
        }
    }

    while s.len() > 4 {
        if let Some(last) = s.chars().last() {
            if last.is_ascii_digit() {
                s.pop();
            } else {
                break;
            }
        } else {
            break;
        }
    }

    s
}

fn match_key(sport: &str, team1: &str, team2: &str) -> String {
    let sport_lower = normalize_sport(sport);
    // All esports game labels (cs2, dota-2, valorant, lol, starcraft, esports)
    // are normalized to a SINGLE "esports" prefix.  This eliminates the need
    // for write-time alias explosion.  The actual game title is still preserved
    // in payload.sport for downstream consumers (Azuro execution, etc.).
    let is_esports = matches!(sport_lower.as_str(),
        "cs2" | "dota-2" | "valorant" | "league-of-legends" | "lol" | "starcraft" | "esports"
    );
    let sport_prefix = if is_esports { "esports" } else { sport_lower.as_str() };

    // Tennis uses surname-only matching (FlashScore "Blanchet U." ↔ Azuro "Ugo Blanchet")
    let (a, b) = if sport_lower == "tennis" {
        (normalize_tennis_name(team1), normalize_tennis_name(team2))
    } else if is_esports {
        (normalize_esports_name(team1), normalize_esports_name(team2))
    } else {
        (normalize_name(team1), normalize_name(team2))
    };
    // Sort alphabetically so team order doesn't matter for matching
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    format!("{}::{}_vs_{}", sport_prefix, first, second)
}

/// Given a match_key, return all sport-alias variants.
/// E.g. "esports::a_vs_b" → ["cs2::a_vs_b", "dota-2::a_vs_b", ...]
/// and "cs2::a_vs_b" → ["esports::a_vs_b"]
fn match_key_aliases(key: &str) -> Vec<String> {
    let esports_games: &[&str] = &[
        "cs2", "dota-2", "league-of-legends", "lol", "valorant", "starcraft",
    ];
    if let Some(tail) = key.strip_prefix("esports::") {
        esports_games.iter().map(|g| format!("{}::{}", g, tail)).collect()
    } else {
        for g in esports_games {
            if let Some(tail) = key.strip_prefix(&format!("{}::", g)) {
                return vec![format!("esports::{}", tail)];
            }
        }
        vec![]
    }
}

fn parse_ts(ts: &Option<String>) -> DateTime<Utc> {
    ts.as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn gate_odds(odds: &OddsPayload, seen_at: DateTime<Utc>) -> (bool, String) {
    // null = ok (Azuro doesn't always send liquidity/spread)
    let liquidity_ok = odds.liquidity_usd.map_or(true, |l| l >= 500.0);
    let spread_ok = odds.spread_pct.map_or(true, |s| s <= 5.0);

    let age = Utc::now().signed_duration_since(seen_at);
    let stale_ok = age.num_seconds().abs() <= 10;

    if !liquidity_ok {
        return (false, "liquidity<2000".to_string());
    }
    if !spread_ok {
        return (false, "spread>1.5%".to_string());
    }
    if !stale_ok {
        return (false, "stale>10s".to_string());
    }

    (true, "ok".to_string())
}

#[derive(Debug, Clone, Serialize)]
struct HttpLiveItem {
    match_key: String,
    source: String,
    seen_at: String,
    payload: LiveMatchPayload,
}

#[derive(Debug, Clone, Serialize)]
struct HttpOddsItem {
    match_key: String,
    source: String,
    seen_at: String,
    payload: OddsPayload,
}

#[derive(Debug, Clone, Serialize)]
struct HttpStateResponse {
    ts: String,
    connections: usize,
    live_items: usize,
    odds_items: usize,
    fused_ready: usize,
    fused_keys: Vec<String>,
    live: Vec<HttpLiveItem>,
    odds: Vec<HttpOddsItem>,
}

// ====================================================================
// OPPORTUNITIES — value/arb detection
// ====================================================================

#[derive(Debug, Clone, Serialize)]
struct Opportunity {
    match_key: String,
    /// "value_bet" | "score_momentum" | "arb_cross_book"
    opp_type: String,
    team1: String,
    team2: String,
    score: String,
    detailed_score: Option<String>,
    /// Which team has value: 1 or 2
    value_side: u8,
    /// Description of the signal
    signal: String,
    /// Confidence 0.0..1.0
    confidence: f64,
    /// The odds that represent value
    odds: f64,
    /// Implied probability from odds
    implied_prob_pct: f64,
    /// Our estimated fair probability based on score
    estimated_fair_pct: f64,
    /// Edge = estimated_fair - implied (positive = value)
    edge_pct: f64,
    bookmaker: String,
    odds_age_secs: i64,
    live_age_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
struct OpportunitiesResponse {
    ts: String,
    total_live: usize,
    total_odds: usize,
    fused_matches: usize,
    opportunities: Vec<Opportunity>,
}

// ====================================================================
// SPORT-SPECIFIC FAIR PROBABILITY MODELS
// ====================================================================
// Each model parses `detailed_score` and returns a fair win probability
// for team1 (0.0 to 100.0). Returns None if unable to parse.

/// Parse tennis detailed_score and compute fair win probability for team1.
/// Format: "1:0 2.set - 6:2, 3:2 (15:30*)"
///   sets1:sets2 → set score
///   games after last comma → current set game score
///   (XX:YY*) → point score, * = serving
///
/// Tennis probabilities (Bo3, empirical ATP data):
///   1-0 sets: ~73% for leader
///   0-0 sets, leading in games: 50% + game_lead * 3%
///   2-0 sets: ~97%
fn tennis_fair_pct(detailed: &str, sets1: i64, sets2: i64) -> Option<f64> {
    // Parse current set game score from detailed_score
    // Pattern: "N.set - X:Y, G1:G2" or "N.set - G1:G2"
    let games = parse_tennis_games(detailed);
    let (g1, g2) = games.unwrap_or((0, 0));

    let set_diff = sets1 - sets2;
    let game_diff = g1 - g2;

    // Base probabilities by set score (Bo3 empirical)
    let base = match (sets1, sets2) {
        (2, _) => return Some(97.0), // Won match
        (_, 2) => return Some(3.0),
        (1, 0) => 73.0, // Up one set
        (0, 1) => 27.0, // Down one set
        (1, 1) => 50.0, // Level
        (0, 0) => 50.0, // First set
        _ => 50.0,
    };

    // Adjust by current set game score
    // Each game lead ≈ 4% swing in set probability
    // A set lead translates to ~3% match probability adjustment
    let game_adj = (game_diff as f64) * 3.0;

    // Clamp to reasonable range
    Some((base + game_adj).max(5.0).min(95.0))
}

/// Parse tennis game score from detailed_score string
/// Returns (games1, games2) for the current set
fn parse_tennis_games(detailed: &str) -> Option<(i64, i64)> {
    // Look for pattern like "set - X:Y, G1:G2" or just "set - G1:G2"
    // The LAST "N:N" pattern before "(" is typically the game score
    // But we need to skip the set score at the start

    // Find the portion after "set -" or "set-"
    let set_idx = detailed.find("set");
    if let Some(idx) = set_idx {
        let after_set = &detailed[idx..];
        // Find the dash after "set"
        if let Some(dash) = after_set.find('-') {
            let games_part = &after_set[dash + 1..];
            // Now find the LAST colon-separated number pair before "("
            // Could be "6:2, 3:2 (15:30*)" → we want "3:2"
            // Or just "3:2 (15:30*)" → we want "3:2"
            let before_paren = if let Some(p) = games_part.find('(') {
                &games_part[..p]
            } else {
                games_part
            };

            // Split by comma, take last part
            let parts: Vec<&str> = before_paren.split(',').collect();
            let last = parts.last()?;
            // Parse "G1:G2" from this last part
            let re_score = last.trim();
            let colon = re_score.find(':')?;
            let g1: i64 = re_score[..colon].trim().parse().ok()?;
            let g2: i64 = re_score[colon + 1..].trim().parse().ok()?;
            return Some((g1, g2));
        }
    }
    None
}

/// Parse football detailed_score and compute fair win probability for team1.
/// Format: "1:0 1.pol. - 32.min (1:0)" or "0:0 2.pol. - 54.min (0:0, 0:0)"
///
/// Football model (simplified):
///   Goal lead value depends heavily on minute:
///   - Early (0-30min): +1 goal ≈ 60% win
///   - Mid (30-60min): +1 goal ≈ 70% win
///   - Late (60-80min): +1 goal ≈ 82% win
///   - Very late (80-90+min): +1 goal ≈ 90% win
///   - Draw at 0-0: 50%
///   Multi-goal leads are exponentially safer.
fn football_fair_pct(detailed: &str, score1: i64, score2: i64) -> Option<f64> {
    let minute = parse_football_minute(detailed)?;
    let goal_diff = score1 - score2;

    if goal_diff == 0 {
        // Draw — slight advantage to the team that's been attacking
        // But we can't tell from score alone, so 50%
        return Some(50.0);
    }

    // Base win probability for a 1-goal lead by minute
    let one_goal_base = if minute <= 15 {
        57.0
    } else if minute <= 30 {
        62.0
    } else if minute <= 45 {
        67.0
    } else if minute <= 60 {
        72.0
    } else if minute <= 70 {
        78.0
    } else if minute <= 80 {
        84.0
    } else if minute <= 85 {
        88.0
    } else {
        92.0 // 85+ minutes
    };

    // For multi-goal leads, each additional goal adds significant safety
    let fair = if goal_diff > 0 {
        let extra = (goal_diff - 1).max(0) as f64 * 8.0;
        (one_goal_base + extra).min(98.0)
    } else {
        // team1 is BEHIND
        let behind_fair = 100.0 - one_goal_base;
        let extra = (goal_diff.abs() - 1) as f64 * 8.0;
        (behind_fair - extra).max(2.0)
    };

    Some(fair)
}

/// Parse minute from football detailed_score
/// Patterns: "32.min", "54.min", "<1min"
fn parse_football_minute(detailed: &str) -> Option<i64> {
    // Pattern 1: "NN.min"
    if let Some(min_idx) = detailed.find(".min") {
        let before = &detailed[..min_idx];
        // Take last contiguous digits before ".min"
        let digits: String = before.chars().rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars().rev().collect();
        if let Ok(min) = digits.parse::<i64>() {
            return Some(min);
        }
    }
    // Pattern 2: "<Nmin"
    if let Some(lt_idx) = detailed.find('<') {
        let after = &detailed[lt_idx + 1..];
        let digits: String = after.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(min) = digits.parse::<i64>() {
            return Some(min);
        }
    }
    // Pattern 3: ".pol." without minute → estimate from half
    if detailed.contains("1.pol") {
        return Some(25); // first half, assume ~25th min
    }
    if detailed.contains("2.pol") {
        return Some(65); // second half, assume ~65th min
    }
    None
}

/// Compute sport-specific fair probability for the LEADING team.
/// Returns (fair_pct_team1, model_name) or None if no sport model applies.
fn compute_sport_fair_pct(
    sport: &str,
    score1: i64,
    score2: i64,
    detailed_score: &Option<String>,
) -> Option<(f64, &'static str)> {
    let ds = detailed_score.as_deref().unwrap_or("");

    match sport {
        "tennis" => {
            tennis_fair_pct(ds, score1, score2)
                .map(|f| (f, "tennis_model"))
        }
        "football" => {
            football_fair_pct(ds, score1, score2)
                .map(|f| (f, "football_model"))
        }
        // Esports: keep existing CS2/Dota map-based model
        "cs2" | "dota-2" | "league-of-legends" | "valorant" | "esports" => {
            let diff = score1 - score2;
            let fair = match diff.abs() {
                0 => 50.0,
                1 => if diff > 0 { 68.0 } else { 32.0 },
                2 => if diff > 0 { 95.0 } else { 5.0 },
                _ => if diff > 0 { 97.0 } else { 3.0 },
            };
            Some((fair, "esport_map_model"))
        }
        "basketball" => {
            // Basketball: score difference matters but games are high-scoring
            // A 10-point lead in 4th quarter ≈ 80%
            // For now, use simple model: each point diff ≈ 1.5% edge
            let diff = score1 - score2;
            let fair = (50.0 + diff as f64 * 1.5).max(5.0).min(95.0);
            Some((fair, "basketball_model"))
        }
        _ => None,
    }
}

/// Fuzzy key resolver: given a match_key like "tennis::li_vs_sonmez",
/// find a matching key in `candidates` where team tokens are suffix-contained.
/// E.g. "tennis::li_vs_sonmez" matches "tennis::annli_vs_sonmezzeynep"
///       because "li" is a suffix of "annli" AND "sonmez" is a suffix of "sonmezzeynep".
/// Only matches within the same sport prefix.
fn fuzzy_find_key<'a>(needle: &str, candidates: &'a [&str]) -> Option<&'a str> {
    let parts: Vec<&str> = needle.splitn(2, "::").collect();
    if parts.len() != 2 { return None; }
    let sport = parts[0];
    let teams = parts[1];
    let vs_parts: Vec<&str> = teams.splitn(2, "_vs_").collect();
    if vs_parts.len() != 2 { return None; }
    let (n_a, n_b) = (vs_parts[0], vs_parts[1]);

    for &cand in candidates {
        let cparts: Vec<&str> = cand.splitn(2, "::").collect();
        if cparts.len() != 2 { continue; }
        if cparts[0] != sport { continue; }
        let cteams: Vec<&str> = cparts[1].splitn(2, "_vs_").collect();
        if cteams.len() != 2 { continue; }
        let (c_a, c_b) = (cteams[0], cteams[1]);

        // Check: needle teams are substrings (suffix) of candidate teams
        // or vice versa. Match both orderings (sorted, but just in case).
        let match_fwd = (c_a.ends_with(n_a) || n_a.ends_with(c_a))
                     && (c_b.ends_with(n_b) || n_b.ends_with(c_b));
        let match_rev = (c_a.ends_with(n_b) || n_b.ends_with(c_a))
                     && (c_b.ends_with(n_a) || n_a.ends_with(c_b));
        if match_fwd || match_rev {
            return Some(cand);
        }
    }
    None
}

async fn build_opportunities(state: &FeedHubState) -> OpportunitiesResponse {
    let live_map = state.live.read().await;
    let odds_map = state.odds.read().await;
    let now = Utc::now();

    let total_live = live_map.len();
    let total_odds = odds_map.len();

    // Group odds by match_key
    let mut odds_by_match: HashMap<&str, Vec<&OddsState>> = HashMap::new();
    for (ok, ov) in odds_map.iter() {
        odds_by_match.entry(&ok.match_key).or_default().push(ov);
    }

    // Collect odds keys for fuzzy matching fallback
    let odds_keys_vec: Vec<&str> = odds_by_match.keys().copied().collect();

    let fused_matches = odds_by_match.keys()
        .filter(|k| live_map.contains_key(**k))
        .count();

    let mut opportunities = Vec::new();

    for (match_key, live) in live_map.iter() {
        // Try alternate sport prefixes for ANY live key that doesn't match Azuro directly.
        // FlashScore/Tipsport may label a match as 'esports' while Azuro uses 'cs2',
        // 'dota-2', 'basketball', 'football' etc.
        // Also: Tipsport 'basketball'/'football' live won't match if Azuro key differs slightly.
        let esports_alts: &[&str] = &[
            "cs2", "dota-2", "league-of-legends", "valorant",
            "basketball", "football", "mma", "starcraft",
        ];
        let odds_list_opt = odds_by_match.get(match_key.as_str())
            .or_else(|| {
                if match_key.starts_with("esports::") {
                    let tail = &match_key["esports::".len()..];
                    esports_alts.iter().find_map(|alt| {
                        let alt_key = format!("{}::{}", alt, tail);
                        odds_by_match.get(alt_key.as_str())
                    })
                } else {
                    None
                }
            })
            // Fuzzy suffix matching: handles Fortuna short names vs Tipsport/Azuro full names
            // E.g. "tennis::li_vs_sonmez" matches "tennis::annli_vs_sonmezzeynep"
            .or_else(|| {
                fuzzy_find_key(match_key.as_str(), &odds_keys_vec)
                    .and_then(|fk| odds_by_match.get(fk))
            });
        let Some(odds_list) = odds_list_opt else {
            continue;
        };

        let score1 = live.payload.score1.unwrap_or(0);
        let score2 = live.payload.score2.unwrap_or(0);
        let score_str = format!("{}-{}", score1, score2);
        let live_age = now.signed_duration_since(live.seen_at).num_seconds();

        // === SPORT-AWARE SCORE SANITY CHECK ===
        // Catches mislabeled sports: basketball game tagged as football, etc.
        let sport_in_key = match_key.split("::").next().unwrap_or("unknown");
        let score_looks_valid = match sport_in_key {
            "football" => score1 <= 8 && score2 <= 8,   // max realistic football score (tightened)
            "hockey" => score1 <= 10 && score2 <= 10,    // max realistic hockey score
            "cs2" | "dota-2" | "league-of-legends" | "valorant" => {
                // Map scores: 0-3 range for match level
                score1 <= 3 && score2 <= 3
            },
            "tennis" => score1 <= 5 && score2 <= 5, // sets: max 5
            "basketball" => score1 <= 200 && score2 <= 200, // max realistic basketball score per team
            "mma" | "boxing" => score1 <= 5 && score2 <= 5,
            "handball" => score1 <= 45 && score2 <= 45,
            "volleyball" => score1 <= 5 && score2 <= 5,
            _ => score1 <= 50 && score2 <= 50,  // generic safety net for unknown sports
        };
        if !score_looks_valid {
            // Skip — score doesn't match sport type, likely mislabeled
            continue;
        }

        for odds_state in odds_list {
            let odds = &odds_state.payload;
            let odds_age = now.signed_duration_since(odds_state.seen_at).num_seconds();

            // Skip stale odds (>60s)
            if odds_age > 60 { continue; }

            let implied1 = 1.0 / odds.odds_team1 * 100.0;
            let implied2 = 1.0 / odds.odds_team2 * 100.0;

            // === SCORE MOMENTUM DETECTION (SPORT-SPECIFIC MODELS) ===
            // Uses detailed_score to compute accurate fair probability per sport
            let score_diff = score1 - score2;

            if score_diff != 0 {
                // Try sport-specific model first, fallback to generic
                let (fair1, model) = compute_sport_fair_pct(
                    sport_in_key, score1, score2, &live.payload.detailed_score
                ).unwrap_or_else(|| {
                    // Generic fallback (old model)
                    let f = match score_diff.abs() {
                        1 => if score_diff > 0 { 68.0 } else { 32.0 },
                        2 => if score_diff > 0 { 95.0 } else { 5.0 },
                        _ => if score_diff > 0 { (implied1 + 15.0).min(95.0) } else { (implied2 - 15.0).max(5.0) },
                    };
                    (f, "generic")
                });
                // fair1 = fair probability for TEAM1
                let fair2 = 100.0 - fair1;

                // Check team1 value: fair1 > implied1
                let edge1 = fair1 - implied1;
                if edge1 > 3.0 && implied1 < 85.0 {
                    opportunities.push(Opportunity {
                        match_key: match_key.clone(),
                        opp_type: "score_momentum".to_string(),
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score: score_str.clone(),
                        detailed_score: live.payload.detailed_score.clone(),
                        value_side: 1,
                        signal: format!("{} leads {}, {} fair={:.0}% vs odds={:.0}%",
                            live.payload.team1, score_str, model, fair1, implied1),
                        confidence: (edge1 / 20.0).min(1.0),
                        odds: odds.odds_team1,
                        implied_prob_pct: (implied1 * 100.0).round() / 100.0,
                        estimated_fair_pct: (fair1 * 100.0).round() / 100.0,
                        edge_pct: (edge1 * 100.0).round() / 100.0,
                        bookmaker: odds.bookmaker.clone(),
                        odds_age_secs: odds_age,
                        live_age_secs: live_age,
                    });
                }

                // Check team2 value: fair2 > implied2
                let edge2 = fair2 - implied2;
                if edge2 > 3.0 && implied2 < 85.0 {
                    opportunities.push(Opportunity {
                        match_key: match_key.clone(),
                        opp_type: "score_momentum".to_string(),
                        team1: live.payload.team1.clone(),
                        team2: live.payload.team2.clone(),
                        score: score_str.clone(),
                        detailed_score: live.payload.detailed_score.clone(),
                        value_side: 2,
                        signal: format!("{} trails {}, {} fair={:.0}% vs odds={:.0}%",
                            live.payload.team2, score_str, model, fair2, implied2),
                        confidence: (edge2 / 20.0).min(1.0),
                        odds: odds.odds_team2,
                        implied_prob_pct: (implied2 * 100.0).round() / 100.0,
                        estimated_fair_pct: (fair2 * 100.0).round() / 100.0,
                        edge_pct: (edge2 * 100.0).round() / 100.0,
                        bookmaker: odds.bookmaker.clone(),
                        odds_age_secs: odds_age,
                        live_age_secs: live_age,
                    });
                }
            }

            // === SPREAD CHECK (single bookmaker) ===
            // Very low spread (<3%) = bookmaker very sure → potential value on the underdog
            // High spread (>12%) = bookmaker unsure → avoid
            let spread = (implied1 + implied2 - 100.0).abs();
            if spread < 3.0 && (odds.odds_team1 > 2.5 || odds.odds_team2 > 2.5) {
                let (side, underdog_odds, underdog_implied) = if odds.odds_team1 > odds.odds_team2 {
                    (1u8, odds.odds_team1, implied1)
                } else {
                    (2u8, odds.odds_team2, implied2)
                };
                let fair = underdog_implied + 5.0;
                let edge = fair - underdog_implied;
                let underdog_name = if side == 1 { &live.payload.team1 } else { &live.payload.team2 };
                opportunities.push(Opportunity {
                    match_key: match_key.clone(),
                    opp_type: "tight_spread_underdog".to_string(),
                    team1: live.payload.team1.clone(),
                    team2: live.payload.team2.clone(),
                    score: score_str.clone(),
                    detailed_score: live.payload.detailed_score.clone(),
                    value_side: side,
                    signal: format!("Tight spread {:.1}%, {} at {:.2}",
                        spread, underdog_name, underdog_odds),
                    confidence: 0.3,
                    odds: underdog_odds,
                    implied_prob_pct: (underdog_implied * 100.0).round() / 100.0,
                    estimated_fair_pct: (fair * 100.0).round() / 100.0,
                    edge_pct: (edge * 100.0).round() / 100.0,
                    bookmaker: odds.bookmaker.clone(),
                    odds_age_secs: odds_age,
                    live_age_secs: live_age,
                });
            }
        }

        // === CROSS-BOOKMAKER ARB (if multiple bookmaker odds for same match) ===
        if odds_list.len() >= 2 {
            for i in 0..odds_list.len() {
                for j in (i+1)..odds_list.len() {
                    let a = &odds_list[i].payload;
                    let b = &odds_list[j].payload;

                    // Skip correlated same-platform markets (e.g. azuro_polygon vs azuro_polygon_map3_winner)
                    // These appear as huge "ARB" but are NOT independent bets — they're map/match sub-markets
                    // of the same underlying book. Real ARB requires genuinely different bookmakers.
                    let a_platform = a.bookmaker.split(|c: char| c == '_' || c == '-').next().unwrap_or("");
                    let b_platform = b.bookmaker.split(|c: char| c == '_' || c == '-').next().unwrap_or("");
                    if a_platform == b_platform && !a_platform.is_empty() {
                        continue; // Same underlying book — skip false ARB
                    }
                    // Check arb: 1/odds_a_team1 + 1/odds_b_team2 < 1
                    let arb1 = 1.0 / a.odds_team1 + 1.0 / b.odds_team2;
                    let arb2 = 1.0 / a.odds_team2 + 1.0 / b.odds_team1;
                    if arb1 < 1.0 {
                        let profit_pct = (1.0 - arb1) * 100.0;
                        opportunities.push(Opportunity {
                            match_key: match_key.clone(),
                            opp_type: "arb_cross_book".to_string(),
                            team1: live.payload.team1.clone(),
                            team2: live.payload.team2.clone(),
                            score: score_str.clone(),
                            detailed_score: live.payload.detailed_score.clone(),
                            value_side: 0,
                            signal: format!("ARB {:.2}%: {} t1@{:.2}({}) + t2@{:.2}({})",
                                profit_pct, match_key, a.odds_team1, a.bookmaker,
                                b.odds_team2, b.bookmaker),
                            confidence: (profit_pct / 5.0).min(1.0),
                            odds: a.odds_team1,
                            implied_prob_pct: arb1 * 100.0,
                            estimated_fair_pct: 100.0,
                            edge_pct: (profit_pct * 100.0).round() / 100.0,
                            bookmaker: format!("{}+{}", a.bookmaker, b.bookmaker),
                            odds_age_secs: 0,
                            live_age_secs: live_age,
                        });
                    }
                    if arb2 < 1.0 {
                        let profit_pct = (1.0 - arb2) * 100.0;
                        opportunities.push(Opportunity {
                            match_key: match_key.clone(),
                            opp_type: "arb_cross_book".to_string(),
                            team1: live.payload.team1.clone(),
                            team2: live.payload.team2.clone(),
                            score: score_str.clone(),
                            detailed_score: live.payload.detailed_score.clone(),
                            value_side: 0,
                            signal: format!("ARB {:.2}%: {} t2@{:.2}({}) + t1@{:.2}({})",
                                profit_pct, match_key, a.odds_team2, a.bookmaker,
                                b.odds_team1, b.bookmaker),
                            confidence: (profit_pct / 5.0).min(1.0),
                            odds: a.odds_team2,
                            implied_prob_pct: arb2 * 100.0,
                            estimated_fair_pct: 100.0,
                            edge_pct: (profit_pct * 100.0).round() / 100.0,
                            bookmaker: format!("{}+{}", a.bookmaker, b.bookmaker),
                            odds_age_secs: 0,
                            live_age_secs: live_age,
                        });
                    }
                }
            }
        }
    }

    // Sort by edge descending
    opportunities.sort_by(|a, b| b.edge_pct.partial_cmp(&a.edge_pct).unwrap_or(std::cmp::Ordering::Equal));

    OpportunitiesResponse {
        ts: Utc::now().to_rfc3339(),
        total_live,
        total_odds,
        fused_matches,
        opportunities,
    }
}

async fn build_state_snapshot(state: &FeedHubState) -> HttpStateResponse {
    let connections = *state.connections.read().await;
    let live_map = state.live.read().await;
    let odds_map = state.odds.read().await;

    let live_items = live_map.len();
    let odds_items = odds_map.len();

    // Collect unique match keys from odds
    let mut odds_match_keys = std::collections::HashSet::new();
    for ok in odds_map.keys() {
        odds_match_keys.insert(ok.match_key.clone());
    }

    // Esports fallback alts — same as in build_opportunities
    let esports_alts_snap: &[&str] = &[
        "cs2", "dota-2", "league-of-legends", "valorant",
        "basketball", "football", "mma", "starcraft",
    ];
    // Collect live keys for fuzzy matching
    let live_keys_vec: Vec<&str> = live_map.keys().map(|s| s.as_str()).collect();

    let mut fused_keys = Vec::new();
    for k in &odds_match_keys {
        let is_fused = if live_map.contains_key(k) {
            true
        } else {
            // Check if any live key with esports:: prefix matches via alt
            let parts: Vec<&str> = k.splitn(2, "::").collect();
            if parts.len() == 2 {
                let tail = parts[1];
                esports_alts_snap.iter().any(|alt| {
                    if *alt == parts[0] {
                        // live key would be esports::tail
                        live_map.contains_key(&format!("esports::{}", tail))
                    } else {
                        false
                    }
                })
            } else { false }
        };
        // Fuzzy suffix matching: Fortuna short names vs Tipsport/Azuro full names
        let is_fused = is_fused || fuzzy_find_key(k.as_str(), &live_keys_vec).is_some();
        if is_fused {
            fused_keys.push(k.clone());
        }
        // No limit on fused_keys — alert-bot needs full picture
    }

    let fused_ready = fused_keys.len();

    let mut live = Vec::new();
    for (k, v) in live_map.iter() {
        live.push(HttpLiveItem {
            match_key: k.clone(),
            source: v.source.clone(),
            seen_at: v.seen_at.to_rfc3339(),
            payload: v.payload.clone(),
        });
    }

    let mut odds = Vec::new();
    for (k, v) in odds_map.iter() {
        odds.push(HttpOddsItem {
            match_key: k.match_key.clone(),
            source: v.source.clone(),
            seen_at: v.seen_at.to_rfc3339(),
            payload: v.payload.clone(),
        });
    }

    HttpStateResponse {
        ts: Utc::now().to_rfc3339(),
        connections,
        live_items,
        odds_items,
        fused_ready,
        fused_keys,
        live,
        odds,
    }
}

/// Fortuna scraper inbound JSON
#[derive(Debug, Deserialize)]
struct FortunaInbound {
    #[allow(dead_code)]
    timestamp: Option<u64>,
    source: Option<String>,
    matches: Vec<FortunaMatch>,
}

#[derive(Debug, Deserialize)]
struct FortunaMatch {
    sport: String,
    league: Option<String>,
    team1: String,
    team2: String,
    score1: Option<i64>,
    score2: Option<i64>,
    status: Option<String>,
    odds: Vec<FortunaOddsEntry>,
}

#[derive(Debug, Deserialize)]
struct FortunaOddsEntry {
    market: Option<String>,
    label: Option<String>,
    value: Option<f64>,
}

async fn handle_fortuna_post(state: &FeedHubState, body_str: &str) -> (bool, String) {
    let parsed: Result<FortunaInbound, _> = serde_json::from_str(body_str);
    let inbound = match parsed {
        Ok(v) => v,
        Err(e) => return (false, format!("parse error: {}", e)),
    };

    let now = Utc::now();
    let source = inbound.source.unwrap_or_else(|| "fortuna".to_string());
    let mut live_count = 0usize;
    let mut odds_count = 0usize;

    for m in &inbound.matches {
        let sport = &m.sport;
        let key = match_key(sport, &m.team1, &m.team2);

        // NORM_TRACE (sampled)
        if should_norm_trace() {
            debug!(
                "NORM_TRACE src=fortuna sport={} raw={}|{} key={}",
                sport, m.team1, m.team2, key
            );
        }

        let mut score1 = m.score1;
        let mut score2 = m.score2;

        // Server-side safety net: Fortuna občas pošle nereálné skóre pro football
        // (např. 12-12, 30-30). Nechceme, aby to poškodilo score-edge logiku.
        if sport.eq_ignore_ascii_case("football") {
            if let (Some(s1), Some(s2)) = (score1, score2) {
                if s1 < 0 || s2 < 0 || s1 > 7 || s2 > 7 {
                    warn!(
                        "[FORTUNA] sanitized suspicious football score {}-{} for {} vs {}",
                        s1, s2, m.team1, m.team2
                    );
                    score1 = Some(0);
                    score2 = Some(0);
                }
            }
        }

        if sport.eq_ignore_ascii_case("ice-hockey") {
            if let (Some(s1), Some(s2)) = (score1, score2) {
                if s1 < 0 || s2 < 0 || s1 > 15 || s2 > 15 {
                    warn!(
                        "[FORTUNA] sanitized suspicious ice-hockey score {}-{} for {} vs {}",
                        s1, s2, m.team1, m.team2
                    );
                    score1 = Some(0);
                    score2 = Some(0);
                }
            }
        }

        // Server-side tennis score guard: Fortuna sometimes sends concatenated
        // game scores (63:6) instead of set scores (1:0).  Real tennis set
        // scores in Bo3/Bo5 are at most 3 or 5.
        if sport == "tennis" {
            let s1 = score1.unwrap_or(0);
            let s2 = score2.unwrap_or(0);
            if s1 > 5 || s2 > 5 || s1 < 0 || s2 < 0 {
                warn!("[FORTUNA] Tennis score guard: {} vs {} score={}:{} → reset to 0:0",
                    m.team1, m.team2, s1, s2);
                score1 = Some(0);
                score2 = Some(0);
            }
        }

        // Upsert live state
        {
            let live_entry = LiveMatchState {
                source: source.clone(),
                seen_at: now,
                payload: LiveMatchPayload {
                    sport: sport.clone(),
                    team1: m.team1.clone(),
                    team2: m.team2.clone(),
                    score1,
                    score2,
                    detailed_score: None,
                    status: m.status.clone(),
                    url: None,
                },
            };
            state.live.write().await.insert(key.clone(), live_entry);
            live_count += 1;
        }

        // Extract 1X2 odds (team1 win / team2 win)
        // Fortuna typically sends odds with labels like "1", "X", "2" or team names
        let mut odds_w1: Option<f64> = None;
        let mut odds_w2: Option<f64> = None;

        for o in &m.odds {
            let label = o.label.as_deref().unwrap_or("").trim();
            let market = o.market.as_deref().unwrap_or("").trim().to_lowercase();
            let val = match o.value {
                Some(v) if v > 1.0 => v,
                _ => continue,
            };

            // Match by label: "1" = home, "2" = away, or team name substring
            let t1_lower = m.team1.to_lowercase();
            let t2_lower = m.team2.to_lowercase();
            // Safe char-boundary prefix: take up to 6 chars (not bytes)
            let t1_prefix: String = t1_lower.chars().take(6).collect();
            let t2_prefix: String = t2_lower.chars().take(6).collect();
            let is_home = label == "1" || label.to_lowercase().contains(&t1_prefix);
            let is_away = label == "2" || label.to_lowercase().contains(&t2_prefix);

            // For 1X2 or match_winner markets only
            if market.contains("1x2") || market.contains("winner") || market.contains("vítěz")
               || market.contains("výsledek") || market.is_empty() || market == "unknown" {
                if is_home && odds_w1.is_none() {
                    odds_w1 = Some(val);
                } else if is_away && odds_w2.is_none() {
                    odds_w2 = Some(val);
                } else if odds_w1.is_none() && !is_away {
                    // First unmatched → home
                    odds_w1 = Some(val);
                } else if odds_w2.is_none() && !is_home {
                    // Second unmatched → away
                    odds_w2 = Some(val);
                }
            }
        }

        if let (Some(w1), Some(w2)) = (odds_w1, odds_w2) {
            let odds_key = OddsKey {
                match_key: key.clone(),
                bookmaker: "fortuna".to_string(),
            };
            let odds_state = OddsState {
                source: source.clone(),
                seen_at: now,
                payload: OddsPayload {
                    sport: sport.clone(),
                    bookmaker: "fortuna".to_string(),
                    market: "match_winner".to_string(),
                    team1: m.team1.clone(),
                    team2: m.team2.clone(),
                    odds_team1: w1,
                    odds_team2: w2,
                    liquidity_usd: None,
                    spread_pct: None,
                    url: None,
                    game_id: None,
                    condition_id: None,
                    outcome1_id: None,
                    outcome2_id: None,
                    chain: None,
                },
            };
            state.odds.write().await.insert(odds_key, odds_state);
            odds_count += 1;

            // No alias expansion needed — match_key() already normalizes
            // all esports game labels to a single "esports::" prefix.
        }
    }

    info!("[FORTUNA] ingested {} live + {} odds from {} matches", live_count, odds_count, inbound.matches.len());
    (true, format!("ok: {} live, {} odds from {} matches", live_count, odds_count, inbound.matches.len()))
}

async fn handle_http_connection(mut stream: TcpStream, state: FeedHubState) -> Result<()> {
    // Read headers + potentially partial body
    let mut all_data = Vec::with_capacity(64 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];

    // First read — gets headers + maybe body
    let n = stream.read(&mut tmp).await.context("http read")?;
    if n == 0 {
        return Ok(());
    }
    all_data.extend_from_slice(&tmp[..n]);

    // Parse headers to find Content-Length
    let req_so_far = String::from_utf8_lossy(&all_data);
    let header_end = req_so_far.find("\r\n\r\n");

    let content_length: usize = req_so_far
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    // If we have Content-Length, keep reading until we have all the body
    if let Some(hdr_end) = header_end {
        let body_start = hdr_end + 4;
        let body_received = all_data.len() - body_start;
        let mut remaining = content_length.saturating_sub(body_received);
        while remaining > 0 {
            let n2 = stream.read(&mut tmp).await.context("http read body")?;
            if n2 == 0 { break; }
            all_data.extend_from_slice(&tmp[..n2]);
            remaining = remaining.saturating_sub(n2);
        }
    }

    let req = String::from_utf8_lossy(&all_data).to_string();
    let first_line = req.lines().next().unwrap_or_default().to_string();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    // CORS preflight for Tampermonkey/browser
    let cors_headers = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type";

    if method == "OPTIONS" {
        let resp = format!(
            "HTTP/1.1 204 No Content\r\n{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            cors_headers
        );
        stream.write_all(resp.as_bytes()).await.context("http write")?;
        return Ok(());
    }

    let (status_line, content_type, body) = match (method, path) {
        ("GET", "/health") => ("HTTP/1.1 200 OK", "text/plain; charset=utf-8", "ok".to_string()),
        ("GET", "/state") => {
            let snap = build_state_snapshot(&state).await;
            let json = serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".to_string());
            ("HTTP/1.1 200 OK", "application/json; charset=utf-8", json)
        }
        ("GET", "/opportunities") => {
            let opps = build_opportunities(&state).await;
            let json = serde_json::to_string_pretty(&opps).unwrap_or_else(|_| "{}".to_string());
            ("HTTP/1.1 200 OK", "application/json; charset=utf-8", json)
        }
        ("POST", "/fortuna") => {
            // Extract HTTP body after \r\n\r\n
            let body_start = req.find("\r\n\r\n").map(|p| p + 4).unwrap_or(req.len());
            let http_body = &req[body_start..];
            let (ok, msg) = handle_fortuna_post(&state, http_body).await;
            let status = if ok { "HTTP/1.1 200 OK" } else { "HTTP/1.1 400 Bad Request" };
            let resp_json = serde_json::json!({"ok": ok, "note": msg}).to_string();
            (status, "application/json; charset=utf-8", resp_json)
        }
        _ => (
            "HTTP/1.1 404 Not Found",
            "text/plain; charset=utf-8",
            "not found".to_string(),
        ),
    };

    let resp = format!(
        "{status_line}\r\n{cors_headers}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );
    stream.write_all(resp.as_bytes()).await.context("http write")?;
    Ok(())
}

async fn start_http_server(state: FeedHubState, bind: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(bind).await.context("http bind")?;
    info!("feed-hub http listening on http://{} (GET /health, /state, /opportunities; POST /fortuna)", bind);

    loop {
        let (stream, peer) = listener.accept().await.context("http accept")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_http_connection(stream, state).await {
                debug!("http handler err {}: {}", peer, e);
            }
        });
    }
}

async fn handle_socket(
    peer: SocketAddr,
    stream: TcpStream,
    state: FeedHubState,
    logger: Arc<EventLogger>,
    db_tx: mpsc::Sender<DbMsg>,
) -> Result<()> {
    let ws_stream = accept_async(stream).await.context("WS handshake failed")?;

    {
        let mut c = state.connections.write().await;
        *c += 1;
    }

    info!("WS client connected: {}", peer);

    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    while let Some(msg) = ws_stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!("WS recv err from {}: {}", peer, e);
                break;
            }
        };

        match msg {
            Message::Text(txt) => {
                let txt = txt.to_string();
                let parsed: Result<FeedEnvelope> = serde_json::from_str(&txt)
                    .context("invalid JSON envelope")
                    .map_err(Into::into);

                let (ok, note) = match parsed {
                    Ok(env) => {
                        if env.v != 1 {
                            (false, format!("unsupported version {}", env.v))
                        } else {
                            let env_source = env.source.clone();
                            match env.msg_type {
                                FeedMessageType::LiveMatch => {
                                    let payload: LiveMatchPayload = serde_json::from_value(env.payload)
                                        .context("invalid live_match payload")?;
                                    let seen_at = parse_ts(&env.ts);
                                    let key = match_key(&payload.sport, &payload.team1, &payload.team2);
                                    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

                                    // NORM_TRACE (sampled)
                                    if should_norm_trace() {
                                        debug!(
                                            "NORM_TRACE src={} sport={} raw={}|{} key={}",
                                            env_source, payload.sport, payload.team1, payload.team2, key
                                        );
                                    }

                                    state.live.write().await.insert(
                                        key.clone(),
                                        LiveMatchState {
                                            source: env_source.clone(),
                                            seen_at,
                                            payload: payload.clone(),
                                        },
                                    );

                                    let _ = db_tx.try_send(DbMsg::LiveUpsert(DbLiveRow {
                                        ts: seen_at,
                                        source: env_source,
                                        sport: payload.sport,
                                        team1: payload.team1,
                                        team2: payload.team2,
                                        match_key: key,
                                        payload_json,
                                    }));

                                    (true, "live_match_ingested".to_string())
                                }
                                FeedMessageType::Odds => {
                                    let payload: OddsPayload = serde_json::from_value(env.payload)
                                        .context("invalid odds payload")?;
                                    let seen_at = parse_ts(&env.ts);
                                    let key = match_key(&payload.sport, &payload.team1, &payload.team2);
                                    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

                                    // NORM_TRACE (sampled)
                                    if should_norm_trace() {
                                        debug!(
                                            "NORM_TRACE src={} sport={} bk={} raw={}|{} key={}",
                                            env_source, payload.sport, payload.bookmaker,
                                            payload.team1, payload.team2, key
                                        );
                                    }

                                    let odds_key = OddsKey {
                                        match_key: key.clone(),
                                        bookmaker: payload.bookmaker.clone(),
                                    };
                                    {
                                        let mut odds_w = state.odds.write().await;
                                        odds_w.insert(
                                            odds_key,
                                            OddsState {
                                                source: env_source.clone(),
                                                seen_at,
                                                payload: payload.clone(),
                                            },
                                        );
                                        // No alias expansion — match_key() already
                                        // normalizes all esports labels to "esports::".
                                    }

                                    let _ = db_tx.try_send(DbMsg::OddsUpsert(DbOddsRow {
                                        ts: seen_at,
                                        source: env_source.clone(),
                                        sport: payload.sport.clone(),
                                        bookmaker: payload.bookmaker.clone(),
                                        market: payload.market.clone(),
                                        team1: payload.team1.clone(),
                                        team2: payload.team2.clone(),
                                        match_key: key.clone(),
                                        odds_team1: payload.odds_team1,
                                        odds_team2: payload.odds_team2,
                                        liquidity_usd: payload.liquidity_usd,
                                        spread_pct: payload.spread_pct,
                                        payload_json,
                                    }));

                                    let (pass, why) = gate_odds(&payload, seen_at);
                                    if pass {
                                        if let Some(live) = state.live.read().await.get(&key).cloned() {
                                            let fusion = LiveFusionReadyEvent {
                                                ts: Utc::now().to_rfc3339(),
                                                event: "LIVE_FUSION_READY",
                                                sport: payload.sport.clone(),
                                                match_key: key,
                                                live_source: live.source,
                                                odds_source: env_source.clone(),
                                                bookmaker: payload.bookmaker.clone(),
                                                market: payload.market.clone(),
                                                liquidity_usd: payload.liquidity_usd,
                                                spread_pct: payload.spread_pct,
                                            };
                                            let _ = logger.log(&fusion);

                                            let _ = db_tx.try_send(DbMsg::Fusion(DbFusionRow {
                                                ts: Utc::now(),
                                                sport: fusion.sport.clone(),
                                                match_key: fusion.match_key.clone(),
                                                live_source: fusion.live_source.clone(),
                                                odds_source: fusion.odds_source.clone(),
                                                bookmaker: fusion.bookmaker.clone(),
                                                market: fusion.market.clone(),
                                                liquidity_usd: fusion.liquidity_usd,
                                                spread_pct: fusion.spread_pct,
                                            }));
                                        }
                                        (true, format!("odds_ingested_gated:{}", why))
                                    } else {
                                        (true, format!("odds_ingested_rejected:{}", why))
                                    }
                                }
                                FeedMessageType::Heartbeat => (true, "heartbeat".to_string()),
                            }
                        }
                    }
                    Err(e) => (false, format!("parse_error:{}", e)),
                };

                let ingest = FeedIngestEvent {
                    ts: Utc::now().to_rfc3339(),
                    event: "FEED_INGEST",
                    source: "ws".to_string(),
                    msg_type: "text".to_string(),
                    ok,
                    note: note.clone(),
                };
                let _ = logger.log(&ingest);

                let _ = db_tx.try_send(DbMsg::Ingest(DbIngestRow {
                    ts: Utc::now(),
                    source: "ws".to_string(),
                    msg_type: "text".to_string(),
                    ok,
                    note: note.clone(),
                    raw_json: Some(txt.clone()),
                }));

                let ack = serde_json::json!({"ok": ok, "note": note});
                let _ = ws_sink.send(Message::Text(ack.to_string().into())).await;
            }
            Message::Ping(payload) => {
                let _ = ws_sink.send(Message::Pong(payload)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    info!("WS client disconnected: {}", peer);
    {
        let mut c = state.connections.write().await;
        *c = c.saturating_sub(1);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    load_feature_flags();

    let bind = std::env::var("FEED_HUB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse().context("Invalid FEED_HUB_BIND")?;

    let listener = TcpListener::bind(addr).await.context("bind failed")?;
    info!("feed-hub listening on ws://{}/feed", addr);

    let state = FeedHubState::new();
    let logger = Arc::new(EventLogger::new("logs"));

    let db_path = std::env::var("FEED_DB_PATH").unwrap_or_else(|_| "data/feed.db".to_string());
    info!("feed-hub DB: {}", db_path);
    let db_tx = spawn_db_writer(DbConfig { path: db_path });

    // Minimal HTTP read-only state endpoint
    {
        let http_bind = std::env::var("FEED_HTTP_BIND").unwrap_or_else(|_| "127.0.0.1:8081".to_string());
        let http_addr: SocketAddr = http_bind.parse().context("Invalid FEED_HTTP_BIND")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = start_http_server(state, http_addr).await {
                warn!("http server stopped: {e}");
            }
        });
    }

    // Staleness cleanup — remove entries older than 120s
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let cutoff = Utc::now() - chrono::Duration::seconds(120);
                {
                    let mut live = state.live.write().await;
                    let before = live.len();
                    live.retain(|_, v| v.seen_at > cutoff);
                    let removed = before - live.len();
                    if removed > 0 {
                        info!("staleness cleanup: removed {} stale live entries", removed);
                    }
                }
                {
                    let mut odds = state.odds.write().await;
                    let before = odds.len();
                    odds.retain(|_, v| v.seen_at > cutoff);
                    let removed = before - odds.len();
                    if removed > 0 {
                        info!("staleness cleanup: removed {} stale odds entries", removed);
                    }
                }
            }
        });
    }

    // Heartbeat summary
    {
        let state = state.clone();
        let logger = Arc::clone(&logger);
        let db_tx = db_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;

                let connections = *state.connections.read().await;
                let live_items = state.live.read().await.len();
                let odds_items = state.odds.read().await.len();

                // “fused_ready” = kolik odds klíčů má zároveň live
                let (fused_ready, fusion_misses) = {
                    let live = state.live.read().await;
                    let odds = state.odds.read().await;
                    let mut match_keys = std::collections::HashSet::new();
                    for ok in odds.keys() {
                        match_keys.insert(ok.match_key.clone());
                    }
                    let live_keys_vec: Vec<&str> = live.keys().map(|s| s.as_str()).collect();
                    let ea: &[&str] = &[
                        "cs2", "dota-2", "league-of-legends", "valorant",
                        "basketball", "football", "mma", "starcraft",
                    ];
                    let mut fused = 0usize;
                    let mut misses: Vec<String> = Vec::new();
                    for k in &match_keys {
                        let direct = live.contains_key(k.as_str());
                        let alt = !direct && {
                            let p: Vec<&str> = k.splitn(2, "::").collect();
                            if p.len() == 2 {
                                let tail = p[1];
                                ea.iter().any(|a2| {
                                    if *a2 == p[0] {
                                        live.contains_key(&format!("esports::{}", tail))
                                    } else { false }
                                })
                            } else { false }
                        };
                        let fz = !direct && !alt && fuzzy_find_key(k.as_str(), &live_keys_vec).is_some();
                        let tk = !direct && !alt && !fz && token_subset_pair_match(k.as_str(), &live_keys_vec).is_some();
                        if direct || alt || fz || tk {
                            fused += 1;
                        } else {
                            let sp = k.split("::").next().unwrap_or("?");
                            let mut rt1 = String::new();
                            let mut rt2 = String::new();
                            let mut sr = String::new();
                            let mut ot = String::new();
                            for (ok2, ov) in odds.iter() {
                                if ok2.match_key == *k {
                                    rt1 = ov.payload.team1.clone();
                                    rt2 = ov.payload.team2.clone();
                                    sr = format!("{}:{}", ov.source, ov.payload.bookmaker);
                                    ot = ov.seen_at.format("%H:%M:%S").to_string();
                                    break;
                                }
                            }
                            let t3: Vec<&str> = live_keys_vec.iter()
                                .filter(|lk| lk.starts_with(&format!("{}::", sp)))
                                .take(3).copied().collect();
                            misses.push(format!(
                                "key={} sport={} src={} ots={} raw={}|{} top3=[{}]",
                                k, sp, sr, ot, rt1, rt2, t3.join(", ")
                            ));
                        }
                    }
                    (fused, misses)
                };
                let miss_n = fusion_misses.len();
                for (i, line) in fusion_misses.iter().enumerate() {
                    if i >= 10 { break; }
                    warn!("FUSION_MISS {}", line);
                }
                if miss_n > 10 {
                    warn!("FUSION_MISS ...and {} more", miss_n - 10);
                }

                let hb = FeedHeartbeatEvent {
                    ts: Utc::now().to_rfc3339(),
                    event: "FEED_HUB_HEARTBEAT",
                    connections,
                    live_items,
                    odds_items,
                    fused_ready,
                };
                let _ = logger.log(&hb);
                let _ = db_tx.try_send(DbMsg::Heartbeat(DbHeartbeatRow {
                    ts: Utc::now(),
                    connections: connections as i64,
                    live_items: live_items as i64,
                    odds_items: odds_items as i64,
                    fused_ready: fused_ready as i64,
                }));
                info!(
                    "HB: conns={} live={} odds={} fused={} miss={}",
                    connections, live_items, odds_items, fused_ready, miss_n
                );
            }
        });
    }

    // NOTE: path routing se řeší u higher-level serverů; tady přijímáme WS na jakémkoliv path.
    // Azuro GraphQL poller — periodicky stahuje CS2 odds z on-chain subgraph
    {
        let state = state.clone();
        let db_tx = db_tx.clone();
        tokio::spawn(async move {
            azuro_poller::run_azuro_poller(state, db_tx).await;
        });
    }

    while let Ok((stream, peer)) = listener.accept().await {
        let state = state.clone();
        let logger = Arc::clone(&logger);
        let db_tx = db_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_socket(peer, stream, state, logger, db_tx).await {
                debug!("socket handler err {}: {}", peer, e);
            }
        });
    }

    Ok(())
}
