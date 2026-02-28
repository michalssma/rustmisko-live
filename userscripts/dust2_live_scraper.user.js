// ==UserScript==
// @name         Dust2.us → Feed Hub CS2 Live Scraper
// @namespace    rustmisko
// @version      1.0
// @description  Scrapes live CS2 matches from dust2.us (round + map scores) → Feed Hub WS
// @author       RustMisko
// @match        https://www.dust2.us/matches?filter=all*
// @match        https://www.dust2.us/matches?filter=all*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 2000; // 2s — CS2 rounds change fast, match scraper speed!
  const RECONNECT_MS = 3000;
  const HEARTBEAT_MS = 15000;
  const SOURCE_NAME = "dust2";
  const DEBUG = true;

  // === AUTO-REFRESH CONFIG ===
  const AUTO_REFRESH_MS = 5 * 60 * 1000; // 5 minutes
  const STALE_DETECT_MS = 90 * 1000; // 90s — if same data, refresh early
  const FINISHED_ROUND = 13; // CS2 match point — map likely done

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let lastScan = [];
  let scanTimer = null;
  let hbTimer = null;
  let refreshTimer = null;
  let refreshAt = 0;
  let lastScanHash = "";
  let staleStartedAt = 0;

  const PREFIX = "[Dust2→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // UI PANEL
  // ====================================================================
  function createPanel() {
    const panel = document.createElement("div");
    panel.id = "feedhub-panel";
    panel.style.cssText = `
      position: fixed; bottom: 10px; right: 10px; z-index: 999999;
      background: #1a1a2e; color: #0f0; font-family: 'Consolas', monospace;
      font-size: 12px; padding: 10px 14px; border-radius: 8px;
      border: 1px solid #0f0; min-width: 260px; opacity: 0.92;
      box-shadow: 0 0 20px rgba(0,255,0,0.15);
      cursor: move; user-select: none;
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom:4px;">🎯 Dust2 → Feed Hub</div>
      <div id="fh-status">⏳ Starting...</div>
      <div id="fh-matches" style="color:#8f8; margin-top:4px;">Matches: 0</div>
      <div id="fh-refresh" style="color:#8f8; margin-top:2px;">🔄 Refresh: --:--</div>
      <div id="fh-sent" style="color:#888; margin-top:2px;">Sent: 0</div>
      <div id="fh-detail" style="color:#aaa; margin-top:4px; font-size:11px; max-height:120px; overflow-y:auto;"></div>
    `;
    document.body.appendChild(panel);

    // Draggable
    let dragging = false, dx = 0, dy = 0;
    panel.addEventListener("mousedown", (e) => {
      dragging = true;
      dx = e.clientX - panel.offsetLeft;
      dy = e.clientY - panel.offsetTop;
    });
    document.addEventListener("mousemove", (e) => {
      if (!dragging) return;
      panel.style.left = (e.clientX - dx) + "px";
      panel.style.top = (e.clientY - dy) + "px";
      panel.style.right = "auto";
      panel.style.bottom = "auto";
    });
    document.addEventListener("mouseup", () => { dragging = false; });
  }

  function updatePanel(status, matchCount, detailHtml) {
    const el = document.getElementById("fh-status");
    if (el) el.textContent = status;
    const mc = document.getElementById("fh-matches");
    if (mc && matchCount !== undefined) mc.textContent = `Matches: ${matchCount}`;
    const sc = document.getElementById("fh-sent");
    if (sc) sc.textContent = `Sent: ${sentCount}`;
    const dt = document.getElementById("fh-detail");
    if (dt && detailHtml) dt.innerHTML = detailHtml;
  }

  // ====================================================================
  // TEAM NAME NORMALIZATION
  // ====================================================================
  function normalizeTeam(raw) {
    if (!raw) return "";
    return raw
      .trim()
      .replace(/\s+/g, " ") // collapse whitespace
      .replace(/^Team\s+/i, "") // strip "Team " prefix optionally
      .split(" ")
      .map(w => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase())
      .join(" ");
  }

  function slugifyTeam(name) {
    return name
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "")
      .trim();
  }

  // ====================================================================
  // DOM SCRAPING — dust2.us/matches
  // ====================================================================
  //
  // dust2.us uses HLTV-powered data. The matches page has:
  //   1. Top ticker bar with live match cards
  //   2. "Live CS2 matches" section in the main content
  //
  // Match links follow pattern: /matches/{id}/{team1-vs-team2}
  // Scores appear as text like "6 (1)" = 6 rounds, 1 map
  //
  // Strategy: find all <a> links pointing to /matches/{id}/{slug}
  // that are inside or near a "Live" indicator, extract team names
  // from the slug, and parse round/map scores from surrounding text.

  function scrapeMatches() {
    const matches = [];
    const seen = new Set();

    // APPROACH: Find the "Live CS2 matches" section first, then only
    // process links INSIDE that section. This avoids false-matching
    // upcoming matches that sit in a different section on the same page.
    const liveLinks = collectLiveLinks();
    dbg(`collectLiveLinks returned ${liveLinks.length} link(s)`);

    for (const link of liveLinks) {
      const href = link.getAttribute("href") || "";
      const urlMatch = href.match(/\/matches\/(\d+)\/([\w-]+)/);
      if (!urlMatch) continue;

      const matchId = urlMatch[1];
      if (seen.has(matchId)) continue;

      const slug = urlMatch[2];
      const teams = extractTeamsFromSlug(slug);
      if (!teams) continue;

      const scores = extractScores(link);

      seen.add(matchId);
      matches.push({
        team1: teams.team1,
        team2: teams.team2,
        score1: scores.round1,
        score2: scores.round2,
        mapScore1: scores.map1,
        mapScore2: scores.map2,
        status: "LIVE",
        url: href.startsWith("http") ? href : "https://www.dust2.us" + href,
        matchId,
      });
    }

    return matches;
  }

  // Collect match links that are actually LIVE.
  // Two approaches combined:
  //   A) Find the "Live CS2 matches" heading → take its parent container → grab links inside
  //   B) Fallback: any <a> whose text contains a score pattern N(M) (ticker bar cards)
  function collectLiveLinks() {
    const result = [];
    const seenHref = new Set();

    // --- A) Section-based: find heading with "Live" ---
    const headings = document.querySelectorAll('h1, h2, h3, h4, h5, h6');
    for (const h of headings) {
      const txt = (h.textContent || "").trim();
      if (!/\bLive\b/i.test(txt)) continue;
      // The parent (or grandparent) of this heading is the section container
      // with the live match cards.
      let container = h.parentElement;
      // If the heading has a very small parent (just a wrapper), step one more level up
      if (container && container.querySelectorAll('a[href*="/matches/"]').length === 0) {
        container = container.parentElement;
      }
      if (!container) continue;
      const links = container.querySelectorAll('a[href*="/matches/"]');
      for (const l of links) {
        const href = l.getAttribute("href") || "";
        if (!seenHref.has(href)) { seenHref.add(href); result.push(l); }
      }
      dbg(`Live section "${txt}" → ${links.length} link(s)`);
    }

    // --- B) Fallback: any link with score pattern in its text (e.g., top ticker) ---
    const allLinks = document.querySelectorAll('a[href*="/matches/"]');
    for (const link of allLinks) {
      const href = link.getAttribute("href") || "";
      if (seenHref.has(href)) continue;
      const linkText = link.textContent || "";
      if (/\d+\s*\(\d+\)/.test(linkText)) {
        seenHref.add(href);
        result.push(link);
      }
    }

    return result;
  }

  // Extract team names from URL slug like "red-canids-vs-sharks"
  function extractTeamsFromSlug(slug) {
    const vsSplit = slug.split("-vs-");
    if (vsSplit.length !== 2) return null;

    const team1 = vsSplit[0].split("-").map(w => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
    const team2 = vsSplit[1].split("-")
      // Remove trailing tournament qualifiers (after a numbered suffix often)
      .filter(w => w.length > 0)
      .map(w => w.charAt(0).toUpperCase() + w.slice(1))
      .join(" ");

    if (team1.length < 2 || team2.length < 2) return null;

    return { team1, team2 };
  }

  // Extract round scores and map scores from the link element itself.
  // On dust2.us each live match card is a single <a> wrapping everything.
  // Scores appear as "N (M)" where N=round score, M=map score.
  // Example: "RED Canids 6 (1) Sharks 10 (1)"
  function extractScores(link) {
    const result = { round1: 0, round2: 0, map1: 0, map2: 0 };

    // IMPORTANT: Search within the link element itself, NOT a broad parent.
    // This prevents mixing scores from different match cards.
    const text = link.textContent || "";

    // Primary pattern: "N (M)" or "N(M)" — round score with map score in parentheses
    const scorePattern = /(\d{1,2})\s*\((\d{1,2})\)/g;
    const allScores = [];
    let m;
    while ((m = scorePattern.exec(text)) !== null) {
      allScores.push({ round: parseInt(m[1]), map: parseInt(m[2]) });
    }

    if (allScores.length >= 2) {
      result.round1 = allScores[0].round;
      result.map1 = allScores[0].map;
      result.round2 = allScores[1].round;
      result.map2 = allScores[1].map;
      return result;
    } else if (allScores.length === 1) {
      result.round1 = allScores[0].round;
      result.map1 = allScores[0].map;
    }

    // Fallback: look for standalone numbers within the link text nodes
    if (allScores.length === 0) {
      const plainScores = [];
      const walker = document.createTreeWalker(link, NodeFilter.SHOW_TEXT);
      while (walker.nextNode()) {
        const t = walker.currentNode.textContent.trim();
        if (/^\d{1,2}$/.test(t)) {
          const n = parseInt(t);
          if (n >= 0 && n <= 50) plainScores.push(n);
        }
      }
      if (plainScores.length >= 2) {
        result.round1 = plainScores[0];
        result.round2 = plainScores[1];
      }
    }

    return result;
  }

  // ====================================================================
  // WEBSOCKET
  // ====================================================================
  function connectWS() {
    if (ws && (ws.readyState === WebSocket.CONNECTING || ws.readyState === WebSocket.OPEN)) return;
    log(`Connecting to ${WS_URL}...`);
    updatePanel("⏳ Connecting...");
    try { ws = new WebSocket(WS_URL); } catch (e) {
      log("WS error:", e); updatePanel("❌ Error"); scheduleReconnect(); return;
    }
    ws.onopen = () => {
      connected = true; log("✅ Connected");
      updatePanel("✅ Connected");
      sendHeartbeat(); startScanning(); startHeartbeat();
    };
    ws.onmessage = (e) => { dbg("← Hub:", e.data); };
    ws.onclose = (e) => {
      connected = false; log(`Closed (${e.code})`);
      updatePanel("❌ Disconnected"); stopScanning(); stopHeartbeat(); scheduleReconnect();
    };
    ws.onerror = () => { connected = false; updatePanel("❌ Error"); };
  }

  function scheduleReconnect() { setTimeout(connectWS, RECONNECT_MS); }

  function sendJSON(obj) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify(obj)); sentCount++; return true;
  }

  // ====================================================================
  // FEED MESSAGES
  // ====================================================================
  function buildLiveMatchMessage(match) {
    // Use map score (Bo series score) as the primary score in the feed,
    // and include round-level detail in detailed_score.
    // This matches how HLTV scraper and feed_hub expect CS2 scores.
    const detailedScore = `R:${match.score1}-${match.score2} M:${match.mapScore1}-${match.mapScore2}`;
    return {
      v: 1, type: "live_match", source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: {
        sport: "cs2",
        team1: match.team1,
        team2: match.team2,
        score1: match.mapScore1,  // Map/series score → primary
        score2: match.mapScore2,
        detailed_score: detailedScore,
        status: match.status,
        url: match.url,
      },
    };
  }

  function sendHeartbeat() {
    sendJSON({
      v: 1, type: "heartbeat", source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: { page: window.location.pathname, matches_found: lastScan.length },
    });
  }

  // ====================================================================
  // AUTO-REFRESH
  // ====================================================================
  function doPageRefresh(reason) {
    log(`🔄 Page refresh (${reason})...`);
    try {
      sessionStorage.setItem("d2_refresh_reason", reason);
      sessionStorage.setItem("d2_refresh_time", Date.now().toString());
      sessionStorage.setItem("d2_sent_count", sentCount.toString());
    } catch (e) {}
    location.reload();
  }

  function scheduleAutoRefresh() {
    if (refreshTimer) clearTimeout(refreshTimer);
    refreshAt = Date.now() + AUTO_REFRESH_MS;
    refreshTimer = setTimeout(() => doPageRefresh("auto-timer"), AUTO_REFRESH_MS);
    log(`🔄 Auto-refresh scheduled in ${AUTO_REFRESH_MS / 1000}s`);
  }

  function updateRefreshCountdown() {
    const el = document.getElementById("fh-refresh");
    if (!el) return;
    const remaining = Math.max(0, Math.round((refreshAt - Date.now()) / 1000));
    const mins = Math.floor(remaining / 60);
    const secs = remaining % 60;
    el.textContent = `🔄 Refresh: ${mins}:${secs.toString().padStart(2, "0")}`;
    if (remaining < 15) el.style.color = "#f00";
    else if (remaining < 60) el.style.color = "#ff0";
    else el.style.color = "#8f8";
  }

  function startRefreshCountdown() {
    setInterval(updateRefreshCountdown, 1000);
  }

  // Stale detection
  function checkStaleData(matches) {
    const hash = matches.map(m =>
      `${m.team1}|${m.team2}|${m.score1}-${m.score2}|${m.mapScore1}-${m.mapScore2}`
    ).sort().join(";");

    if (hash === lastScanHash && hash.length > 0) {
      if (staleStartedAt === 0) {
        staleStartedAt = Date.now();
      } else if (Date.now() - staleStartedAt > STALE_DETECT_MS) {
        log("⚠️ Data stale for >90s — refreshing early");
        doPageRefresh("stale-data");
        return;
      }
    } else {
      staleStartedAt = 0;
      lastScanHash = hash;
    }

    for (const m of matches) {
      if (m.score1 >= FINISHED_ROUND || m.score2 >= FINISHED_ROUND) {
        if (staleStartedAt === 0) staleStartedAt = Date.now();
        dbg(`⚠️ Match ${m.team1} vs ${m.team2} likely finished map (${m.score1}-${m.score2})`);
      }
    }
  }

  function recoverPostReload() {
    try {
      const reason = sessionStorage.getItem("d2_refresh_reason");
      const prevSent = parseInt(sessionStorage.getItem("d2_sent_count") || "0");
      if (reason) {
        sentCount = prevSent;
        log(`🔄 Reloaded (reason: ${reason}), preserving sent count: ${sentCount}`);
        sessionStorage.removeItem("d2_refresh_reason");
        sessionStorage.removeItem("d2_refresh_time");
        sessionStorage.removeItem("d2_sent_count");
      }
    } catch (e) {}
  }

  // ====================================================================
  // SCAN LOOP
  // ====================================================================
  function scanAndSend() {
    const matches = scrapeMatches();
    lastScan = matches;

    checkStaleData(matches);

    let detailHtml = "";
    for (const m of matches) {
      detailHtml += `${m.team1} R:${m.score1}-${m.score2} M:${m.mapScore1}-${m.mapScore2} ${m.team2}<br>`;
    }
    updatePanel(connected ? "✅ Connected" : "❌ Disconnected", matches.length, detailHtml);

    if (matches.length === 0) { dbg("No live matches found"); return; }

    log(`Found ${matches.length} live match(es):`);
    for (const m of matches) {
      log(`  ${m.team1} Round:${m.score1}-${m.score2} Map:${m.mapScore1}-${m.mapScore2} ${m.team2}`);
      sendJSON(buildLiveMatchMessage(m));
    }
  }

  function startScanning() {
    stopScanning(); scanAndSend();
    scanTimer = setInterval(scanAndSend, SCAN_INTERVAL_MS);
  }
  function stopScanning() { if (scanTimer) { clearInterval(scanTimer); scanTimer = null; } }
  function startHeartbeat() { stopHeartbeat(); hbTimer = setInterval(sendHeartbeat, HEARTBEAT_MS); }
  function stopHeartbeat() { if (hbTimer) { clearInterval(hbTimer); hbTimer = null; } }

  // ====================================================================
  // INIT
  // ====================================================================
  function init() {
    log("Initializing v1.0...");
    log(`Page: ${window.location.href}`);
    recoverPostReload();
    setTimeout(() => {
      createPanel();
      connectWS();
      scheduleAutoRefresh();
      startRefreshCountdown();
    }, 2000);
  }

  init();
})();
