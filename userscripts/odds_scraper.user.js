// ==UserScript==
// @name         Odds → Feed Hub Scraper
// @namespace    rustmisko
// @version      3.0
// @description  Scrapes CS2 betting odds from bo3.gg, oddspedia, and other sites
// @author       RustMisko
// @match        https://www.oddspedia.com/*
// @match        https://oddspedia.com/*
// @match        https://www.strafe.com/esports/*
// @match        https://strafe.com/esports/*
// @match        https://www.oddsportal.com/esports/*
// @match        https://oddsportal.com/esports/*
// @match        https://esport.cz/*
// @match        https://www.bo3.gg/*
// @match        https://bo3.gg/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 4000; // 4s — faster odds detection for arb/anomaly
  const RECONNECT_MS = 3000;
  const HEARTBEAT_MS = 20000;
  const SOURCE_NAME = "odds-tm";
  const DEBUG = true;

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let lastScan = [];
  let scanTimer = null;
  let hbTimer = null;

  const PREFIX = "[Odds→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // UI PANEL
  // ====================================================================
  function createPanel() {
    const panel = document.createElement("div");
    panel.id = "oddshub-panel";
    panel.style.cssText = `
      position: fixed; bottom: 10px; left: 10px; z-index: 999999;
      background: #1a1a2e; color: #ff0; font-family: 'Consolas', monospace;
      font-size: 12px; padding: 10px 14px; border-radius: 8px;
      border: 1px solid #ff0; min-width: 240px; opacity: 0.92;
      box-shadow: 0 0 20px rgba(255,255,0,0.15);
      cursor: move; user-select: none;
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom:6px; font-size:13px;">
        💰 Odds → Feed Hub v3
      </div>
      <div id="oh-status">⏳ Connecting...</div>
      <div id="oh-odds">Odds rows: –</div>
      <div id="oh-sent">Sent: 0</div>
      <div id="oh-site">Site: ${detectSite()}</div>
      <div id="oh-detail" style="font-size:10px;color:#ff8;margin-top:4px;max-height:100px;overflow-y:auto;"></div>
      <div style="margin-top:6px;">
        <button id="oh-btn-scan" style="background:#aa0;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Force Scan</button>
        <button id="oh-btn-debug" style="background:#333;color:#ff0;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;">DOM Debug</button>
      </div>
    `;
    document.body.appendChild(panel);

    let isDragging = false, ox, oy;
    panel.addEventListener("mousedown", (e) => {
      if (e.target.tagName === "BUTTON") return;
      isDragging = true;
      ox = e.clientX - panel.getBoundingClientRect().left;
      oy = e.clientY - panel.getBoundingClientRect().top;
    });
    document.addEventListener("mousemove", (e) => {
      if (!isDragging) return;
      panel.style.left = (e.clientX - ox) + "px";
      panel.style.top = (e.clientY - oy) + "px";
      panel.style.right = "auto"; panel.style.bottom = "auto";
    });
    document.addEventListener("mouseup", () => { isDragging = false; });

    document.getElementById("oh-btn-scan").addEventListener("click", scanAndSend);
    document.getElementById("oh-btn-debug").addEventListener("click", domDiscovery);
  }

  function updatePanel(status, oddsCount, details) {
    const el = (id) => document.getElementById(id);
    if (el("oh-status")) {
      el("oh-status").textContent = status;
      el("oh-status").style.color = connected ? "#0f0" : "#f00";
    }
    if (oddsCount !== undefined && el("oh-odds"))
      el("oh-odds").textContent = `Odds rows: ${oddsCount}`;
    if (el("oh-sent")) el("oh-sent").textContent = `Sent: ${sentCount}`;
    if (details && el("oh-detail")) el("oh-detail").innerHTML = details;
  }

  function detectSite() {
    const host = window.location.hostname;
    if (host.includes("oddspedia")) return "oddspedia";
    if (host.includes("strafe")) return "strafe";
    if (host.includes("oddsportal")) return "oddsportal";
    if (host.includes("bo3.gg")) return "bo3gg";
    if (host.includes("esport.cz")) return "esportcz";
    return "unknown";
  }

  // ====================================================================
  // DOM DISCOVERY — dumps DOM structure to console for debugging
  // ====================================================================
  function domDiscovery() {
    log("=== ODDS DOM DISCOVERY v3 ===");
    log(`Site: ${detectSite()}, URL: ${window.location.href}`);

    // Find all text nodes that look like odds
    const oddsTexts = findOddsTextNodes(document.body);
    log(`Odds-like text nodes: ${oddsTexts.length}`);
    oddsTexts.forEach((o, i) => {
      if (i < 20) {
        const parent = o.textNode.parentElement;
        log(`  [${i}] val=${o.val} parent=<${parent.tagName}.${parent.className}> grandparent=<${parent.parentElement?.tagName}.${parent.parentElement?.className}>`);
      }
    });

    // Match links
    const links = document.querySelectorAll("a[href*='/matches/']");
    log(`Match links: ${links.length}`);
    links.forEach((l, i) => {
      if (i < 15) log(`  [${i}] href="${l.href}" text="${l.textContent.replace(/\s+/g,' ').trim().substring(0,80)}"`);
    });

    log("=== DOM DISCOVERY END ===");
    alert("DOM Discovery v3 done — check F12 Console");
  }

  // ====================================================================
  // SCRAPING — SITE-SPECIFIC
  // ====================================================================
  function scrapeOdds() {
    const site = detectSite();
    dbg(`Scraping ${site} v3...`);
    switch (site) {
      case "bo3gg": return scrapeBo3gg();
      case "oddspedia": return scrapeOddspedia();
      default: return [];
    }
  }

  // ====================================================================
  // FIND ODDS TEXT NODES — core utility
  // Walks the DOM tree and finds TEXT NODES whose content is EXACTLY
  // a decimal number in odds range (e.g. "1.82", "3.20")
  // This avoids picking up numbers from larger text blocks
  // ====================================================================
  function findOddsTextNodes(root) {
    const results = [];
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    while (walker.nextNode()) {
      const text = walker.currentNode.textContent.trim();
      // Must be EXACTLY a decimal number: 1.82, 12.50, 3.2, etc.
      if (/^\d+\.\d{1,2}$/.test(text)) {
        const val = parseFloat(text);
        if (val >= 1.01 && val <= 30.0) {
          results.push({ textNode: walker.currentNode, el: walker.currentNode.parentElement, val });
        }
      }
    }
    return results;
  }

  // ====================================================================
  // BO3.GG SCRAPER v3 — Bottom-up approach
  //
  // Strategy:
  // 1. Find ALL odds-like text nodes on the page (leaf decimal numbers)
  // 2. For each odds node, walk UP to find the nearest match link
  // 3. Group odds by their associated match
  // 4. Extract team names from match URL (reliable, no DOM override)
  // 5. Clean up team name suffixes (-cs, -es, -cs-go, etc.)
  // ====================================================================
  function scrapeBo3gg() {
    const rows = [];

    // Step 1: Find all odds text nodes
    const oddsNodes = findOddsTextNodes(document.body);
    dbg(`Bo3.gg: found ${oddsNodes.length} odds text nodes`);

    // Step 2: For each odds node, find the closest match link
    // by walking up the DOM tree from the odds element
    for (const node of oddsNodes) {
      let el = node.el;
      let matchLink = null;
      for (let i = 0; i < 10 && el; i++) {
        // Look for match links WITHIN this element
        const links = el.querySelectorAll("a[href*='/matches/']");
        if (links.length > 0) {
          // Take the first one — this is the match this odds belongs to
          // But ONLY if there's exactly 1 unique match URL in this container
          // (prevents picking from a container with multiple matches)
          const unique = new Set(Array.from(links).map(l => l.href));
          if (unique.size === 1) {
            matchLink = links[0];
            break;
          } else if (unique.size > 1) {
            // Container has multiple matches — find the CLOSEST one
            // by checking which link is nearest in DOM position
            matchLink = findClosestLink(node.el, links);
            break;
          }
        }
        el = el.parentElement;
      }
      node.matchHref = matchLink ? matchLink.href : null;
    }

    // Step 3: Group odds by match URL
    const byMatch = {};
    for (const node of oddsNodes) {
      if (!node.matchHref) continue;
      if (!byMatch[node.matchHref]) byMatch[node.matchHref] = [];
      byMatch[node.matchHref].push(node.val);
    }

    dbg(`Bo3.gg: grouped odds for ${Object.keys(byMatch).length} matches`);

    // Step 4: Build result rows
    const processed = new Set();
    for (const [href, oddsVals] of Object.entries(byMatch)) {
      if (oddsVals.length < 2) continue;
      if (processed.has(href)) continue;
      processed.add(href);

      // Parse team names from URL
      const teams = parseTeamsFromBo3ggUrl(href);
      if (!teams) continue;

      // Determine LIVE status by checking if any match link has LIVE nearby
      let isLive = false;
      try {
        const linkEl = document.querySelector(`a[href="${href}"]`);
        if (linkEl) {
          let check = linkEl;
          for (let i = 0; i < 5 && check; i++) {
            if ((check.textContent || "").toUpperCase().includes("LIVE")) {
              isLive = true;
              break;
            }
            check = check.parentElement;
          }
        }
      } catch (e) {}

      // Deduplicate odds — take unique values in order
      const uniqueOdds = [...new Set(oddsVals)];
      if (uniqueOdds.length < 2) {
        // If all odds are the same value, skip (likely parsing error)
        dbg(`  Skipping ${teams.team1} vs ${teams.team2}: all odds identical (${uniqueOdds[0]})`);
        continue;
      }

      rows.push({
        team1: teams.team1,
        team2: teams.team2,
        odds_team1: uniqueOdds[0],
        odds_team2: uniqueOdds[1],
        bookmaker: "1xbit",
        market: "match_winner",
        url: href,
        isLive,
      });
    }

    dbg(`Bo3.gg: ${rows.length} matches with valid odds`);
    return rows;
  }

  // Find the DOM-closest link to a target element
  function findClosestLink(target, links) {
    let best = null;
    let bestDist = Infinity;
    const targetRect = target.getBoundingClientRect();

    for (const link of links) {
      const linkRect = link.getBoundingClientRect();
      const dist = Math.abs(targetRect.top - linkRect.top) + Math.abs(targetRect.left - linkRect.left);
      if (dist < bestDist) {
        bestDist = dist;
        best = link;
      }
    }
    return best;
  }

  // Parse team names from Bo3.gg URL like /matches/bad-luck-vs-r2-es-24-02-2026
  function parseTeamsFromBo3ggUrl(href) {
    const slug = href.match(/\/matches\/(.+)/)?.[1];
    if (!slug) return null;

    const vsParts = slug.split("-vs-");
    if (vsParts.length < 2) return null;

    // Team 1: everything before "-vs-"
    const t1raw = vsParts[0];

    // Team 2: everything after "-vs-", minus date suffix
    const rest = vsParts.slice(1).join("-vs-");
    const t2raw = rest.replace(/-\d{2}-\d{2}-\d{4}.*$/, "");

    // Clean up team slugs (remove game suffixes, "team-" prefix)
    const team1 = capitalize(cleanTeamSlug(t1raw));
    const team2 = capitalize(cleanTeamSlug(t2raw));

    if (!team1 || !team2 || team1.length < 2 || team2.length < 2) return null;

    return { team1, team2 };
  }

  // Remove common Bo3.gg URL suffixes that are NOT part of team names
  function cleanTeamSlug(slug) {
    return slug
      .replace(/^team-/i, "")          // "team-novaq" → "novaq"
      .replace(/-cs-go$/i, "")         // "m80-cs-go" → "m80"
      .replace(/-csgo$/i, "")          // "-csgo" suffix
      .replace(/-cs2$/i, "")           // "-cs2" suffix
      .replace(/-cs$/i, "")            // "galorys-cs" → "galorys"
      .replace(/-es$/i, "")            // "r2-es" → "r2"
      .replace(/-esports$/i, "")       // "the-huns-esports" → "the-huns"
      .replace(/-/g, " ");             // hyphens to spaces
  }

  // ====================================================================
  // ODDSPEDIA SCRAPER
  // Oddspedia often shows "NO ODDS AVAILABLE" for CS matches.
  // We try to find any match with actual odds displayed.
  // ====================================================================
  function scrapeOddspedia() {
    const rows = [];

    // Use the same bottom-up approach: find odds text nodes, match to links
    const oddsNodes = findOddsTextNodes(document.body);

    for (const node of oddsNodes) {
      let el = node.el;
      let matchLink = null;
      for (let i = 0; i < 8 && el; i++) {
        const links = el.querySelectorAll("a[href*='match'], a[href*='game']");
        if (links.length === 1) { matchLink = links[0]; break; }
        el = el.parentElement;
      }
      node.matchHref = matchLink ? matchLink.href : null;
    }

    const byMatch = {};
    for (const node of oddsNodes) {
      if (!node.matchHref) continue;
      if (!byMatch[node.matchHref]) byMatch[node.matchHref] = [];
      byMatch[node.matchHref].push(node.val);
    }

    for (const [href, oddsVals] of Object.entries(byMatch)) {
      if (oddsVals.length < 2) continue;
      const uniqueOdds = [...new Set(oddsVals)];
      if (uniqueOdds.length < 2) continue;

      // Try to parse teams from URL
      const urlVs = href.match(/\/([^/]*?)-vs-([^/]*?)(?:-\d|\/|$)/i);
      if (!urlVs) continue;

      rows.push({
        team1: capitalize(urlVs[1].replace(/-/g, " ")),
        team2: capitalize(urlVs[2].replace(/-/g, " ")),
        odds_team1: uniqueOdds[0],
        odds_team2: uniqueOdds[1],
        bookmaker: "oddspedia",
        market: "match_winner",
        url: href,
        isLive: false,
      });
    }

    dbg(`Oddspedia: found ${rows.length} odds rows`);
    return rows;
  }

  // ====================================================================
  // HELPERS
  // ====================================================================
  function capitalize(s) {
    if (!s) return "";
    return s.trim().split(/\s+/).map(w => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
  }

  // ====================================================================
  // FEED MESSAGE
  // ====================================================================
  function buildOddsMessage(row) {
    const spread = row.odds_team1 && row.odds_team2
      ? Math.abs((1/row.odds_team1 + 1/row.odds_team2 - 1) * 100)
      : null;
    return {
      v: 1, type: "odds", source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: {
        sport: "cs2",
        bookmaker: row.bookmaker || "unknown",
        market: row.market || "match_winner",
        team1: row.team1, team2: row.team2,
        odds_team1: row.odds_team1, odds_team2: row.odds_team2,
        liquidity_usd: 3000.0,
        spread_pct: spread !== null ? Math.round(spread * 100) / 100 : null,
        url: row.url || window.location.href,
      },
    };
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
      startScanning(); startHeartbeat();
    };
    ws.onmessage = (e) => { dbg("← Hub:", e.data); };
    ws.onclose = () => {
      connected = false; log("Disconnected");
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
  // SCAN LOOP
  // ====================================================================
  function scanAndSend() {
    const oddsRows = scrapeOdds();
    lastScan = oddsRows;

    let detailHtml = "";
    for (const r of oddsRows) {
      detailHtml += `${r.team1} vs ${r.team2}: ${r.odds_team1}/${r.odds_team2}${r.isLive?' 🔴':''}<br>`;
    }
    updatePanel(connected ? "✅ Connected" : "❌ Disconnected", oddsRows.length, detailHtml);

    if (oddsRows.length === 0) { dbg("No odds found this scan"); return; }

    log(`Found ${oddsRows.length} odds row(s):`);
    for (const row of oddsRows) {
      log(`  ${row.team1} vs ${row.team2}: ${row.odds_team1}/${row.odds_team2} (${row.bookmaker})${row.isLive?' LIVE':''}`);
      sendJSON(buildOddsMessage(row));
    }
  }

  function startScanning() { stopScanning(); scanAndSend(); scanTimer = setInterval(scanAndSend, SCAN_INTERVAL_MS); }
  function stopScanning() { if (scanTimer) { clearInterval(scanTimer); scanTimer = null; } }
  function startHeartbeat() {
    stopHeartbeat();
    hbTimer = setInterval(() => {
      sendJSON({
        v: 1, type: "heartbeat", source: SOURCE_NAME,
        ts: new Date().toISOString(),
        payload: { page: window.location.href, odds_found: lastScan.length },
      });
    }, HEARTBEAT_MS);
  }
  function stopHeartbeat() { if (hbTimer) { clearInterval(hbTimer); hbTimer = null; } }

  // ====================================================================
  // INIT
  // ====================================================================
  function init() {
    log(`Initializing v3 on ${detectSite()}...`);
    setTimeout(() => { createPanel(); connectWS(); }, 2000);
  }

  init();
})();
