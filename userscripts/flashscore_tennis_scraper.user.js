// ==UserScript==
// @name         FlashScore → Feed Hub Tennis Scraper
// @namespace    rustmisko
// @version      1.0
// @description  Scrapes live tennis matches from FlashScore, sends to Feed Hub via WebSocket
// @author       RustMisko
// @match        https://www.flashscore.com/*
// @match        https://www.flashscore.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 8000; // 8s — tennis points change slower than CS2 rounds
  const RECONNECT_MS = 3000;
  const HEARTBEAT_MS = 15000;
  const SOURCE_NAME = "flashscore-tennis";
  const DEBUG = true;

  // Auto-refresh to keep DOM fresh
  const AUTO_REFRESH_MS = 5 * 60 * 1000; // 5 minutes

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let scanTimer = null;
  let hbTimer = null;
  let lastScanHash = "";

  const PREFIX = "[Tennis→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // UI PANEL
  // ====================================================================
  function createPanel() {
    const panel = document.createElement("div");
    panel.id = "tennis-feedhub-panel";
    panel.style.cssText = `
      position: fixed; bottom: 10px; left: 10px; z-index: 999999;
      background: #1a1a2e; color: #0fa; font-family: 'Consolas', monospace;
      font-size: 12px; padding: 10px 14px; border-radius: 8px;
      border: 1px solid #0fa; min-width: 260px; opacity: 0.92;
      box-shadow: 0 0 20px rgba(0,255,170,0.15);
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom: 4px;">🎾 Tennis → Feed Hub</div>
      <div id="tf-status" style="color: #fa0;">⏳ Connecting...</div>
      <div id="tf-matches" style="margin-top: 4px; color: #aaa;">Scanning...</div>
      <div id="tf-sent" style="margin-top: 2px; color: #888;">Sent: 0</div>
    `;
    document.body.appendChild(panel);
    return panel;
  }

  function updatePanel(status, matches, sent) {
    const el = (id) => document.getElementById(id);
    if (el("tf-status")) el("tf-status").textContent = status;
    if (el("tf-matches")) el("tf-matches").textContent = matches;
    if (el("tf-sent")) el("tf-sent").textContent = `Sent: ${sent}`;
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
      updatePanel("✅ Connected", "Scanning...", sentCount);
      startScanning();
      startHeartbeat();
    };

    ws.onclose = (e) => {
      connected = false;
      log("❌ Disconnected:", e.code, e.reason);
      updatePanel("❌ Disconnected", "Reconnecting...", sentCount);
      stopScanning();
      stopHeartbeat();
      setTimeout(connectWS, RECONNECT_MS);
    };

    ws.onerror = (e) => {
      log("WS error:", e);
    };

    ws.onmessage = (e) => {
      dbg("Server:", e.data);
    };
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

  function stopHeartbeat() {
    if (hbTimer) { clearInterval(hbTimer); hbTimer = null; }
  }

  // ====================================================================
  // SCANNING — FlashScore DOM extraction
  // ====================================================================

  /**
   * Extracts live tennis matches from FlashScore's DOM.
   * FlashScore uses classes like:
   *   .event__match--live — live match row
   *   .event__participant — participant names
   *   .event__score — game scores
   *   .event__part — set scores
   *
   * The page structure can vary; we try multiple selectors.
   */
  function scanLiveTennisMatches() {
    const matches = [];

    // Strategy 1: Modern FlashScore layout (2024+)
    // Live matches are in sections with specific data attributes
    const liveRows = document.querySelectorAll(
      '.event__match--live, .event__match--oneLine--live, [class*="event__match"][class*="live"]'
    );

    for (const row of liveRows) {
      try {
        const match = extractMatchFromRow(row);
        if (match && match.isTennis) {
          matches.push(match);
        }
      } catch (e) {
        dbg("Error extracting match:", e);
      }
    }

    // Strategy 2: If no live rows found, check for generic match rows on tennis page
    if (matches.length === 0 && isTennisPage()) {
      const allRows = document.querySelectorAll('.event__match, [id^="g_1_"]');
      for (const row of allRows) {
        try {
          // Check if match has live indicators
          if (isLiveRow(row)) {
            const match = extractMatchFromRow(row);
            if (match) {
              match.isTennis = true; // We're on tennis page
              matches.push(match);
            }
          }
        } catch (e) {
          dbg("Error extracting match (strategy 2):", e);
        }
      }
    }

    return matches;
  }

  function isTennisPage() {
    const url = window.location.href.toLowerCase();
    const breadcrumb = document.querySelector('.breadcrumb, .heading__title, [class*="heading"]');
    const breadcrumbText = breadcrumb ? breadcrumb.textContent.toLowerCase() : "";
    return url.includes("tennis") || breadcrumbText.includes("tennis") || breadcrumbText.includes("tenis");
  }

  function isLiveRow(row) {
    // Check for live indicators in class names or child elements
    const classes = row.className || "";
    if (classes.includes("live")) return true;

    // Check for live stage text
    const stageEl = row.querySelector('.event__stage, .event__stage--live, [class*="stage"]');
    if (stageEl) {
      const text = stageEl.textContent.trim().toLowerCase();
      if (text && !text.includes("finished") && !text.includes("ended") && !text.includes("final")) {
        return true;
      }
    }

    // Check for timer/clock icon indicating live
    const timerEl = row.querySelector('.icon--clock, .eventTimer, [class*="timer"], [class*="clock"]');
    if (timerEl) return true;

    return false;
  }

  function extractMatchFromRow(row) {
    // Team names
    const participants = row.querySelectorAll(
      '.event__participant, [class*="participant"], .event__participant--home, .event__participant--away'
    );
    if (participants.length < 2) return null;

    const team1 = cleanTeamName(participants[0].textContent);
    const team2 = cleanTeamName(participants[1].textContent);
    if (!team1 || !team2) return null;

    // Check if tennis by looking at tournament name or page context
    const headerEl = row.closest('.sportName, [class*="sportName"]') ||
                     row.closest('.event__header, [class*="header"]');
    const headerText = headerEl ? headerEl.textContent.toLowerCase() : "";
    const isTennis = isTennisPage() || headerText.includes("tennis") || headerText.includes("tenis");

    // Set scores — FlashScore shows set scores in .event__part elements
    const partEls = row.querySelectorAll('.event__part, [class*="event__part"]');
    let sets1 = 0, sets2 = 0;
    const setScores = [];

    if (partEls.length >= 2) {
      // Parts come in pairs: home score, away score for each set
      // First check the main score (total sets won)
      const scoreEls = row.querySelectorAll('.event__score, [class*="event__score"]');
      if (scoreEls.length >= 2) {
        sets1 = parseInt(scoreEls[0].textContent.trim()) || 0;
        sets2 = parseInt(scoreEls[1].textContent.trim()) || 0;
      }

      // Individual set scores
      for (let i = 0; i < partEls.length; i += 2) {
        if (i + 1 < partEls.length) {
          const s1 = parseInt(partEls[i].textContent.trim()) || 0;
          const s2 = parseInt(partEls[i + 1].textContent.trim()) || 0;
          setScores.push(`${s1}-${s2}`);
        }
      }
    } else {
      // Try main score elements directly
      const scoreEls = row.querySelectorAll('.event__score, .event__scores, [class*="score"]');
      if (scoreEls.length >= 2) {
        sets1 = parseInt(scoreEls[0].textContent.trim()) || 0;
        sets2 = parseInt(scoreEls[1].textContent.trim()) || 0;
      }
    }

    // Game status (set in progress, etc.)
    const stageEl = row.querySelector('.event__stage, .event__stage--live, [class*="stage"]');
    const status = stageEl ? stageEl.textContent.trim() : "Live";

    // Match ID from row
    const matchId = row.id || row.getAttribute("data-id") || `${team1}_${team2}`;

    return {
      team1,
      team2,
      sets1,    // Sets won by player 1
      sets2,    // Sets won by player 2
      setScores, // Individual set scores: ["6-3", "4-2"]
      status,
      isTennis,
      matchId,
    };
  }

  function cleanTeamName(text) {
    if (!text) return "";
    return text
      .replace(/\(\d+\)/g, "")  // Remove seeding like "(1)"
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
        sport: "tennis",
        team1: match.team1,
        team2: match.team2,
        score1: match.sets1,   // SET count (0-2)
        score2: match.sets2,
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

  function stopScanning() {
    if (scanTimer) { clearInterval(scanTimer); scanTimer = null; }
  }

  function doScan() {
    const matches = scanLiveTennisMatches();
    const hash = JSON.stringify(matches.map(m => `${m.team1}:${m.team2}:${m.sets1}:${m.sets2}`));

    let sentThisScan = 0;

    for (const match of matches) {
      const msg = buildLiveMatchMessage(match);
      if (sendJSON(msg)) {
        sentThisScan++;
        dbg(`Sent: ${match.team1} ${match.sets1}-${match.sets2} ${match.team2}`);
      }
    }

    const statusText = connected ? "✅ Connected" : "❌ Disconnected";
    const matchInfo = matches.length > 0
      ? matches.map(m => `${m.team1} ${m.sets1}-${m.sets2} ${m.team2}`).join(" | ")
      : "No live tennis matches";

    updatePanel(statusText, matchInfo, sentCount);

    if (matches.length > 0 || hash !== lastScanHash) {
      log(`Scan: ${matches.length} live tennis, sent ${sentThisScan}`);
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
    log("🎾 FlashScore Tennis Scraper v1.0");
    log("Page:", window.location.href);
    log("Is tennis page:", isTennisPage());

    createPanel();
    connectWS();
    scheduleAutoRefresh();

    // If we're on the main FlashScore page, try to navigate to tennis
    if (!isTennisPage()) {
      log("⚠️ Not on tennis page. Navigate to FlashScore Tennis for best results.");
      log("  Recommended: https://www.flashscore.com/tennis/");
      updatePanel("⚠️ Navigate to Tennis", "flashscore.com/tennis/", 0);
    }
  }

  // Wait for page to settle
  if (document.readyState === "complete") {
    setTimeout(init, 1500);
  } else {
    window.addEventListener("load", () => setTimeout(init, 1500));
  }
})();
