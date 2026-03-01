// ==UserScript==
// @name         HLTV → Feed Hub Live Scraper
// @namespace    rustmisko
// @version      3.1
// @description  Scrapes live CS2 matches from HLTV + auto-refresh + SAFE odds extraction
// @author       RustMisko
// @match        https://www.hltv.org/matches*
// @match        https://www.hltv.org/live*
// @match        https://www.hltv.org/
// @match        https://hltv.org/matches*
// @match        https://hltv.org/live*
// @match        https://hltv.org/
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 2000; // 2s — CS2 rounds change fast, match scraper speed!
  const RECONNECT_MS = 3000;
  const HEARTBEAT_MS = 15000;
  const SOURCE_NAME = "hltv-tm";
  const DEBUG = true;

  // === AUTO-REFRESH CONFIG ===
  // HLTV DOM goes stale when matches end — periodic reload fixes it
  const AUTO_REFRESH_MS = 3 * 60 * 1000; // 3 minutes — full page reload
  const STALE_DETECT_MS = 90 * 1000; // 90s — if same data, refresh early
  const FINISHED_SCORE = 13; // CS2 match point — likely finished

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let lastScan = [];
  let scanTimer = null;
  let hbTimer = null;
  let refreshTimer = null;
  let refreshAt = 0; // timestamp when next refresh happens
  let lastScanHash = ""; // detect stale data
  let staleStartedAt = 0; // when we first detected stale data

  const PREFIX = "[HLTV→Hub]";
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
      border: 1px solid #0f0; min-width: 240px; opacity: 0.92;
      box-shadow: 0 0 20px rgba(0,255,0,0.15);
      cursor: move; user-select: none;
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom:6px; font-size:13px;">
        🎮 HLTV → Feed Hub v3
      </div>
      <div id="fh-status">⏳ Connecting...</div>
      <div id="fh-matches">Matches: –</div>
      <div id="fh-sent">Sent: 0</div>
      <div id="fh-last">Last scan: –</div>
      <div id="fh-refresh" style="color:#ff0;">🔄 Refresh: –</div>
      <div id="fh-detail" style="font-size:10px;color:#8f8;margin-top:4px;max-height:80px;overflow-y:auto;"></div>
      <div style="margin-top:6px; font-size:10px; color:#888;">
        <span>${WS_URL}</span>
      </div>
      <div style="margin-top:6px;">
        <button id="fh-btn-scan" style="background:#0a0;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Force Scan</button>
        <button id="fh-btn-refresh" style="background:#a80;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Refresh Now</button>
        <button id="fh-btn-debug" style="background:#333;color:#ff0;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;">DOM Debug</button>
      </div>
    `;
    document.body.appendChild(panel);

    let isDragging = false, offsetX, offsetY;
    panel.addEventListener("mousedown", (e) => {
      if (e.target.tagName === "BUTTON") return;
      isDragging = true;
      offsetX = e.clientX - panel.getBoundingClientRect().left;
      offsetY = e.clientY - panel.getBoundingClientRect().top;
    });
    document.addEventListener("mousemove", (e) => {
      if (!isDragging) return;
      panel.style.left = (e.clientX - offsetX) + "px";
      panel.style.top = (e.clientY - offsetY) + "px";
      panel.style.right = "auto";
      panel.style.bottom = "auto";
    });
    document.addEventListener("mouseup", () => { isDragging = false; });

    document.getElementById("fh-btn-scan").addEventListener("click", () => scanAndSend());
    document.getElementById("fh-btn-refresh").addEventListener("click", () => doPageRefresh("manual"));
    document.getElementById("fh-btn-debug").addEventListener("click", () => domDiscovery());
  }

  function updatePanel(status, matchCount, details) {
    const el = (id) => document.getElementById(id);
    if (el("fh-status")) {
      el("fh-status").textContent = status;
      el("fh-status").style.color = connected ? "#0f0" : "#f00";
    }
    if (matchCount !== undefined && el("fh-matches"))
      el("fh-matches").textContent = `Matches: ${matchCount}`;
    if (el("fh-sent")) el("fh-sent").textContent = `Sent: ${sentCount}`;
    if (el("fh-last")) el("fh-last").textContent = `Last scan: ${new Date().toLocaleTimeString()}`;
    if (details && el("fh-detail")) el("fh-detail").innerHTML = details;
  }

  // ====================================================================
  // DOM DISCOVERY
  // ====================================================================
  function domDiscovery() {
    log("=== DOM DISCOVERY START ===");
    const patterns = [
      ".liveMatch", "[class*='liveMatch']",
      "a[href*='/matches/']",
      ".matchTeamName", "[class*='matchTeamName']",
      "[class*='matchTeam']",
      ".currentMapScore", ".matchTeamScore",
      "[class*='Score']",
    ];
    for (const sel of patterns) {
      try {
        const els = document.querySelectorAll(sel);
        if (els.length > 0) {
          log(`✅ "${sel}" → ${els.length} elements`);
          els.forEach((el, i) => {
            if (i < 5) log(`   [${i}] tag=${el.tagName} class="${el.className}" text="${el.textContent.substring(0, 100).trim()}"`);
          });
        }
      } catch (e) {}
    }

    // Live match links
    const links = document.querySelectorAll("a[href*='/matches/']");
    log(`\nAll match links: ${links.length}`);
    const seen = new Set();
    for (const link of links) {
      if (seen.has(link.href)) continue;
      seen.add(link.href);
      const txt = link.textContent.replace(/\s+/g, " ").trim().substring(0, 120);
      const isLive = txt.includes("LIVE");
      if (isLive) log(`  🔴 ${link.href}\n     "${txt}"`);
    }
    log("=== DOM DISCOVERY END ===");
    alert("DOM Discovery done — check F12 Console");
  }

  // ====================================================================
  // SCORE EXTRACTION (SAFE)
  // ====================================================================
  function extractLinkScores(link) {
    const selectors = 'span, div, b, strong, em, i, small';

    function findNumericLeafIn(root) {
      if (!root) return null;
      // Prefer leaf elements with exact numeric text
      const candidates = root.querySelectorAll(selectors);
      for (const el of candidates) {
        if (el.children.length > 0) continue;
        const t = (el.textContent || '').trim();
        if (!/^\d{1,2}$/.test(t)) continue;
        const n = parseInt(t, 10);
        if (!Number.isFinite(n) || n < 0 || n > 99) continue;
        return n;
      }
      // Also allow the root itself if it's a simple numeric leaf
      if (root.children && root.children.length === 0) {
        const t = (root.textContent || '').trim();
        if (/^\d{1,2}$/.test(t)) {
          const n = parseInt(t, 10);
          if (Number.isFinite(n) && n >= 0 && n <= 99) return n;
        }
      }
      return null;
    }

    // Strategy 1 (best): find two team containers and extract one score per team.
    // This avoids picking e.g. map-count numbers (1-0) that can appear elsewhere.
    const teamContainers = link.querySelectorAll("[class*='matchTeam'], .matchTeam");
    if (teamContainers.length >= 2) {
      const s1 = findNumericLeafIn(teamContainers[0].querySelector(".matchTeamScore, [class*='matchTeamScore'], .currentMapScore, [class*='currentMapScore'], .score, [class*='Score'], [class*='score']") || teamContainers[0]);
      const s2 = findNumericLeafIn(teamContainers[1].querySelector(".matchTeamScore, [class*='matchTeamScore'], .currentMapScore, [class*='currentMapScore'], .score, [class*='Score'], [class*='score']") || teamContainers[1]);
      if (s1 !== null && s2 !== null) return { score1: s1, score2: s2 };
    }

    // Strategy 2: explicit score elements within the link.
    const directScoreEls = link.querySelectorAll(
      ".matchTeamScore, [class*='matchTeamScore'], .currentMapScore, [class*='currentMapScore'], .score, [class*='Score'], [class*='score']"
    );
    const nums = [];
    for (const el of directScoreEls) {
      const n = findNumericLeafIn(el);
      if (n !== null) nums.push(n);
      if (nums.length >= 2) break;
    }
    if (nums.length >= 2) return { score1: nums[0], score2: nums[1] };

    return { score1: 0, score2: 0 };
  }

  // ====================================================================
  // MATCH SCRAPING v2 — URL-based team extraction
  // ====================================================================
  function scrapeMatches() {
    const matches = [];
    const seen = new Set();

    const allLinks = document.querySelectorAll("a[href*='/matches/']");

    for (const link of allLinks) {
      const href = link.href;
      if (seen.has(href)) continue;

      // Parse URL: /matches/{id}/{team1}-vs-{team2}-{event}
      const urlMatch = href.match(/\/matches\/(\d+)\/(.+)/);
      if (!urlMatch) continue;

      const matchId = urlMatch[1];
      const slug = urlMatch[2];

      // Extract teams from URL slug: "mindshock-vs-aimclub-digital-crusade-..."
      const vsSplit = slug.split("-vs-");
      if (vsSplit.length < 2) continue;

      const team1FromUrl = vsSplit[0].replace(/-/g, " ");

      // team2 is everything after -vs- up to the event name
      // Event names usually have patterns like "season", "cup", "league", etc.
      const rest = vsSplit.slice(1).join("-vs-"); // handle rare case of "vs" in event name
      const eventCutoff = rest.search(
        /-(season|cup|league|qualifier|series|open|closed|major|minor|invitational|championship|tournament|finals|group|playoff|esl|iem|blast|cct|pgl|betboom|summit|rush|rushb|digital|nodwin|elisa|jb|exort|ukic|faceit|esea|dreamhack|weplay|pinnacle|perfect|world|bucharest|cracovia)/i
      );
      let team2FromUrl;
      if (eventCutoff > 0) {
        team2FromUrl = rest.substring(0, eventCutoff).replace(/-/g, " ");
      } else {
        team2FromUrl = rest.replace(/-/g, " ");
      }

      if (!team1FromUrl || !team2FromUrl) continue;

      // Check if LIVE
      const linkText = link.textContent || "";
      const isLive = linkText.includes("LIVE");

      // Try DOM-based team names (more accurate)
      let team1 = null, team2 = null;
      const teamNameEls = link.querySelectorAll(".matchTeamName, [class*='matchTeamName']");
      if (teamNameEls.length >= 2) {
        team1 = teamNameEls[0].textContent.trim();
        team2 = teamNameEls[1].textContent.trim();
      }

      // Validate DOM names — reject if they look like tournament names
      if (team1 && (team1.length > 30 || team1.toLowerCase().includes("season") || team1.toLowerCase().includes("league"))) {
        team1 = null;
      }
      if (team2 && (team2.length > 30 || team2.toLowerCase().includes("season") || team2.toLowerCase().includes("league"))) {
        team2 = null;
      }

      // Fallback to URL names
      if (!team1) team1 = capitalize(team1FromUrl);
      if (!team2) team2 = capitalize(team2FromUrl);

      // Extract map scores
      const { score1, score2 } = extractLinkScores(link);

      // Extract HLTV featured odds — SAFE version
      // Only look within the match link itself or 1 parent level up.
      // NEVER walk up 5 levels — that picks up odds from neighboring matches!
      let hltvOdds1 = null, hltvOdds2 = null;
      let hltvBookmaker = null;

      // Strategy: Look for odds WITHIN the link element first, then 1 parent up.
      // Validate: the container must NOT contain multiple match links (= not a parent section).
      const oddsSearchTargets = [link];
      if (link.parentElement) oddsSearchTargets.push(link.parentElement);
      if (link.parentElement && link.parentElement.parentElement)
        oddsSearchTargets.push(link.parentElement.parentElement);

      for (const container of oddsSearchTargets) {
        // Safety: if this container has multiple match links, skip it (too broad)
        const matchLinksInContainer = container.querySelectorAll("a[href*='/matches/']");
        if (matchLinksInContainer.length > 1) {
          dbg(`Skipping odds container (has ${matchLinksInContainer.length} match links)`);
          continue;
        }

        const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
        const foundOdds = [];
        while (walker.nextNode()) {
          const text = walker.currentNode.textContent.trim();
          if (/^\d+\.\d{2}$/.test(text)) {
            const val = parseFloat(text);
            if (val >= 1.01 && val <= 30.0) foundOdds.push(val);
          }
        }
        if (foundOdds.length === 2) {
          // Exactly 2 odds = perfect, assign team1/team2
          hltvOdds1 = foundOdds[0];
          hltvOdds2 = foundOdds[1];
          const containerText = container.textContent || "";
          if (containerText.toLowerCase().includes("20bet")) hltvBookmaker = "20bet";
          else if (containerText.toLowerCase().includes("ggbet")) hltvBookmaker = "ggbet";
          else if (containerText.toLowerCase().includes("1xbit")) hltvBookmaker = "1xbit";
          else if (containerText.toLowerCase().includes("betway")) hltvBookmaker = "betway";
          else hltvBookmaker = "hltv-featured";
          break;
        }
        // If >2 odds found, ambiguous — skip this level, don't use
        if (foundOdds.length > 2) {
          dbg(`Ambiguous odds (${foundOdds.length} numbers) in container, skipping`);
          continue;
        }
      }

      // Only include LIVE matches
      if (isLive || score1 > 0 || score2 > 0) {
        seen.add(href);
        matches.push({
          team1, team2, score1, score2,
          status: "LIVE", url: href, matchId,
          hltvOdds1, hltvOdds2, hltvBookmaker,
        });
      }
    }

    return matches;
  }

  function capitalize(s) {
    if (!s) return "";
    return s.split(" ").map(w => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
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
    return {
      v: 1, type: "live_match", source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: {
        sport: "cs2", team1: match.team1, team2: match.team2,
        score1: match.score1, score2: match.score2,
        status: match.status, url: match.url,
      },
    };
  }

  function buildOddsMessage(match) {
    if (!match.hltvOdds1 || !match.hltvOdds2) return null;
    const spread = Math.abs((1/match.hltvOdds1 + 1/match.hltvOdds2 - 1) * 100);
    return {
      v: 1, type: "odds", source: "hltv-odds",
      ts: new Date().toISOString(),
      payload: {
        sport: "cs2", bookmaker: match.hltvBookmaker || "hltv-featured", market: "match_winner",
        team1: match.team1, team2: match.team2,
        odds_team1: match.hltvOdds1, odds_team2: match.hltvOdds2,
        liquidity_usd: 5000.0,
        spread_pct: Math.round(spread * 100) / 100,
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
  // AUTO-REFRESH — HLTV DOM goes stale when matches end
  // ====================================================================
  function doPageRefresh(reason) {
    log(`🔄 Page refresh (${reason})...`);
    // Save state for post-reload panel
    try {
      sessionStorage.setItem("fh_refresh_reason", reason);
      sessionStorage.setItem("fh_refresh_time", Date.now().toString());
      sessionStorage.setItem("fh_sent_count", sentCount.toString());
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
    // Color: green when plenty of time, yellow under 60s, red under 15s
    if (remaining < 15) el.style.color = "#f00";
    else if (remaining < 60) el.style.color = "#ff0";
    else el.style.color = "#8f8";
  }

  // Start countdown display updater (every second)
  function startRefreshCountdown() {
    setInterval(updateRefreshCountdown, 1000);
  }

  // Smart stale detection: if scan data hasn't changed → refresh early
  function checkStaleData(matches) {
    // Build hash of current scan data
    const hash = matches.map(m =>
      `${m.team1}|${m.team2}|${m.score1}-${m.score2}`
    ).sort().join(";");

    if (hash === lastScanHash && hash.length > 0) {
      // Same data as before
      if (staleStartedAt === 0) {
        staleStartedAt = Date.now();
      } else if (Date.now() - staleStartedAt > STALE_DETECT_MS) {
        log("⚠️ Data stale for >90s — refreshing early");
        doPageRefresh("stale-data");
        return;
      }
    } else {
      // Data changed — reset stale timer
      staleStartedAt = 0;
      lastScanHash = hash;
    }

    // Detect likely-finished matches: any team at 13+
    for (const m of matches) {
      if (m.score1 >= FINISHED_SCORE || m.score2 >= FINISHED_SCORE) {
        // This match is probably over — mark data as potentially stale
        if (staleStartedAt === 0) staleStartedAt = Date.now();
        dbg(`⚠️ Match ${m.team1} vs ${m.team2} likely finished (${m.score1}-${m.score2})`);
      }
    }
  }

  // Recover state after reload
  function recoverPostReload() {
    try {
      const reason = sessionStorage.getItem("fh_refresh_reason");
      const prevSent = parseInt(sessionStorage.getItem("fh_sent_count") || "0");
      if (reason) {
        sentCount = prevSent;
        log(`🔄 Reloaded (reason: ${reason}), preserving sent count: ${sentCount}`);
        sessionStorage.removeItem("fh_refresh_reason");
        sessionStorage.removeItem("fh_refresh_time");
        sessionStorage.removeItem("fh_sent_count");
      }
    } catch (e) {}
  }

  // ====================================================================
  // SCAN LOOP (updated with stale detection)
  // ====================================================================
  function scanAndSend() {
    const matches = scrapeMatches();
    lastScan = matches;

    // Check for stale/finished matches → auto-refresh if needed
    checkStaleData(matches);

    let detailHtml = "";
    for (const m of matches) {
      const odds = m.hltvOdds1 ? ` [${m.hltvOdds1}/${m.hltvOdds2} ${m.hltvBookmaker||'?'}]` : "";
      detailHtml += `${m.team1} ${m.score1}-${m.score2} ${m.team2}${odds}<br>`;
    }
    updatePanel(connected ? "✅ Connected" : "❌ Disconnected", matches.length, detailHtml);

    if (matches.length === 0) { dbg("No live matches found"); return; }

    log(`Found ${matches.length} live match(es):`);
    for (const m of matches) {
      log(`  ${m.team1} ${m.score1}-${m.score2} ${m.team2}${m.hltvOdds1 ? ` odds:${m.hltvOdds1}/${m.hltvOdds2}` : ''}`);
      sendJSON(buildLiveMatchMessage(m));
      const oddsMsg = buildOddsMessage(m);
      if (oddsMsg) sendJSON(oddsMsg);
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
    log("Initializing v3 (auto-refresh)...");
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
