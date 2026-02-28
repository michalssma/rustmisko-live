/**
 * overtime_paper.mjs — Paper Trading Logger for Overtime Markets
 *
 * ZERO RISK — reads feed-hub /opportunities + Overtime live-markets API,
 * cross-references matches, logs "WOULD BET" entries with paper P&L tracking.
 *
 * Run: node overtime_paper.mjs
 * Logs to: ../data/overtime_paper.jsonl  (JSONL, one JSON per line)
 *          ../data/overtime_paper_pnl.json (cumulative paper P&L)
 *
 * After 2+ weeks of paper data → decide whether to build real executor.
 */

import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const DATA_DIR = path.resolve(__dirname, "../data");

// ─── Config ───────────────────────────────────────────────────────────────────
const FEED_HUB_URL      = "http://127.0.0.1:8081";
const OVERTIME_BASE_URL = "https://api.overtime.io/overtime-v2/networks/8453";
const OVERTIME_API_KEY  = process.env.OVERTIME_API_KEY || "";  // Set via: set OVERTIME_API_KEY=yourkey
const POLL_INTERVAL_MS  = 30_000;   // 30 seconds
const PAPER_STAKE_USD   = 3.0;      // Mirror our real stake
const MIN_EDGE_PCT      = 5.0;      // Only log edges above this threshold
const LOG_FILE          = path.join(DATA_DIR, "overtime_paper.jsonl");
const PNL_FILE          = path.join(DATA_DIR, "overtime_paper_pnl.json");
const RAW_SAMPLE_FILE   = path.join(DATA_DIR, "overtime_raw_sample.json");

// Overtime League IDs (from overtime-live-trading-utils LeagueEnum)
// Reference: https://github.com/thales-markets/overtime-live-trading-utils
const ESPORTS_LEAGUE_IDS = new Set([
  9001, 9002, 9003, 9004, 9005, // CS2 / CSGO leagues
  9006, 9007, 9008,             // League of Legends
  9009, 9010, 9011,             // Dota 2
  9012, 9013,                   // Valorant
  9014, 9015,                   // Rocket League
  // Wide net — will narrow after seeing real data
]);

// Market type IDs (from MarketType enum in overtime-live-trading-utils)
const MARKET_WINNER    = 0;
const MARKET_MAP_WIN   = 10;  // MAP_WINNER
const MARKET_TOTAL     = 2;

// ─── Dedup: track bets already logged this session ────────────────────────────
// Key: `${overtimeGameId}_${valueSide}` → logged timestamp
const loggedBets = new Map();

// ─── Utility ──────────────────────────────────────────────────────────────────

/** Normalize team name for fuzzy matching */
function normTeam(name) {
  return (name || "")
    .toLowerCase()
    .replace(/^team\s+/, "")      // Remove leading "team "
    .replace(/\s+/g, "")          // Remove all spaces
    .replace(/[^a-z0-9]/g, "")    // Keep only alphanumeric
    .substring(0, 15);            // Limit length
}

/** Check if two team names are probably the same team */
function teamMatch(nameA, nameB) {
  const a = normTeam(nameA);
  const b = normTeam(nameB);
  if (!a || !b) return false;
  if (a === b) return true;
  // One contains the other (handles "NatusVincere" vs "natus")
  if (a.includes(b) || b.includes(a)) return true;
  // Prefix match of at least 5 chars
  const prefix = 5;
  if (a.length >= prefix && b.length >= prefix && a.substring(0, prefix) === b.substring(0, prefix)) return true;
  return false;
}

/** Parse match_key like "esports::spirit_vs_natus" → ["spirit", "natus"] */
function parseMatchKey(matchKey) {
  const tail = matchKey.includes("::") ? matchKey.split("::")[1] : matchKey;
  const parts = tail.split("_vs_");
  if (parts.length !== 2) return null;
  return parts; // ["spirit", "natus"]
}

/** Check if a match_key is esports/cs2 relevant */
function isEsports(matchKey) {
  return matchKey.startsWith("esports::") || matchKey.startsWith("cs2::") ||
         matchKey.startsWith("dota-2::") || matchKey.startsWith("league-of-legends::") ||
         matchKey.startsWith("valorant::") || matchKey.startsWith("rocket-league::");
}

/** Append one line to JSONL log file */
function appendLog(record) {
  const line = JSON.stringify(record) + "\n";
  fs.appendFileSync(LOG_FILE, line, "utf8");
}

/** Load or init paper P&L tracker */
function loadPnl() {
  if (fs.existsSync(PNL_FILE)) {
    try { return JSON.parse(fs.readFileSync(PNL_FILE, "utf8")); }
    catch {}
  }
  return { total_bets: 0, total_staked: 0, total_return: 0, net_pnl: 0, bets: [] };
}

function savePnl(pnl) {
  fs.writeFileSync(PNL_FILE, JSON.stringify(pnl, null, 2), "utf8");
}

// ─── API Fetchers ─────────────────────────────────────────────────────────────

async function fetchOpportunities() {
  try {
    const res = await fetch(`${FEED_HUB_URL}/opportunities`, { signal: AbortSignal.timeout(5000) });
    if (!res.ok) return null;
    return await res.json();
  } catch (e) {
    console.error(`[feed-hub] /opportunities error: ${e.message}`);
    return null;
  }
}

async function fetchOvertimeLiveMarkets() {
  if (!OVERTIME_API_KEY) {
    // No API key — silently skip (will log EDGE_NO_OVERTIME)
    return null;
  }
  try {
    const res = await fetch(`${OVERTIME_BASE_URL}/live-markets`, {
      headers: {
        "Accept": "application/json",
        "x-api-key": OVERTIME_API_KEY,
        "Origin": "https://overtimemarkets.xyz",
        "Referer": "https://overtimemarkets.xyz/",
        "User-Agent": "Mozilla/5.0 (compatible; OvertimePaperBot/1.0)",
      },
      signal: AbortSignal.timeout(10000),
    });
    if (!res.ok) {
      console.error(`[overtime] live-markets HTTP ${res.status}`);
      return null;
    }
    const data = await res.json();
    // Save first successful raw sample for inspection
    if (!fs.existsSync(RAW_SAMPLE_FILE)) {
      fs.writeFileSync(RAW_SAMPLE_FILE, JSON.stringify(data, null, 2), "utf8");
      console.log(`[overtime] Saved raw sample → ${RAW_SAMPLE_FILE}`);
    }
    return data;
  } catch (e) {
    // Expected if API is unreachable — log quietly
    if (e.message.includes("fetch failed") || e.message.includes("ENOTFOUND")) {
      console.warn(`[overtime] API unreachable (${e.cause?.code || e.message}) — will retry`);
    } else {
      console.error(`[overtime] live-markets error: ${e.message}`);
    }
    return null;
  }
}

// ─── Market Extraction from Overtime response ─────────────────────────────────

/**
 * Overtime API returns either:
 *   A) Array of markets directly
 *   B) Object { markets: [...] }
 *   C) Object keyed by leagueId { "9001": [...], "9002": [...] }
 * Handle all cases.
 */
function extractMarkets(rawData) {
  if (!rawData) return [];
  if (Array.isArray(rawData)) return rawData;
  if (Array.isArray(rawData.markets)) return rawData.markets;
  // Keyed by league ID
  const all = [];
  for (const val of Object.values(rawData)) {
    if (Array.isArray(val)) all.push(...val);
  }
  return all;
}

/**
 * Get team names from an Overtime market object.
 * Tries common field name patterns.
 */
function getMarketTeams(market) {
  // Try various field patterns
  if (market.homeTeam && market.awayTeam) {
    return [market.homeTeam, market.awayTeam];
  }
  if (market.teamNames && Array.isArray(market.teamNames) && market.teamNames.length >= 2) {
    return [market.teamNames[0], market.teamNames[1]];
  }
  if (market.team1 && market.team2) {
    return [market.team1, market.team2];
  }
  if (market.participants && Array.isArray(market.participants) && market.participants.length >= 2) {
    return [market.participants[0].name || market.participants[0], 
            market.participants[1].name || market.participants[1]];
  }
  return [null, null];
}

/**
 * Get odds from an Overtime market.
 * Returns [oddsTeam1, oddsTeam2] or null.
 */
function getMarketOdds(market) {
  // Standard fields
  if (market.odds) {
    const o = market.odds;
    if (Array.isArray(o) && o.length >= 2) return [o[0], o[1]];
    if (o.home !== undefined && o.away !== undefined) return [o.home, o.away];
  }
  if (market.homeOdds !== undefined && market.awayOdds !== undefined) {
    return [market.homeOdds, market.awayOdds];
  }
  if (market.odds0 !== undefined && market.odds1 !== undefined) {
    return [market.odds0, market.odds1];
  }
  if (market.buyInOnePositionCost !== undefined) {
    // AMM format: position cost in USDC = 1/odds
    const cost0 = market.buyInOnePositionCost[0] || market.buyInOnePositionCost?.home;
    const cost1 = market.buyInOnePositionCost[1] || market.buyInOnePositionCost?.away;
    if (cost0 && cost1) return [1.0 / cost0, 1.0 / cost1];
  }
  return null;
}

/** Get game ID from market */
function getGameId(market) {
  return market.gameId || market.id || market.marketId || market.address || null;
}

/** Get league ID from market */
function getLeagueId(market) {
  return market.leagueId || market.sportId || market.league || null;
}

/** Get market type ID */
function getTypeId(market) {
  return market.typeId ?? market.marketType ?? market.type ?? MARKET_WINNER;
}

// ─── Match opportunities with Overtime markets ────────────────────────────────

function findOvertimeMatch(opp, overtimeMarkets) {
  const keys = parseMatchKey(opp.match_key);
  if (!keys) return null;
  const [rawT1, rawT2] = keys;

  // Also get full team names from opportunity
  const oppTeam1 = opp.team1;
  const oppTeam2 = opp.team2;

  for (const market of overtimeMarkets) {
    const [mTeam1, mTeam2] = getMarketTeams(market);
    if (!mTeam1 || !mTeam2) continue;

    // Try matching our team1/team2 against market teams
    const t1_vs_m1 = teamMatch(oppTeam1, mTeam1) || teamMatch(rawT1, mTeam1);
    const t2_vs_m2 = teamMatch(oppTeam2, mTeam2) || teamMatch(rawT2, mTeam2);
    const t1_vs_m2 = teamMatch(oppTeam1, mTeam2) || teamMatch(rawT1, mTeam2);
    const t2_vs_m1 = teamMatch(oppTeam2, mTeam1) || teamMatch(rawT2, mTeam1);

    if ((t1_vs_m1 && t2_vs_m2) || (t1_vs_m2 && t2_vs_m1)) {
      return {
        market,
        reversed: t1_vs_m2 && t2_vs_m1, // Our team1 = their team2
      };
    }
  }
  return null;
}

// ─── Main polling loop ────────────────────────────────────────────────────────

let pollCount = 0;
let overtimeConnected = false;

async function poll() {
  pollCount++;
  const ts = new Date().toISOString();
  
  // 1. Fetch opportunities from feed-hub
  const oppData = await fetchOpportunities();
  if (!oppData) {
    if (pollCount % 10 === 1) console.log(`[${ts}] feed-hub unavailable`);
    return;
  }

  const esportsOpps = (oppData.opportunities || []).filter(o =>
    isEsports(o.match_key) &&
    o.edge_pct >= MIN_EDGE_PCT &&
    o.odds >= 1.5 &&        // Skip near-certain outcomes with tiny odds
    (o.value_side === 1 || o.value_side === 2) // Skip arb (value_side=0 = cross-book arb, not bettable on Overtime)
  );

  if (esportsOpps.length === 0) {
    if (pollCount % 4 === 1) {
      console.log(`[${ts}] No esports edges side=1/2 (total opps: ${oppData.opportunities?.length || 0})`);
    }
    return;
  }

  console.log(`[${ts}] Esports SCORE edges (side 1/2): ${esportsOpps.length} (total opps: ${oppData.opportunities?.length})`);
  for (const o of esportsOpps) {
    console.log(`  → ${o.match_key} | edge=${o.edge_pct.toFixed(1)}% | side=${o.value_side} | odds=${o.odds}`);
  }

  // 2. Fetch Overtime live markets
  const rawMarkets = await fetchOvertimeLiveMarkets();
  const allMarkets = extractMarkets(rawMarkets);

  if (allMarkets.length === 0) {
    if (!overtimeConnected) {
      console.warn(`[${ts}] Overtime API unreachable — logging edges only (no match possible)`);
    }
    // Log edges with NO_OVERTIME_MATCH status for visibility
    for (const opp of esportsOpps) {
      const dedupKey = `no_overtime_${opp.match_key}_${opp.value_side}`;
      if (!loggedBets.has(dedupKey)) {
        const entry = {
          ts,
          status: "EDGE_NO_OVERTIME",
          match_key: opp.match_key,
          our_edge_pct: opp.edge_pct,
          value_side: opp.value_side,
          our_team: opp.value_side === 1 ? opp.team1 : opp.team2,
          score: opp.score,
          azuro_odds: opp.odds,
          bookmaker: opp.bookmaker,
          signal: opp.signal,
          note: "Edge detected but Overtime API unreachable",
        };
        appendLog(entry);
        loggedBets.set(dedupKey, Date.now());
        console.log(`  [LOG] EDGE_NO_OVERTIME: ${opp.match_key} edge=${opp.edge_pct.toFixed(1)}%`);
      }
    }
    return;
  }

  overtimeConnected = true;
  console.log(`[${ts}] Overtime markets: ${allMarkets.length}`);

  // 3. For each esports edge, try to find matching Overtime market
  const pnl = loadPnl();

  for (const opp of esportsOpps) {
    const match = findOvertimeMatch(opp, allMarkets);

    if (!match) {
      // Log as "edge exists but no Overtime market found"
      const dedupKey = `miss_${opp.match_key}_${opp.value_side}_${opp.score}`;
      if (!loggedBets.has(dedupKey)) {
        const entry = {
          ts,
          status: "EDGE_NO_MARKET",
          match_key: opp.match_key,
          our_edge_pct: opp.edge_pct,
          value_side: opp.value_side,
          our_team: opp.value_side === 1 ? opp.team1 : opp.team2,
          score: opp.score,
          azuro_odds: opp.odds,
          note: "Edge detected, no matching Overtime market",
        };
        appendLog(entry);
        loggedBets.set(dedupKey, Date.now());
        console.log(`  [LOG] EDGE_NO_MARKET: ${opp.match_key}`);
      }
      continue;
    }

    const { market, reversed } = match;
    const gameId = getGameId(market);
    const odds = getMarketOdds(market);
    const typeId = getTypeId(market);
    const leagueId = getLeagueId(market);
    const [mTeam1, mTeam2] = getMarketTeams(market);

    // Which side should we bet on Overtime?
    // value_side=1 means our team1 has value
    // If match is reversed (our team1 = their team2), flip side
    let overtimeSide = opp.value_side;
    if (reversed) overtimeSide = opp.value_side === 1 ? 2 : 1;

    const overtimeOdds = odds ? odds[overtimeSide - 1] : null;
    const betTeam = overtimeSide === 1 ? mTeam1 : mTeam2;

    const dedupKey = `${gameId}_${overtimeSide}_${opp.score}`;
    if (loggedBets.has(dedupKey)) continue;

    // Calculate if Overtime odds offer positive EV given our fair estimate
    const fairPct = opp.estimated_fair_pct || (opp.edge_pct + (overtimeOdds ? 100 / overtimeOdds : 50));
    const overtimeImplied = overtimeOdds ? (100 / overtimeOdds) : null;
    const overtimeEdge = overtimeImplied ? (fairPct - overtimeImplied) : null;

    const entry = {
      ts,
      status: "WOULD_BET",
      match_key: opp.match_key,
      game_id: gameId,
      league_id: leagueId,
      market_type_id: typeId,
      market_type_name: typeId === MARKET_MAP_WIN ? "MAP_WINNER" : typeId === MARKET_TOTAL ? "TOTAL" : "WINNER",
      our_team: opp.value_side === 1 ? opp.team1 : opp.team2,
      bet_team_overtime: betTeam,
      score: opp.score,
      // Our analysis
      our_edge_pct: opp.edge_pct,
      our_fair_pct: opp.estimated_fair_pct,
      azuro_odds: opp.odds,
      signal: opp.signal,
      // Overtime
      overtime_odds: overtimeOdds,
      overtime_implied_pct: overtimeImplied ? parseFloat(overtimeImplied.toFixed(2)) : null,
      overtime_edge_pct: overtimeEdge ? parseFloat(overtimeEdge.toFixed(2)) : null,
      // Paper bet
      paper_stake_usd: PAPER_STAKE_USD,
      paper_potential_win: overtimeOdds ? parseFloat((PAPER_STAKE_USD * overtimeOdds).toFixed(2)) : null,
      // Settlement
      settled: false,
      result: null,
      payout: null,
    };

    appendLog(entry);
    loggedBets.set(dedupKey, Date.now());

    // Track in P&L
    pnl.total_bets++;
    pnl.total_staked += PAPER_STAKE_USD;
    pnl.bets.push({
      ts,
      match_key: opp.match_key,
      game_id: gameId,
      bet_team: betTeam,
      overtime_odds: overtimeOdds,
      stake: PAPER_STAKE_USD,
      settled: false,
      result: null,
      payout: null,
    });
    savePnl(pnl);

    const edgeStr = overtimeEdge !== null ? ` OT_edge=${overtimeEdge.toFixed(1)}%` : "";
    console.log(`  ✅ WOULD_BET: $${PAPER_STAKE_USD} on ${betTeam} @ ${overtimeOdds || "??"} (${entry.market_type_name})${edgeStr}`);
  }

  // Clean up stale dedup entries (older than 4h)
  if (pollCount % 120 === 0) {
    const cutoff = Date.now() - 4 * 60 * 60 * 1000;
    for (const [k, v] of loggedBets.entries()) {
      if (v < cutoff) loggedBets.delete(k);
    }
  }
}

// ─── Startup ──────────────────────────────────────────────────────────────────

console.log("═══════════════════════════════════════════════════════");
console.log(" Overtime Paper Trading Logger");
console.log(` Stake: $${PAPER_STAKE_USD} | Min edge: ${MIN_EDGE_PCT}%`);
console.log(` Log: ${LOG_FILE}`);
console.log(` P&L: ${PNL_FILE}`);
console.log(` Polling every ${POLL_INTERVAL_MS / 1000}s`);
console.log(` Overtime API: ${OVERTIME_BASE_URL}`);
if (OVERTIME_API_KEY) {
  console.log(` API Key: SET ✅ (${OVERTIME_API_KEY.substring(0,6)}...)`);
} else {
  console.log(" API Key: ⚠️  NOT SET — logging EDGE_NO_OVERTIME only");
  console.log("   → Pro aktivaci: set OVERTIME_API_KEY=<key>");
  console.log("   → Klíč získáš: Discord Overtime dev channel / api.overtime.io");
}
console.log("═══════════════════════════════════════════════════════");

// Create data dir if missing
if (!fs.existsSync(DATA_DIR)) fs.mkdirSync(DATA_DIR, { recursive: true });

// Initial poll immediately, then every 30s
await poll();
setInterval(poll, POLL_INTERVAL_MS);
