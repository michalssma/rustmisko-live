// ==UserScript==
// @name         FlashScore → Feed Hub MULTI-SPORT Scraper v2
// @namespace    rustmisko
// @version      2.0
// @description  Scrapes ALL live matches from FlashScore (tennis, football, basketball, hockey, etc.), sends to Feed Hub via WebSocket
// @author       RustMisko
// @match        https://www.flashscore.com/*
// @match        https://www.flashscore.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 5000; // 5s — universal for all sports
  const RECONNECT_MS = 3000;
  const HEARTBEAT_MS = 15000;
  const SOURCE_NAME = "flashscore-multi";
  const DEBUG = true;

  // Auto-refresh to keep DOM fresh (10 min — longer for multi-sport)
  const AUTO_REFRESH_MS = 10 * 60 * 1000;

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let scanTimer = null;
  let hbTimer = null;
  let lastScanHash = "";

  const PREFIX = "[FS→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // SPORT DETECTION
  // ====================================================================

  // FlashScore sport slug → our protocol sport name
  const SPORT_MAP = {
    "tennis":     "tennis",
    "tenis":      "tennis",
    "football":   "football",
    "fotbal":     "football",
    "soccer":     "football",
    "basketball": "basketball",
    "basketbal":  "basketball",
    "hockey":     "hockey",
    "hokej":      "hockey",
    "ice-hockey": "hockey",
    "esports":    "esports",
    "e-sports":   "esports",
    "mma":        "mma",
    "baseball":   "baseball",
    "handball":   "handball",
    "volleyball": "volleyball",
    "volejbal":   "volleyball",
    "dota":       "dota-2",
    "dota2":      "dota-2",
    "dota-2":     "dota-2",
    "csgo":       "cs2",
    "cs2":        "cs2",
    "cs:go":      "cs2",
    "counter-strike": "cs2",
    "lol":        "league-of-legends",
    "league-of-legends": "league-of-legends",
  };

  /**
   * Detect sport from URL, breadcrumb, or event header
   */
  function detectSportFromURL() {
    const url = window.location.href.toLowerCase();
    for (const [key, sport] of Object.entries(SPORT_MAP)) {
      if (url.includes(`/${key}/`) || url.includes(`/${key}?`)) {
        return sport;
      }
    }
    return null;
  }

  function detectSportFromElement(el) {
    if (!el) return null;

    // Walk up to find sport header
    const header = el.closest('.event__header, [class*="sportName"], [class*="event__title"]');
    if (header) {
      const text = header.textContent.toLowerCase();
      for (const [key, sport] of Object.entries(SPORT_MAP)) {
        if (text.includes(key)) return sport;
      }
    }

    // Check data attributes
    const sportAttr = el.getAttribute("data-sport") || el.closest("[data-sport]")?.getAttribute("data-sport");
    if (sportAttr) {
      const normalized = sportAttr.toLowerCase();
      return SPORT_MAP[normalized] || normalized;
    }

    return null;
  }

  function getPageSport() {
    // 1. URL-based
    const urlSport = detectSportFromURL();
    if (urlSport) return urlSport;

    // 2. Heading / breadcrumb
    const heading = document.querySelector('.heading__title, .breadcrumb, [class*="heading"]');
    if (heading) {
      const text = heading.textContent.toLowerCase();
      for (const [key, sport] of Object.entries(SPORT_MAP)) {
        if (text.includes(key)) return sport;
      }
    }

    // 3. Main page — unknown/mixed
    return null;
  }

  // ====================================================================
  // UI PANEL
  // ====================================================================
  function createPanel() {
    const panel = document.createElement("div");
    panel.id = "fs-multisport-panel";
    panel.style.cssText = `
      position: fixed; bottom: 10px; left: 10px; z-index: 999999;
      background: #1a1a2e; color: #0fa; font-family: 'Consolas', monospace;
      font-size: 11px; padding: 10px 14px; border-radius: 8px;
      border: 1px solid #0fa; min-width: 280px; max-width: 400px;
      opacity: 0.92; box-shadow: 0 0 20px rgba(0,255,170,0.15);
      max-height: 300px; overflow-y: auto;
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom: 4px;">🏆 Multi-Sport → Feed Hub v2</div>
      <div id="fs-status" style="color: #fa0;">⏳ Connecting...</div>
      <div id="fs-sport" style="margin-top: 2px; color: #aaa;">Sport: detecting...</div>
      <div id="fs-matches" style="margin-top: 4px; color: #aaa; max-height: 150px; overflow-y: auto;">Scanning...</div>
      <div id="fs-sent" style="margin-top: 2px; color: #888;">Sent: 0</div>
    `;
    document.body.appendChild(panel);
    return panel;
  }

  function updatePanel(status, sport, matches, sent) {
    const el = (id) => document.getElementById(id);
    if (el("fs-status")) el("fs-status").textContent = status;
    if (el("fs-sport")) el("fs-sport").textContent = `Sport: ${sport || "all/unknown"}`;
    if (el("fs-matches")) el("fs-matches").textContent = matches;
    if (el("fs-sent")) el("fs-sent").textContent = `Sent: ${sent}`;
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
      updatePanel("✅ Connected", getPageSport(), "Scanning...", sentCount);
      startScanning();
      startHeartbeat();
    };

    ws.onclose = (e) => {
      connected = false;
      log("❌ Disconnected:", e.code, e.reason);
      updatePanel("❌ Disconnected", getPageSport(), "Reconnecting...", sentCount);
      stopScanning();
      stopHeartbeat();
      setTimeout(connectWS, RECONNECT_MS);
    };

    ws.onerror = (e) => { log("WS error:", e); };
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
      if (connected) {
        sendJSON({ v: 1, type: "heartbeat", source: SOURCE_NAME, ts: new Date().toISOString() });
      }
    }, HEARTBEAT_MS);
  }
  function stopHeartbeat() { if (hbTimer) { clearInterval(hbTimer); hbTimer = null; } }

  // ====================================================================
  // SCANNING — FlashScore DOM extraction (MULTI-SPORT)
  // ====================================================================

  function scanAllLiveMatches() {
    const matches = [];
    const pageSport = getPageSport();

    // Find all live match rows
    const liveRows = document.querySelectorAll(
      '.event__match--live, .event__match--oneLine--live, [class*="event__match"][class*="live"]'
    );

    for (const row of liveRows) {
      try {
        const match = extractMatchFromRow(row, pageSport);
        if (match) {
          matches.push(match);
        }
      } catch (e) {
        dbg("Error extracting match:", e);
      }
    }

    // Fallback: if on a specific sport page, check all rows for live indicators
    if (matches.length === 0 && pageSport) {
      const allRows = document.querySelectorAll('.event__match, [id^="g_1_"]');
      for (const row of allRows) {
        try {
          if (isLiveRow(row)) {
            const match = extractMatchFromRow(row, pageSport);
            if (match) {
              matches.push(match);
            }
          }
        } catch (e) {
          dbg("Error in fallback scan:", e);
        }
      }
    }

    return matches;
  }

  function isLiveRow(row) {
    const classes = row.className || "";
    if (classes.includes("live")) return true;

    const stageEl = row.querySelector('.event__stage, .event__stage--live, [class*="stage"]');
    if (stageEl) {
      const text = stageEl.textContent.trim().toLowerCase();
      if (text && !text.includes("finished") && !text.includes("ended")
        && !text.includes("after") && !text.includes("final")
        && !text.includes("postp") && !text.includes("cancel")) {
        return true;
      }
    }

    const timerEl = row.querySelector('.icon--clock, .eventTimer, [class*="timer"], [class*="clock"]');
    if (timerEl) return true;

    return false;
  }

  function extractMatchFromRow(row, pageSport) {
    // Team names
    const participants = row.querySelectorAll(
      '.event__participant, [class*="participant"], .event__participant--home, .event__participant--away'
    );
    if (participants.length < 2) return null;

    const team1 = cleanTeamName(participants[0].textContent);
    const team2 = cleanTeamName(participants[1].textContent);
    if (!team1 || !team2) return null;

    // Detect sport for this row
    const sport = detectSportFromElement(row) || pageSport || "unknown";

    // === SCORE EXTRACTION ===
    // FlashScore uses different score layouts per sport:
    //   Tennis: sets (main score) + games (parts)
    //   Football: goals (main score) + halftime
    //   Basketball: total points (main) + quarters (parts)

    // Main score (total / sets won)
    const scoreEls = row.querySelectorAll('.event__score, [class*="event__score"]');
    let score1 = 0, score2 = 0;
    if (scoreEls.length >= 2) {
      score1 = parseInt(scoreEls[0].textContent.trim()) || 0;
      score2 = parseInt(scoreEls[1].textContent.trim()) || 0;
    }

    // Period/set/quarter scores
    const partEls = row.querySelectorAll('.event__part, [class*="event__part"]');
    const partScores = [];
    for (let i = 0; i < partEls.length; i += 2) {
      if (i + 1 < partEls.length) {
        const s1 = parseInt(partEls[i].textContent.trim()) || 0;
        const s2 = parseInt(partEls[i + 1].textContent.trim()) || 0;
        partScores.push(`${s1}-${s2}`);
      }
    }

    // Game status
    const stageEl = row.querySelector('.event__stage, .event__stage--live, [class*="stage"]');
    const status = stageEl ? stageEl.textContent.trim() : "Live";

    // Match ID
    const matchId = row.id || row.getAttribute("data-id") || `${team1}_${team2}`;

    return {
      team1,
      team2,
      score1,   // Main score (sets for tennis, goals for football, etc.)
      score2,
      partScores,
      status,
      sport,
      matchId,
    };
  }

  function cleanTeamName(text) {
    if (!text) return "";
    return text
      .replace(/\(\d+\)/g, "")    // Remove seeding "(1)"
      .replace(/\(W\)/gi, "")      // Remove "(W)" women marker
      .replace(/\s+/g, " ")
      .trim();
  }

  // ====================================================================
  // FEED MESSAGES
  // ====================================================================

  function buildLiveMatchMessage(match) {
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
        status: match.status,
        url: window.location.href,
      },
    };
  }

  // ====================================================================
  // ODDS SCRAPING (from FlashScore odds comparison)
  // ====================================================================
  // FlashScore sometimes shows odds from bookmakers — we can scrape those
  // as additional reference data for odds anomaly detection.
  // This is a bonus feature — not all pages show odds.

  function scrapeVisibleOdds(matches) {
    // On match detail pages, FlashScore shows odds from various bookmakers.
    // Currently we only send live_match data; odds scraping is for future use.
    // TODO: Implement odds scraping from FlashScore comparison tables
  }

  // ====================================================================
  // SCAN LOOP
  // ====================================================================

  function startScanning() {
    stopScanning();
    doScan();
    scanTimer = setInterval(doScan, SCAN_INTERVAL_MS);
  }

  function stopScanning() {
    if (scanTimer) { clearInterval(scanTimer); scanTimer = null; }
  }

  function doScan() {
    const matches = scanAllLiveMatches();
    const hash = JSON.stringify(matches.map(m => `${m.sport}:${m.team1}:${m.team2}:${m.score1}:${m.score2}`));

    let sentThisScan = 0;
    const sportCounts = {};

    for (const match of matches) {
      const msg = buildLiveMatchMessage(match);
      if (sendJSON(msg)) {
        sentThisScan++;
        sportCounts[match.sport] = (sportCounts[match.sport] || 0) + 1;
      }
    }

    const statusText = connected ? "✅ Connected" : "❌ Disconnected";
    const pageSport = getPageSport();

    const matchInfo = matches.length > 0
      ? matches.slice(0, 10).map(m =>
          `[${m.sport}] ${m.team1} ${m.score1}-${m.score2} ${m.team2}`
        ).join("\n") + (matches.length > 10 ? `\n...+${matches.length - 10} more` : "")
      : "No live matches found";

    const sportSummary = Object.entries(sportCounts).map(([s, c]) => `${s}:${c}`).join(", ");

    updatePanel(statusText, pageSport || sportSummary, matchInfo, sentCount);

    if (matches.length > 0 || hash !== lastScanHash) {
      log(`Scan: ${matches.length} live [${sportSummary}], sent ${sentThisScan}`);
    }
    lastScanHash = hash;
  }

  // ====================================================================
  // AUTO-REFRESH
  // ====================================================================

  function scheduleAutoRefresh() {
    setTimeout(() => {
      log("🔄 Auto-refreshing page...");
      window.location.reload();
    }, AUTO_REFRESH_MS);
  }

  // ====================================================================
  // INIT
  // ====================================================================

  function init() {
    const pageSport = getPageSport();
    log(`🏆 FlashScore Multi-Sport Scraper v2.0`);
    log(`Page: ${window.location.href}`);
    log(`Detected sport: ${pageSport || "all/mixed"}`);

    createPanel();
    connectWS();
    scheduleAutoRefresh();

    if (!pageSport) {
      log("📋 Main page detected — will scrape ALL visible live matches");
      log("  For best results, navigate to a specific sport:");
      log("  Tennis:     flashscore.com/tennis/");
      log("  Football:   flashscore.com/football/");
      log("  Basketball: flashscore.com/basketball/");
      log("  Esports:    flashscore.com/esports/");
    }
  }

  // Wait for page to settle
  if (document.readyState === "complete") {
    setTimeout(init, 1500);
  } else {
    window.addEventListener("load", () => setTimeout(init, 1500));
  }
})();
