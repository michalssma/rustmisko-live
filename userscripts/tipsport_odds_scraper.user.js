// ==UserScript==
// @name         Tipsport → Feed Hub Odds Scraper
// @namespace    rustmisko
// @version      2.1
// @description  Scrapes live odds from Tipsport.cz and sends to Feed Hub — generic DOM v2.1 with text cleanup
// @author       RustMisko
// @match        https://www.tipsport.cz/*
// @match        https://m.tipsport.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 10000; // 10s — odds don't change as fast as scores
  const RECONNECT_MS = 5000;
  const HEARTBEAT_MS = 20000;
  const SOURCE_NAME = "tipsport";
  const DEBUG = true;

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let scanTimer = null;
  let hbTimer = null;

  const PREFIX = "[Tipsport→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // SPORT DETECTION — Tipsport specific
  // ====================================================================

  const TIPSPORT_SPORT_MAP = {
    "tenis":        "tennis",
    "fotbal":       "football",
    "basketbal":    "basketball",
    "hokej":        "hockey",
    "esport":       "esports",
    "e-sporty":     "esports",
    "cs2":          "cs2",
    "cs:go":        "cs2",
    "counter-strike": "cs2",
    "dota":         "dota-2",
    "dota 2":       "dota-2",
    "league of legends": "league-of-legends",
    "lol":          "league-of-legends",
    "mma":          "mma",
    "box":          "mma",
    "baseball":     "baseball",
    "házená":       "handball",
    "volejbal":     "volleyball",
  };

  function detectTipsportSport() {
    // 1. URL-based detection
    const url = window.location.href.toLowerCase();
    for (const [key, sport] of Object.entries(TIPSPORT_SPORT_MAP)) {
      if (url.includes(key.replace(/\s+/g, "-"))) return sport;
    }

    // 2. Breadcrumb / page title
    const breadcrumb = document.querySelector(
      '.breadcrumb, .o-page__title, .sport-name, [class*="breadcrumb"], [class*="sportName"]'
    );
    if (breadcrumb) {
      const text = breadcrumb.textContent.toLowerCase();
      for (const [key, sport] of Object.entries(TIPSPORT_SPORT_MAP)) {
        if (text.includes(key)) return sport;
      }
    }

    // 3. Meta / title
    const pageTitle = document.title.toLowerCase();
    for (const [key, sport] of Object.entries(TIPSPORT_SPORT_MAP)) {
      if (pageTitle.includes(key)) return sport;
    }

    return null;
  }

  // ====================================================================
  // UI PANEL
  // ====================================================================
  function createPanel() {
    const panel = document.createElement("div");
    panel.id = "tipsport-feedhub-panel";
    panel.style.cssText = `
      position: fixed; bottom: 10px; right: 10px; z-index: 999999;
      background: #1a2e1a; color: #0f0; font-family: 'Consolas', monospace;
      font-size: 11px; padding: 10px 14px; border-radius: 8px;
      border: 1px solid #0f0; min-width: 260px; max-width: 380px;
      opacity: 0.92; box-shadow: 0 0 20px rgba(0,255,0,0.15);
      max-height: 300px; overflow-y: auto;
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom: 4px;">💰 Tipsport → Feed Hub</div>
      <div id="tp-status" style="color: #fa0;">⏳ Connecting...</div>
      <div id="tp-sport" style="margin-top: 2px; color: #aaa;">Sport: detecting...</div>
      <div id="tp-matches" style="margin-top: 4px; color: #aaa;">Scanning...</div>
      <div id="tp-sent" style="margin-top: 2px; color: #888;">Sent: 0</div>
    `;
    document.body.appendChild(panel);
  }

  function updatePanel(status, sport, matches, sent) {
    const el = (id) => document.getElementById(id);
    if (el("tp-status")) el("tp-status").textContent = status;
    if (el("tp-sport")) el("tp-sport").textContent = `Sport: ${sport || "?"}`;
    if (el("tp-matches")) el("tp-matches").textContent = matches;
    if (el("tp-sent")) el("tp-sent").textContent = `Sent: ${sent}`;
  }

  // ====================================================================
  // WEBSOCKET
  // ====================================================================
  function connectWS() {
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;

    log("Connecting to", WS_URL);
    ws = new WebSocket(WS_URL);

    ws.onopen = () => {
      connected = true;
      log("✅ Connected to Feed Hub");
      updatePanel("✅ Connected", detectTipsportSport(), "Scanning...", sentCount);
      startScanning();
      startHeartbeat();
    };

    ws.onclose = (e) => {
      connected = false;
      log("❌ Disconnected:", e.code);
      updatePanel("❌ Disconnected", detectTipsportSport(), "Reconnecting...", sentCount);
      stopScanning();
      stopHeartbeat();
      setTimeout(connectWS, RECONNECT_MS);
    };

    ws.onerror = () => {};
    ws.onmessage = (e) => { dbg("Server:", e.data); };
  }

  function sendJSON(obj) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify(obj));
    sentCount++;
    return true;
  }

  function startHeartbeat() {
    stopHeartbeat();
    hbTimer = setInterval(() => {
      if (connected) sendJSON({ v: 1, type: "heartbeat", source: SOURCE_NAME, ts: new Date().toISOString() });
    }, HEARTBEAT_MS);
  }
  function stopHeartbeat() { if (hbTimer) { clearInterval(hbTimer); hbTimer = null; } }

  // ====================================================================
  // TIPSPORT SCRAPING — Generic DOM approach v2.0
  // ====================================================================
  /**
   * Instead of relying on specific CSS classes (which change with Tipsport updates),
   * we use a generic approach:
   *   1. Find all <a> links with "Team1 - Team2" text pattern
   *   2. Walk up the DOM to find the match container with odds values
   *   3. Extract odds from leaf elements matching decimal number pattern
   * This works regardless of Tipsport's CSS class naming.
   */

  // ====================================================================
  // ESPORT SUB-DETECTION — detect CS2 vs Dota2 vs LoL vs eFOOTBALL from DOM
  // Tipsport groups esports under category headers on the live page.
  // We walk prevSibling + ancestor to find the nearest header text.
  // Returns: 'cs2' | 'dota-2' | 'league-of-legends' | 'valorant' | 'skip' | null
  // 'skip' = e-football, e-basketball → EXCLUDE from feed
  // ====================================================================
  const TP_CS2_KW   = ['counter-strike', 'cs2', 'cs:go', 'csgo', 'iem', 'blast', 'esl pro', 'faceit'];
  const TP_DOTA_KW  = ['dota 2', 'dota2', 'dota-2'];
  const TP_LOL_KW   = ['league of legends', ' lcs ', ' lec ', ' lck ', ' lpl '];
  const TP_VAL_KW   = ['valorant', 'vct '];
  const TP_SKIP_KW  = ['efootball', 'e-football', 'ea sports', 'fifa', 'nba 2k', 'nba2k',
                       'e-basketbal', 'ebasketbal', 'madden', 'e-fotbal', 'efotbal',
                       'echampions', 'e-liga', 'eliga', 'nba 2k25'];

  // Team names that indicate real football/basketball clubs in e-sports
  const TP_REAL_TEAMS = [
    'liverpool', 'realmadrid', 'barcelona', 'manchestercity', 'manchesterunited',
    'chelsea', 'arsenal', 'juventus', 'bayernmunchen', 'dortmund', 'psg', 'atletico',
    'porto', 'benfica', 'ajax', 'milan', 'roma', 'napoli', 'rangers', 'celtic',
    'racing', 'riverplate', 'bocajuniors', 'flamengo', 'corinthians', 'deportivo',
    'lakers', 'celtics', 'warriors', 'bulls', 'heat', 'knicks', 'nets', 'clippers',
    'houstonrockets', 'clevelandcavaliers', 'sacramentokings', 'minnesotatimberwolves',
    'denvernuggets', 'phoenixsuns', 'milwaukeebucks', 'goldenstatewarriors',
  ];

  function detectEsportFromLink(linkEl) {
    // Walk up DOM + prev siblings to find category header
    let node = linkEl;
    for (let depth = 0; depth < 10; depth++) {
      if (!node || node === document.body) break;

      let sib = node.previousElementSibling;
      let sibCount = 0;
      while (sib && sibCount < 6) {
        const cls = (sib.className || '').toLowerCase();
        const txt = (' ' + sib.textContent.toLowerCase() + ' ');
        const isHeader = cls.includes('header') || cls.includes('category') || cls.includes('title') ||
                         cls.includes('league') || cls.includes('sport') || cls.includes('section') ||
                         cls.includes('nazev') || cls.includes('skupina') || cls.includes('titul');
        // Also check elements that contain ONLY short text (likely a label/header, not a match row)
        const isShortLabel = sib.textContent.trim().length < 80 && !sib.textContent.includes(' - ');

        if (isHeader || isShortLabel) {
          for (const kw of TP_SKIP_KW) if (txt.includes(kw)) { dbg(`TP skip esport (${kw})`); return 'skip'; }
          for (const kw of TP_CS2_KW)  if (txt.includes(kw)) return 'cs2';
          for (const kw of TP_DOTA_KW) if (txt.includes(kw)) return 'dota-2';
          for (const kw of TP_LOL_KW)  if (txt.includes(kw)) return 'league-of-legends';
          for (const kw of TP_VAL_KW)  if (txt.includes(kw)) return 'valorant';
        }
        sib = sib.previousElementSibling;
        sibCount++;
      }
      node = node.parentElement;
    }
    return null; // Unknown, keep as 'esports'
  }

  function looksLikeRealClub(t1, t2) {
    const key = (t1 + t2).toLowerCase().replace(/[^a-z]/g, '');
    return TP_REAL_TEAMS.some(team => key.includes(team));
  }

  function scanTipsportMatches() {
    const matches = [];
    const sport = detectTipsportSport();
    const seen = new Set();

    // Find all <a> links that look like match names: "Team1 - Team2"
    const allLinks = document.querySelectorAll('a');
    let candidateCount = 0;

    for (const link of allLinks) {
      const rawText = link.textContent.trim().replace(/\s+/g, ' ');

      // Quick filters
      if (rawText.length < 5 || rawText.length > 300) continue;

      // Must contain " - " separator (team1 - team2)
      const dashIdx = rawText.indexOf(' - ');
      if (dashIdx < 2) continue;

      // Strip Tipsport garbage BEFORE team name extraction
      // Tipsport <a> tags wrap entire match row: "Team1 - Team2 0:0 2.pol - 55.min 1.03 15.00 60.00"
      const cleanedText = stripTipsportGarbage(rawText);
      if (cleanedText !== rawText) dbg(`Stripped: "${rawText.substring(0,80)}" → "${cleanedText}"`);
      if (!cleanedText || cleanedText.indexOf(' - ') < 2) continue;

      // Also accept " – " (en-dash)
      let t1, t2;
      const dashMatch = cleanedText.match(/^(.+?)\s*[-–]\s*(.+)$/);
      if (!dashMatch) continue;
      t1 = cleanName(dashMatch[1]);
      t2 = cleanName(dashMatch[2]);

      if (!t1 || !t2 || t1.length < 2 || t2.length < 2) continue;
      if (t1.toLowerCase() === t2.toLowerCase()) continue;

      // Skip league/header links (contain comma + sport name like "Liga mistrů, Fotbal - muži")
      if (cleanedText.includes(',') && /fotbal|tenis|hokej|basket|esport/i.test(cleanedText)) continue;
      // Skip "muži"/"ženy" league headers
      if (/\s*-\s*(muži|ženy|women|men)/i.test(cleanedText)) continue;

      // Deduplicate
      const key = `${t1.toLowerCase()}|${t2.toLowerCase()}`;
      if (seen.has(key)) continue;
      seen.add(key);
      candidateCount++;

      // Walk up DOM to find the match container with odds
      const rowInfo = findMatchRowData(link);
      if (!rowInfo || !rowInfo.hasOdds) {
        dbg(`No odds for: ${t1} - ${t2}`);
        continue;
      }

      // Determine final sport — for esports, try to detect sub-sport from DOM
      let matchSport = sport || "unknown";
      if (matchSport === 'esports') {
        const specific = detectEsportFromLink(link);
        if (specific === 'skip') {
          dbg(`TP skip e-sport (eFootball/eBasketball): ${t1} vs ${t2}`);
          continue;
        }
        if (specific) {
          matchSport = specific;
          dbg(`TP esport detected: ${matchSport} for ${t1} vs ${t2}`);
        } else if (looksLikeRealClub(t1, t2)) {
          dbg(`TP skip real-club esport: ${t1} vs ${t2}`);
          continue;
        }
      }

      matches.push({
        team1: t1,
        team2: t2,
        odds1: rowInfo.odds1,
        odds2: rowInfo.odds2,
        oddsX: rowInfo.oddsX,
        score1: rowInfo.score1,
        score2: rowInfo.score2,
        isLive: rowInfo.isLive,
        hasOdds: rowInfo.hasOdds,
        sport: matchSport,
      });
    }

    dbg(`Link candidates: ${candidateCount}, with odds: ${matches.length}`);
    return matches;
  }

  /**
   * Walk up the DOM from a match-name link to find the row container
   * that holds odds values. A match row typically has 2-6 odds.
   */
  function findMatchRowData(linkElement) {
    let container = linkElement;

    for (let depth = 0; depth < 8; depth++) {
      container = container.parentElement;
      if (!container || container === document.body) return null;

      const odds = extractOddsValues(container);

      // A single match row should have 2-8 odds values (1/2 or 1/X/2 + maybe handicap)
      if (odds.length >= 2 && odds.length <= 8) {
        const txt = container.textContent.toLowerCase();

        // Live detection from Czech text patterns
        const isLive = /\d+\s*:\s*\d+/.test(txt) ||
                       txt.includes('pol.') || txt.includes('.min') ||
                       txt.includes('přestávka') || txt.includes('live') ||
                       txt.includes('živě') || txt.includes('probíhá');

        // Score extraction (first "N:N" pattern)
        let score1 = 0, score2 = 0;
        const scoreMatch = txt.match(/(\d+)\s*:\s*(\d+)/);
        if (scoreMatch) {
          score1 = parseInt(scoreMatch[1]) || 0;
          score2 = parseInt(scoreMatch[2]) || 0;
        }

        // Assign odds: if 3+, treat as 1/X/2; if 2, treat as 1/2
        let odds1, oddsX, odds2;
        if (odds.length >= 3) {
          odds1 = odds[0]; oddsX = odds[1]; odds2 = odds[2];
        } else {
          odds1 = odds[0]; oddsX = 0; odds2 = odds[1];
        }

        return {
          odds1, odds2, oddsX, score1, score2, isLive,
          hasOdds: odds1 > 1.0 && odds2 > 1.0,
        };
      }

      // If we find way too many odds, we've walked up to a page-level container
      if (odds.length > 30) return null;
    }

    return null;
  }

  /**
   * Extract odds-like decimal values from leaf elements inside a container.
   * Matches patterns like "1.03", "15.00", "1,85", "120.00".
   * Only counts the DEEPEST elements (children.length === 0) to avoid double-counting.
   */
  function extractOddsValues(container) {
    const values = [];
    const candidates = container.querySelectorAll(
      'span, button, td, div, b, strong, a, em, i, label, p'
    );

    for (const el of candidates) {
      // Only leaf elements — no sub-elements (prevents counting parent + child)
      if (el.children.length > 0) continue;

      const text = el.textContent.trim();
      // Odds are "1.03" to "999.99" — max 7 chars
      if (text.length < 3 || text.length > 7) continue;

      // Match decimal odds pattern: "1.03", "15.00", "1,85"
      if (/^\d{1,3}[,.]\d{2}$/.test(text)) {
        const val = parseFloat(text.replace(',', '.'));
        if (val >= 1.01 && val <= 500) {
          values.push(val);
        }
      }
    }

    return values;
  }

  /**
   * Strip Tipsport garbage from <a> link text.
   * Tipsport wraps entire match rows in <a> tags, so textContent is like:
   *   "Atalanta Bergamo - Dortmund (odv.)2:02. pol. - 55.min (2:0, 0:0)1.0315.0060.00+53"
   *   "Rinderknech Arthur - Draper Jack1:02.set - 7:5, 3:3 (*30:00)11.5122.53+25"
   *   "KUUSAMO.gg - Partizan EsportZa 13 minut11.8021.90+3"
   *
   * We find the EARLIEST cut point where score/status/odds garbage starts.
   */
  function stripTipsportGarbage(text) {
    const cutPatterns = [
      /\d{1,2}:\d{1,2}/,                    // Score: "0:0", "2:3", "22:36"
      /\d\.\s*(pol|set|min|čt|mapa|kolo)/i,  // Period: "2.pol", "1.set"
      /Za\s+\d+/i,                           // Prematch: "Za 13 minut"
      /Kurzy\s/i,                            // "Kurzy nejsou..."
      /Událost\s/i,                          // "Událost skončila..."
      /Lepší\s+ze/i,                         // "Lepší ze 3"
      /Přestávka/i,                          // Half-time
      /\d{2,}[,.]\d{2}/,                     // Odds values: "11.50", "118.00"
      /\+\d{2,}/,                            // Bet count: "+53", "+25"
    ];

    let minIdx = text.length;
    for (const pattern of cutPatterns) {
      const m = text.match(pattern);
      if (m && m.index < minIdx) {
        minIdx = m.index;
      }
    }

    return text.substring(0, minIdx).trim();
  }

  function cleanName(text) {
    if (!text) return "";
    return text
      .replace(/\([^)]*\)\s*$/g, "")  // Remove trailing parenthetical: "(odv.)", "(OM)", "(KSA)"
      .replace(/\(\d+\)/g, "")         // Remove seeding like "(1)"
      .replace(/^\d+\.\s*/, "")        // Remove numbering like "1. "
      .replace(/\s+/g, " ")
      .trim();
  }

  // ====================================================================
  // FEED MESSAGES
  // ====================================================================

  function buildOddsMessage(match) {
    const msg = {
      v: 1,
      type: "odds",
      source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: {
        sport: match.sport,
        bookmaker: "tipsport",
        market: "match_winner",
        team1: match.team1,
        team2: match.team2,
        odds_team1: match.odds1,
        odds_team2: match.odds2,
        url: window.location.href,
      },
    };
    return msg;
  }

  function buildLiveMessage(match) {
    if (!match.isLive) return null;
    return {
      v: 1,
      type: "live_match",
      source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: {
        sport: match.sport,
        team1: match.team1,
        team2: match.team2,
        score1: match.score1,
        score2: match.score2,
        status: "Live",
        url: window.location.href,
      },
    };
  }

  // ====================================================================
  // SCAN LOOP
  // ====================================================================

  function startScanning() {
    stopScanning();
    doScan();
    scanTimer = setInterval(doScan, SCAN_INTERVAL_MS);
  }
  function stopScanning() { if (scanTimer) { clearInterval(scanTimer); scanTimer = null; } }

  function doScan() {
    const matches = scanTipsportMatches();
    let sentThisScan = 0;
    const sportCounts = {};

    for (const match of matches) {
      // Send odds (primary purpose)
      const oddsMsg = buildOddsMessage(match);
      if (sendJSON(oddsMsg)) sentThisScan++;

      // Send live score if available (bonus)
      if (match.isLive) {
        const liveMsg = buildLiveMessage(match);
        if (liveMsg && sendJSON(liveMsg)) sentThisScan++;
      }

      sportCounts[match.sport] = (sportCounts[match.sport] || 0) + 1;
    }

    const statusText = connected ? "✅ Connected" : "❌ Disconnected";
    const sport = detectTipsportSport();

    const matchInfo = matches.length > 0
      ? matches.slice(0, 8).map(m =>
          `${m.team1} ${m.odds1.toFixed(2)} / ${m.odds2.toFixed(2)} ${m.team2}${m.isLive ? " 🔴" : ""}`
        ).join("\n") + (matches.length > 8 ? `\n...+${matches.length - 8} more` : "")
      : "No matches with odds found";

    updatePanel(statusText, sport, matchInfo, sentCount);

    if (matches.length > 0) {
      const sportSummary = Object.entries(sportCounts).map(([s, c]) => `${s}:${c}`).join(", ");
      log(`Scan: ${matches.length} matches [${sportSummary}], sent ${sentThisScan}`);
    }
  }

  // ====================================================================
  // INIT
  // ====================================================================

  function init() {
    const sport = detectTipsportSport();
    log(`💰 Tipsport Odds Scraper v2.1 (Generic DOM + text cleanup)`);
    log(`Page: ${window.location.href}`);
    log(`Sport: ${sport || "unknown"}`);
    log(`Strategy: Find <a> links with 'Team - Team', strip scores/status garbage, walk up DOM for odds`);

    createPanel();
    connectWS();

    // No auto-refresh for Tipsport (login session would be lost)
  }

  if (document.readyState === "complete") {
    setTimeout(init, 2000);
  } else {
    window.addEventListener("load", () => setTimeout(init, 2000));
  }
})();
