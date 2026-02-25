// ==UserScript==
// @name         Tipsport → Feed Hub Odds Scraper
// @namespace    rustmisko
// @version      1.0
// @description  Scrapes live odds from Tipsport.cz and sends to Feed Hub as reference odds for anomaly detection
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
  // TIPSPORT SCRAPING
  // ====================================================================

  /**
   * Tipsport DOM structure (may vary, we try multiple selectors):
   *
   * Live matches typically in a table/list with:
   *   - Match name (team1 - team2)
   *   - "1" / "X" / "2" odds buttons
   *   - Live indicator (score, clock, "LIVE" badge)
   *
   * Selectors may include:
   *   .o-match, .m-matchRow, .o-event, [data-testid*="match"]
   *   .o-odd__value, .m-odds__odd, [class*="odd"]
   *
   * We try multiple strategies to handle DOM changes.
   */

  function scanTipsportMatches() {
    const matches = [];
    const sport = detectTipsportSport();

    // Strategy 1: Modern Tipsport layout
    const matchRows = document.querySelectorAll(
      '.o-match, .m-matchRow, .o-event, [class*="match-row"], [class*="eventRow"], [data-testid*="match"], tr[class*="match"]'
    );

    for (const row of matchRows) {
      try {
        const match = extractTipsportMatch(row, sport);
        if (match && match.hasOdds) {
          matches.push(match);
        }
      } catch (e) {
        dbg("Error extracting Tipsport match:", e);
      }
    }

    // Strategy 2: Try generic table rows if nothing found
    if (matches.length === 0) {
      const tableRows = document.querySelectorAll(
        'table tbody tr, .event-list-item, [class*="event__"]'
      );
      for (const row of tableRows) {
        try {
          const match = extractTipsportMatch(row, sport);
          if (match && match.hasOdds) {
            matches.push(match);
          }
        } catch (e) { /* skip */ }
      }
    }

    return matches;
  }

  function extractTipsportMatch(row, sport) {
    // === TEAM NAMES ===
    // Tipsport shows "Team1 - Team2" in match name, or separate participant elements
    const nameEl = row.querySelector(
      '.o-match__name, .m-matchRow__name, [class*="match-name"], [class*="matchName"], [class*="event-name"], .o-match__participants'
    );

    let team1, team2;

    if (nameEl) {
      const fullText = nameEl.textContent.trim();
      // Split by " - " or " – " or " vs "
      const parts = fullText.split(/\s*[-–]\s*|\s+vs\.?\s+/i);
      if (parts.length >= 2) {
        team1 = cleanName(parts[0]);
        team2 = cleanName(parts[parts.length - 1]); // Use last part in case of multiple separators
      }
    }

    // Try separate participant elements if name parsing failed
    if (!team1 || !team2) {
      const participants = row.querySelectorAll(
        '.o-match__participant, [class*="participant"], [class*="team-name"]'
      );
      if (participants.length >= 2) {
        team1 = cleanName(participants[0].textContent);
        team2 = cleanName(participants[1].textContent);
      }
    }

    if (!team1 || !team2) return null;

    // === ODDS ===
    // Tipsport shows odds as buttons: "1" (home), "X" (draw), "2" (away)
    const oddEls = row.querySelectorAll(
      '.o-odd__value, .m-odds__odd, [class*="odd-value"], [class*="oddValue"], button[class*="odd"], [data-testid*="odd"]'
    );

    let odds1 = 0, oddsX = 0, odds2 = 0;
    let hasOdds = false;

    if (oddEls.length >= 2) {
      // 2-way market (tennis, esports): just 1 and 2
      odds1 = parseOdds(oddEls[0].textContent);
      if (oddEls.length >= 3) {
        // 3-way market: 1, X, 2
        oddsX = parseOdds(oddEls[1].textContent);
        odds2 = parseOdds(oddEls[2].textContent);
      } else {
        odds2 = parseOdds(oddEls[1].textContent);
      }
      hasOdds = odds1 > 1.0 && odds2 > 1.0;
    }

    // === LIVE DETECTION ===
    const isLive = isLiveMatch(row);

    // === SCORE (if live) ===
    let score1 = 0, score2 = 0;
    const scoreEl = row.querySelector(
      '.o-match__score, [class*="score"], [class*="result"]'
    );
    if (scoreEl) {
      const scoreText = scoreEl.textContent.trim();
      const scoreParts = scoreText.split(/[:\-–]/);
      if (scoreParts.length >= 2) {
        score1 = parseInt(scoreParts[0].trim()) || 0;
        score2 = parseInt(scoreParts[1].trim()) || 0;
      }
    }

    return {
      team1,
      team2,
      odds1,
      odds2,
      oddsX,
      score1,
      score2,
      isLive,
      hasOdds,
      sport: sport || "unknown",
    };
  }

  function isLiveMatch(row) {
    const text = row.textContent.toLowerCase();
    if (text.includes("live") || text.includes("živě") || text.includes("probíhá")) return true;

    const liveEl = row.querySelector(
      '.o-match__live, [class*="live"], [class*="inplay"], .live-icon, [data-testid*="live"]'
    );
    if (liveEl) return true;

    const scoreEl = row.querySelector('[class*="score"]');
    if (scoreEl && /\d+\s*[:–-]\s*\d+/.test(scoreEl.textContent)) return true;

    return false;
  }

  function cleanName(text) {
    if (!text) return "";
    return text
      .replace(/\(\d+\)/g, "")    // Remove seeding
      .replace(/^\d+\.\s*/, "")   // Remove numbering like "1. "
      .replace(/\s+/g, " ")
      .trim();
  }

  function parseOdds(text) {
    if (!text) return 0;
    // Tipsport uses Czech format: "1,85" not "1.85"
    const cleaned = text.trim().replace(",", ".");
    const val = parseFloat(cleaned);
    return (val >= 1.01 && val <= 100) ? val : 0;
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
    log(`💰 Tipsport Odds Scraper v1.0`);
    log(`Page: ${window.location.href}`);
    log(`Sport: ${sport || "unknown"}`);

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
