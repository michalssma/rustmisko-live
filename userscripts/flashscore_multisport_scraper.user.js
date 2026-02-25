// ==UserScript==
// @name         FlashScore → Feed Hub MULTI-SPORT Scraper v3
// @namespace    rustmisko
// @version      3.0
// @description  Scrapes ALL live matches from FlashScore using generic DOM detection (no CSS class dependency). Supports tennis, football, basketball, esports, etc.
// @author       RustMisko
// @match        https://www.flashscore.com/*
// @match        https://www.flashscore.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 5000;
  const RECONNECT_MS = 3000;
  const HEARTBEAT_MS = 15000;
  const SOURCE_NAME = "flashscore-multi";
  const DEBUG = true;
  const AUTO_REFRESH_MS = 10 * 60 * 1000; // 10 min

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let matchCount = 0;
  let scanTimer = null;
  let hbTimer = null;

  const PREFIX = "[FS→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // SPORT DETECTION — URL-first, reliable
  // ====================================================================

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
    "counter-strike": "cs2",
    "lol":        "league-of-legends",
    "league-of-legends": "league-of-legends",
    "table-tennis": "table-tennis",
    "badminton":  "badminton",
  };

  function detectSportFromURL() {
    const path = window.location.pathname.toLowerCase();
    for (const [key, sport] of Object.entries(SPORT_MAP)) {
      if (path.startsWith(`/${key}/`) || path === `/${key}`) {
        return sport;
      }
    }
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
      <div style="font-weight:bold; margin-bottom: 4px;">🏆 Multi-Sport → Feed Hub v3</div>
      <div id="fs-status" style="color: #fa0;">⏳ Connecting...</div>
      <div id="fs-sport" style="margin-top: 2px; color: #aaa;">Sport: detecting...</div>
      <div id="fs-matches" style="margin-top: 4px; color: #aaa; white-space: pre-wrap; max-height: 180px; overflow-y: auto;">Scanning...</div>
      <div id="fs-sent" style="margin-top: 2px; color: #888;">Sent: 0</div>
    `;
    document.body.appendChild(panel);
  }

  function updatePanel(status, sport, matchesText, sent) {
    const el = (id) => document.getElementById(id);
    if (el("fs-status")) el("fs-status").textContent = status;
    if (el("fs-sport")) el("fs-sport").textContent = `Sport: ${sport || "detecting..."}`;
    if (el("fs-matches")) el("fs-matches").textContent = matchesText;
    if (el("fs-sent")) el("fs-sent").textContent = `Sent: ${sent} | Live: ${matchCount}`;
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
      updatePanel("✅ Connected", detectSportFromURL(), "Scanning...", sentCount);
      startScanning();
      startHeartbeat();
    };
    ws.onclose = (e) => {
      connected = false;
      log("❌ Disconnected:", e.code);
      updatePanel("❌ Disconnected", detectSportFromURL(), "Reconnecting...", sentCount);
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
  // GENERIC DOM SCANNING — v3.0
  // ====================================================================
  /**
   * FlashScore DOM strategy (works regardless of CSS class names):
   *
   * Strategy 1: Elements with id starting with "g_1_" = LIVE matches on FlashScore
   *   FlashScore uses: g_1_ = live, g_2_ = finished, g_3_ = not started
   *
   * Strategy 2: Elements with class containing "event__match" + "live"
   *
   * Strategy 3: Generic scan of container divs with participant + score patterns
   */

  function scanAllLiveMatches() {
    const matches = [];
    const seen = new Set();
    const pageSport = detectSportFromURL();

    // === STRATEGY 1: g_1_ IDs (FlashScore standard for live matches) ===
    const gLiveEls = document.querySelectorAll('[id^="g_1_"]');
    dbg(`Strategy 1: ${gLiveEls.length} g_1_ elements`);
    for (const el of gLiveEls) {
      const match = tryExtractMatch(el, pageSport, seen);
      if (match) matches.push(match);
    }

    // === STRATEGY 2: BEM class patterns ===
    if (matches.length === 0) {
      const bemLive = document.querySelectorAll(
        '[class*="event__match"][class*="live"], [class*="event__match--live"]'
      );
      dbg(`Strategy 2a: ${bemLive.length} event__match--live elements`);
      for (const el of bemLive) {
        const match = tryExtractMatch(el, pageSport, seen);
        if (match) matches.push(match);
      }

      // Also try ALL event__match elements and filter for live
      if (matches.length === 0) {
        const allBem = document.querySelectorAll('[class*="event__match"]');
        dbg(`Strategy 2b: ${allBem.length} event__match elements`);
        for (const el of allBem) {
          if (isLiveElement(el)) {
            const match = tryExtractMatch(el, pageSport, seen);
            if (match) matches.push(match);
          }
        }
      }
    }

    // === STRATEGY 3: Find divs with "participant" children + score ===
    if (matches.length === 0) {
      dbg("Strategy 3: looking for participant elements...");
      const participantEls = document.querySelectorAll(
        '[class*="participant"], [class*="team"], [class*="player"]'
      );
      dbg(`Found ${participantEls.length} participant-like elements`);

      // Walk up to find match containers
      const containers = new Set();
      for (const el of participantEls) {
        let parent = el.parentElement;
        for (let i = 0; i < 4; i++) {
          if (!parent || parent === document.body) break;
          // A match row container should have 2+ participant children
          const parts = parent.querySelectorAll('[class*="participant"], [class*="team"], [class*="player"]');
          if (parts.length >= 2) {
            containers.add(parent);
            break;
          }
          parent = parent.parentElement;
        }
      }
      dbg(`Strategy 3: ${containers.size} potential containers`);
      for (const el of containers) {
        if (isLiveElement(el)) {
          const match = tryExtractMatch(el, pageSport, seen);
          if (match) matches.push(match);
        }
      }
    }

    // === STRATEGY 4: Last resort — find ALL <a> pairs that look like team matchups ===
    if (matches.length === 0) {
      dbg("Strategy 4: link-based scan...");
      const allLinks = document.querySelectorAll('a[href*="/match/"], a[href*="/game/"]');
      dbg(`Found ${allLinks.length} match/game links`);
      for (const link of allLinks) {
        const container = link.closest('div, tr, li, article') || link.parentElement;
        if (!container) continue;
        if (isLiveElement(container)) {
          const match = tryExtractMatch(container, pageSport, seen);
          if (match) matches.push(match);
        }
      }
    }

    return matches;
  }

  /**
   * Try to extract a live match from a DOM element.
   */
  function tryExtractMatch(el, pageSport, seen) {
    const names = findParticipants(el);
    if (names.length < 2) return null;

    const team1 = cleanTeamName(names[0]);
    const team2 = cleanTeamName(names[1]);

    if (!team1 || !team2 || team1.length < 2 || team2.length < 2) return null;
    if (team1.toLowerCase() === team2.toLowerCase()) return null;

    const key = `${team1.toLowerCase()}|${team2.toLowerCase()}`;
    if (seen.has(key)) return null;
    seen.add(key);

    const scores = extractScores(el);
    const sport = pageSport || "unknown";
    if (sport === "unknown") {
      dbg(`Skip unknown sport: ${team1} vs ${team2}`);
      return null;
    }

    return {
      team1, team2,
      score1: scores.score1,
      score2: scores.score2,
      status: scores.status || "Live",
      sport,
    };
  }

  /**
   * Find participant/team name texts within a container.
   */
  function findParticipants(container) {
    const names = [];

    // Method 1: Elements with "participant" in class
    const partEls = container.querySelectorAll('[class*="participant"]');
    if (partEls.length >= 2) {
      for (const el of partEls) {
        const text = getCleanText(el);
        if (isNameLike(text)) names.push(text);
      }
      if (names.length >= 2) return names.slice(0, 2);
      names.length = 0; // Reset if not enough
    }

    // Method 2: Elements with "team" or "player" in class
    const teamEls = container.querySelectorAll('[class*="team"], [class*="player"]');
    if (teamEls.length >= 2) {
      for (const el of teamEls) {
        const text = getCleanText(el);
        if (isNameLike(text)) names.push(text);
      }
      if (names.length >= 2) return names.slice(0, 2);
      names.length = 0;
    }

    // Method 3: Direct child <a> or <span> with name-like text
    const inlineEls = container.querySelectorAll('a, span');
    for (const el of inlineEls) {
      if (el.children.length > 3) continue; // Skip containers with many children
      const text = getCleanText(el);
      if (text.length >= 3 && text.length <= 40 && isNameLike(text)) {
        // Avoid adding the same name twice
        if (!names.includes(text)) names.push(text);
        if (names.length >= 2) return names.slice(0, 2);
      }
    }

    return names;
  }

  function getCleanText(el) {
    // Get text, preferring direct text content over nested element text
    let direct = '';
    for (const child of el.childNodes) {
      if (child.nodeType === 3) direct += child.textContent;
    }
    direct = direct.trim();
    if (direct && direct.length >= 2) return direct;

    // Fallback: if element has no deep children, use full textContent
    if (el.querySelectorAll('*').length <= 2) {
      return el.textContent.trim();
    }
    return direct || '';
  }

  function isNameLike(text) {
    if (!text || text.length < 2 || text.length > 50) return false;
    if (!/[a-zA-ZÀ-ž]/.test(text)) return false;
    if (/^\d+[:\-\.]\d+$/.test(text)) return false;
    if (/^\d+$/.test(text)) return false;
    if (/^(live|finished|after|half|set|quarter|period|inning|round|game|break|postp|cancel|FT|HT|AET|Pen|AP|ET|OT)$/i.test(text)) return false;
    return true;
  }

  /**
   * Check if element represents a LIVE match.
   */
  function isLiveElement(el) {
    const cls = (el.className || "").toLowerCase();
    const id = (el.id || "").toLowerCase();

    // FlashScore ID convention: g_1_ = live
    if (id.startsWith("g_1_")) return true;
    if (id.startsWith("g_2_") || id.startsWith("g_3_") || id.startsWith("g_4_")) return false;

    // Class-based
    if (cls.includes("--live") || cls.includes("live")) return true;

    // Content-based
    const txt = el.textContent;

    // Finished indicators (exclude these)
    if (/(finished|ended|FT|AET|Final|After OT|After Pen|Cancelled|Postponed|Awarded|Walkover|WO|Retired|Abandoned)/i.test(txt)) {
      // But check if it's just a small part — could have multiple matches
      const finEls = el.querySelectorAll('[class*="stage"], [class*="status"]');
      for (const fin of finEls) {
        if (/(finished|ended|FT|Final)/i.test(fin.textContent)) return false;
      }
    }

    // Live time indicators
    if (/\d+'/.test(txt)) return true; // Football minutes: "45'"
    if (/(1st|2nd|3rd|4th|5th)\s*(set|half|quarter|period|map|OT)/i.test(txt)) return true;
    if (/[12345]\.\s*(set|pol|čtvrt|mapa|třet)/i.test(txt)) return true;
    if (/přestávka|half[\s-]*time|break|pause/i.test(txt)) return true;

    // Clock/timer elements
    if (el.querySelector('[class*="clock"], [class*="timer"], [class*="stage--live"], [class*="blink"]')) return true;

    return false;
  }

  /**
   * Extract scores from a match container.
   */
  function extractScores(el) {
    let score1 = 0, score2 = 0;
    let status = "Live";

    // Priority 1: Elements with "score" in class
    const scoreEls = el.querySelectorAll('[class*="score"]');
    const scoreValues = [];
    for (const sel of scoreEls) {
      // Only leaf-ish elements
      if (sel.querySelectorAll('[class*="score"]').length > 0) continue;
      const num = parseInt(sel.textContent.trim());
      if (!isNaN(num) && num >= 0 && num < 999) {
        scoreValues.push(num);
      }
    }
    if (scoreValues.length >= 2) {
      score1 = scoreValues[0];
      score2 = scoreValues[1];
    }

    // Priority 2: Regex fallback
    if (score1 === 0 && score2 === 0) {
      const scoreMatch = el.textContent.match(/(\d{1,3})\s*[-:–]\s*(\d{1,3})/);
      if (scoreMatch) {
        score1 = parseInt(scoreMatch[1]) || 0;
        score2 = parseInt(scoreMatch[2]) || 0;
      }
    }

    // Status extraction
    const stageEl = el.querySelector('[class*="stage"], [class*="status"], [class*="period"]');
    if (stageEl) {
      const text = stageEl.textContent.trim();
      if (text.length > 0 && text.length < 50) {
        status = text;
      }
    }

    return { score1, score2, status };
  }

  function cleanTeamName(text) {
    if (!text) return "";
    return text
      .replace(/\(\d+\)/g, "")    // Remove seeding
      .replace(/\(W\)/gi, "")     // Remove women marker
      .replace(/^\d+\.\s*/, "")   // Remove ranking
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
  // SCAN LOOP
  // ====================================================================

  function startScanning() {
    stopScanning();
    doScan();
    scanTimer = setInterval(doScan, SCAN_INTERVAL_MS);
  }
  function stopScanning() { if (scanTimer) { clearInterval(scanTimer); scanTimer = null; } }

  function doScan() {
    const matches = scanAllLiveMatches();
    matchCount = matches.length;
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
    const pageSport = detectSportFromURL();

    const matchInfo = matches.length > 0
      ? matches.slice(0, 10).map(m =>
          `${m.team1} ${m.score1}-${m.score2} ${m.team2}`
        ).join("\n") + (matches.length > 10 ? `\n...+${matches.length - 10} more` : "")
      : "No live matches found";

    updatePanel(statusText, pageSport, matchInfo, sentCount);

    if (matches.length > 0) {
      const sportSummary = Object.entries(sportCounts).map(([s, c]) => `${s}:${c}`).join(", ");
      log(`Scan: ${matches.length} live [${sportSummary}], sent ${sentThisScan}`);
    } else {
      // Debug: report what DOM elements we see
      const g1 = document.querySelectorAll('[id^="g_1_"]').length;
      const g2 = document.querySelectorAll('[id^="g_2_"]').length;
      const evMatch = document.querySelectorAll('[class*="event__match"]').length;
      const partEl = document.querySelectorAll('[class*="participant"]').length;
      const scoreEl = document.querySelectorAll('[class*="score"]').length;
      dbg(`DOM: g_1_=${g1} g_2_=${g2} event__match=${evMatch} participant=${partEl} score=${scoreEl}`);
    }
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
    const pageSport = detectSportFromURL();
    log(`🏆 FlashScore Multi-Sport Scraper v3.0 (Generic DOM)`);
    log(`Page: ${window.location.href}`);
    log(`Detected sport: ${pageSport || "all/mixed"}`);
    log(`Strategy: g_1_ IDs → BEM classes → participant elements → link scan`);

    createPanel();
    connectWS();
    scheduleAutoRefresh();
  }

  if (document.readyState === "complete") {
    setTimeout(init, 1500);
  } else {
    window.addEventListener("load", () => setTimeout(init, 1500));
  }
})();
