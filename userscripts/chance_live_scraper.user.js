// ==UserScript==
// @name         Chance.cz → Feed Hub Live Scraper
// @namespace    rustmisko
// @version      1.1
// @description  Scrapes live odds/scores from Chance.cz and sends to Feed Hub — Azuro edge source #3
// @author       RustMisko
// @match        https://www.chance.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  // ====================================================================
  // CONFIG
  // ====================================================================
  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 2000;
  const RECONNECT_MS = 5000;
  const HEARTBEAT_MS = 20000;
  const SOURCE_NAME = "chance";
  const DEBUG = true;

  let ws = null;
  let connected = false;
  let sentCount = 0;
  let errorCount = 0;
  let scanCount = 0;
  let scanTimer = null;
  let hbTimer = null;
  let lastMatchCount = 0;

  const PREFIX = "[Chance→Hub]";
  function log(...args) { console.log(PREFIX, ...args); }
  function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

  // ====================================================================
  // SPORT KEYWORDS (Czech + English)
  // ====================================================================
  const CS2_KW = ['counter-strike', 'counter strike', 'cs2', 'cs:go', 'csgo',
    'counter-strike 2', 'iem', 'blast', 'esl pro', 'faceit', 'pgl', 'cct',
    'berserk', 'thunderpick', 'esea', 'jb pro', 'fireconter', 'pinnacle cup',
    'elisa', 'roobet', 'betboom', 'dust2', 'perfect world'];
  const DOTA_KW = ['dota 2', 'dota2', 'dota-2', 'the international', 'esl one',
    'dreamleague', 'riyadh masters'];
  const LOL_KW = ['league of legends', 'lol', ' lcs', ' lec', ' lck', ' lpl',
    'worlds 20', 'msi 20', 'rift rivals'];
  const VAL_KW = ['valorant', 'vct ', 'champions tour'];
  const COD_KW = ['call of duty', 'cdl '];

  // eFOOTBALL / eBasketball → EXCLUDE
  const EFOOTBALL_KW = ['efootball', 'e-football', 'ea sports fc', 'fifa',
    'e-fotbal', 'efotbal', 'esports battle', 'esoccer', 'e-soccer',
    'fc volta', 'volta –', 'volta -', 'fifa online', 'konami'];
  const EBASKET_KW = ['nba 2k', 'nba2k', 'e-basketbal', 'ebasketbal',
    'ebasketball', 'e-basketball', 'nba – esports', 'nba esports',
    'nba - esports', 'cyber basketball', 'e-nba'];

  // Real football/basketball club names → means eFOOTBALL/eBasketball
  const REAL_FOOTBALL_CLUBS = [
    'liverpool', 'realmadrid', 'barcelona', 'manchestercity', 'manchesterunited',
    'chelsea', 'arsenal', 'juventus', 'bayernmunchen', 'bayernmünchen', 'dortmund',
    'psg', 'atletico', 'ajax', 'milan', 'internazionale', 'roma', 'napoli',
    'tottenham', 'everton', 'westham', 'leicester', 'villarreal', 'sevilla',
    'benfica', 'porto', 'sportingcp', 'marseille', 'monaco',
    'fcbayern', 'borussia', 'atletimadrid', 'atleticomadrid', 'fcbarcelona',
    'realmadridcf', 'mancity', 'manunited', 'manunitedfc',
    'morocco', 'england', 'argentina', 'spain', 'france', 'germany',
    'vitoria', 'braga', 'brighton', 'wolves', 'aston', 'fulham',
  ];
  const REAL_BASKETBALL_CLUBS = [
    'lakers', 'celtics', 'warriors', 'bulls', 'heat', 'knicks', 'nets',
    'clippers', 'rockets', 'cavaliers', 'timberwolves', 'nuggets', 'suns',
    'bucks', 'spurs', 'raptors', 'mavericks', 'grizzlies', 'hawks', 'pacers',
    'pelicans', 'kings', 'hornets', 'blazers', 'thunder', 'pistons', 'magic',
    'wizards', 'jazz', 'atlantahawks', 'neworleans', 'losangeles',
    'philadelphia', 'milwaukee', 'sacramento', 'cleveland',
  ];

  // Sport map for URL detection
  const SPORT_URL_MAP = {
    'esporty': 'esports',
    'esport': 'esports',
    'fotbal': 'football',
    'hokej': 'hockey',
    'ledni-hokej': 'hockey',
    'tenis': 'tennis',
    'basketbal': 'basketball',
    'hazena': 'handball',
    'volejbal': 'volleyball',
    'stolni-tenis': 'table-tennis',
    'baseball': 'baseball',
    'rugby': 'rugby',
    'mma': 'mma',
    'box': 'mma',
    'snooker': 'snooker',
    'cricket': 'cricket',
    'futsal': 'futsal',
  };

  // ====================================================================
  // UI PANEL
  // ====================================================================
  function createPanel() {
    const panel = document.createElement("div");
    panel.id = "chance-feedhub-panel";
    panel.style.cssText = `
      position: fixed; bottom: 10px; right: 10px; z-index: 999999;
      background: #1a1a2e; color: #0f0; font-family: 'Consolas', monospace;
      font-size: 11px; padding: 10px 14px; border-radius: 8px;
      border: 1px solid #e63946; min-width: 280px; max-width: 400px;
      opacity: 0.92; box-shadow: 0 0 20px rgba(230,57,70,0.2);
      max-height: 350px; overflow-y: auto;
      cursor: move; user-select: none;
    `;
    panel.innerHTML = `
      <div style="font-weight:bold; margin-bottom: 4px; color: #e63946; font-size: 13px;">
        🎰 Chance.cz → Feed Hub v1.1
      </div>
      <div id="ch-status" style="color: #fa0;">⏳ Connecting...</div>
      <div id="ch-sport" style="margin-top: 2px; color: #aaa;">Sport: detecting...</div>
      <div id="ch-matches" style="margin-top: 4px; color: #aaa;">Scanning...</div>
      <div id="ch-sent" style="margin-top: 2px; color: #0ff;">Sent: 0</div>
      <div id="ch-errors" style="margin-top: 2px; color: #888;">Errors: 0</div>
      <div id="ch-detail" style="font-size:10px; color:#8f8; margin-top:4px; max-height:120px; overflow-y:auto; white-space:pre-wrap;"></div>
      <div style="margin-top:6px; font-size:10px; color:#666;">${WS_URL}</div>
      <div style="margin-top:4px;">
        <button id="ch-btn-scan" style="background:#e63946;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Force Scan</button>
        <button id="ch-btn-debug" style="background:#333;color:#ff0;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;">DOM Debug</button>
      </div>
    `;
    document.body.appendChild(panel);

    // Dragging
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

    // Buttons
    setTimeout(() => {
      const scanBtn = document.getElementById("ch-btn-scan");
      if (scanBtn) scanBtn.addEventListener("click", () => { doScan(); });
      const debugBtn = document.getElementById("ch-btn-debug");
      if (debugBtn) debugBtn.addEventListener("click", () => { domDebug(); });
    }, 500);
  }

  function updatePanel(status, matchInfo) {
    const el = (id) => document.getElementById(id);
    if (el("ch-status")) el("ch-status").textContent = status;
    if (el("ch-matches")) el("ch-matches").textContent = matchInfo;
    if (el("ch-sent")) el("ch-sent").textContent = `Sent: ${sentCount}`;
    if (el("ch-errors")) el("ch-errors").textContent = `Errors: ${errorCount}`;
  }

  function updateDetail(text) {
    const el = document.getElementById("ch-detail");
    if (el) el.textContent = text;
  }

  function domDebug() {
    const links = document.querySelectorAll('a[href*="/live/zapas/"]');
    log(`DOM Debug: ${links.length} match links found`);
    links.forEach((a, i) => {
      if (i < 5) {
        const oddsEls = a.querySelectorAll('[data-atid*="||ODD||"]');
        log(`  [${i}] ${a.textContent.trim().substring(0, 80)} | odds_els=${oddsEls.length} | href=${a.href}`);
      }
    });
    // Also check generic <a> links
    const allA = document.querySelectorAll('a');
    let matchLike = 0;
    allA.forEach(a => { if (a.textContent.includes(' - ') && a.textContent.length < 200) matchLike++; });
    log(`DOM Debug: ${allA.length} total <a>, ${matchLike} match-like`);
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
      updatePanel("✅ Connected", "Scanning...");
      startScanning();
      startHeartbeat();
    };

    ws.onclose = (e) => {
      connected = false;
      log("❌ Disconnected:", e.code);
      updatePanel("❌ Disconnected — reconnecting...", `Last: ${lastMatchCount} matches`);
      stopScanning();
      stopHeartbeat();
      setTimeout(connectWS, RECONNECT_MS);
    };

    ws.onerror = () => { errorCount++; };
    ws.onmessage = (e) => { dbg("Server:", e.data); };
  }

  function sendJSON(obj) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    try {
      ws.send(JSON.stringify(obj));
      sentCount++;
      return true;
    } catch (e) {
      errorCount++;
      return false;
    }
  }

  function startHeartbeat() {
    stopHeartbeat();
    hbTimer = setInterval(() => {
      if (connected) sendJSON({ v: 1, type: "heartbeat", source: SOURCE_NAME, ts: new Date().toISOString() });
    }, HEARTBEAT_MS);
  }
  function stopHeartbeat() { if (hbTimer) { clearInterval(hbTimer); hbTimer = null; } }

  // ====================================================================
  // SPORT DETECTION
  // ====================================================================

  /** Find closest section header text above a match link */
  function findSectionHeader(matchLink) {
    // Strategy: walk up DOM, check previous siblings for header divs
    let node = matchLink;
    for (let depth = 0; depth < 8; depth++) {
      if (!node || node === document.body) break;

      let sib = node.previousElementSibling;
      let sibCount = 0;
      while (sib && sibCount < 5) {
        const text = sib.textContent.trim();
        // Section headers: short text, NOT a match row (match rows have scores like "0:0")
        // Czech headers often contain " - " (e.g. "Fotbal - muži", "Tenis ženy - dvouhra")
        // so we can't filter on " - " alone. Instead: skip if looks like a match (has score pattern)
        const looksLikeMatch = /\d{1,3}:\d{1,3}/.test(text) && text.includes(' - ');
        if (text.length > 3 && text.length < 150 && !looksLikeMatch) {
          // Check if this looks like a header (has header-like class or contains sport keywords)
          const cls = (sib.className || '').toLowerCase();
          const isHeaderLike = cls.includes('header') || cls.includes('title') ||
            cls.includes('category') || cls.includes('league') ||
            cls.includes('section') || cls.includes('nazev') ||
            cls.includes('skupina') || cls.includes('fc8996f8'); // Chance header class
          // Also accept text with known league/sport keywords
          const lt = text.toLowerCase();
          const hasSportKw = /liga|league|cup|pohár|tour|open|masters|series|atp|wta|itf|utr|ahl|nhl|khl|shl|ligue|serie|bundesliga|premiership|championship|division|muži|ženy|dvouhra|čtyřhra/i.test(lt);
          if (isHeaderLike || hasSportKw || text.length < 80) {
            return text;
          }
        }
        sib = sib.previousElementSibling;
        sibCount++;
      }
      node = node.parentElement;
    }
    return null;
  }

  /** Detect sport for a match, including esport sub-type */
  function detectMatchSport(matchLink, t1, t2) {
    const href = (matchLink.href || '').toLowerCase();

    // 1. URL-based detection: /live/zapas/esporty-xxx, /live/zapas/fotbal-xxx
    for (const [urlKey, sport] of Object.entries(SPORT_URL_MAP)) {
      if (href.includes('/' + urlKey + '-') || href.includes('/' + urlKey + '/')) {
        if (sport === 'esports') {
          return detectEsportSubtype(matchLink, t1, t2);
        }
        return sport;
      }
    }

    // 2. Section header detection
    const header = findSectionHeader(matchLink);
    if (header) {
      const ht = (' ' + header.toLowerCase() + ' ');

      // eFOOTBALL/eBasketball → EXCLUDE
      if (EFOOTBALL_KW.some(k => ht.includes(k))) return 'skip';
      if (EBASKET_KW.some(k => ht.includes(k))) return 'skip';

      // Esport sub-types
      if (CS2_KW.some(k => ht.includes(k))) return 'cs2';
      if (LOL_KW.some(k => ht.includes(k))) return 'league-of-legends';
      if (DOTA_KW.some(k => ht.includes(k))) return 'dota-2';
      if (VAL_KW.some(k => ht.includes(k))) return 'valorant';
      if (COD_KW.some(k => ht.includes(k))) return 'esports';

      // Traditional sports (Czech headers: "1. španělská liga, Fotbal - muži")
      if (ht.includes('fotbal') && !EFOOTBALL_KW.some(k => ht.includes(k))) return 'football';
      if (ht.includes('hokej') || ht.includes('hockey') || ht.includes('ahl') || ht.includes('nhl') || ht.includes('khl')) return 'hockey';
      if ((ht.includes('tenis') || ht.includes('tennis')) && !ht.includes('stolní') && !ht.includes('stolni')) return 'tennis';
      if (ht.includes('stolní tenis') || ht.includes('stolni tenis') || ht.includes('table tennis')) return 'table-tennis';
      if (ht.includes('basketbal') && !EBASKET_KW.some(k => ht.includes(k))) return 'basketball';
      if (ht.includes('házená') || ht.includes('handball')) return 'handball';
      if (ht.includes('volejbal') || ht.includes('volleyball')) return 'volleyball';
      if (ht.includes('futsal')) return 'futsal';
      if (ht.includes('baseball') || ht.includes('mlb')) return 'baseball';
      if (ht.includes('rugby')) return 'rugby';
      if (ht.includes('golf')) return 'golf';
      if (ht.includes('cricket')) return 'cricket';
      if (ht.includes('snooker')) return 'snooker';
      if (ht.includes('mma') || ht.includes('ufc') || ht.includes('box')) return 'mma';
    }

    // 3. Fallback: player-nick pattern (eFOOTBALL: "Team (Nick) - Team (Nick)")
    const rawText = matchLink.textContent || '';
    if (hasPlayerNickPattern(rawText)) return 'skip';

    // 4. Fallback: guess from team names (detect eFOOTBALL)
    return guessFromTeamNames(t1, t2);
  }

  /** For esport URLs, detect specific game from section header */
  function detectEsportSubtype(matchLink, t1, t2) {
    const header = findSectionHeader(matchLink);
    if (header) {
      const ht = (' ' + header.toLowerCase() + ' ');
      if (EFOOTBALL_KW.some(k => ht.includes(k))) return 'skip';
      if (EBASKET_KW.some(k => ht.includes(k))) return 'skip';
      if (CS2_KW.some(k => ht.includes(k))) return 'cs2';
      if (LOL_KW.some(k => ht.includes(k))) return 'league-of-legends';
      if (DOTA_KW.some(k => ht.includes(k))) return 'dota-2';
      if (VAL_KW.some(k => ht.includes(k))) return 'valorant';
      if (COD_KW.some(k => ht.includes(k))) return 'esports';
    }

    // Fallback: player-nick pattern detection (eFOOTBALL: "Team (Nick) - Team (Nick)")
    const rawText = matchLink.textContent || '';
    if (hasPlayerNickPattern(rawText)) return 'skip';

    // Fallback: team name guessing
    const guess = guessFromTeamNames(t1, t2);
    if (guess === 'skip') return 'skip';
    return guess || 'esports';
  }

  /** Guess if match is eFOOTBALL/eBasketball from team names */
  function guessFromTeamNames(t1, t2) {
    const key = (t1 + ' ' + t2).toLowerCase().replace(/[^a-z]/g, '');
    if (REAL_BASKETBALL_CLUBS.some(c => key.includes(c))) return 'skip';
    if (REAL_FOOTBALL_CLUBS.some(c => key.includes(c))) return 'skip';
    return null;
  }

  /** Check if raw link text has eFOOTBALL/eBasketball pattern: "Team (Nick)" */
  function hasPlayerNickPattern(linkText) {
    // eFOOTBALL/VOLTA/eBasketball pattern: "Arsenal (Glumac) - Man City (Maslja)"
    // Real esports teams don't have this pattern → team names are plain
    const matches = linkText.match(/\([A-Z][a-z]{2,15}\)/g);
    return matches && matches.length >= 2;
  }

  // ====================================================================
  // ODDS EXTRACTION — data-atid primary, text fallback
  // ====================================================================

  /**
   * Extract odds from data-atid attributes inside the match link.
   * Format: data-atid="content||ODD||id1||id2||oddsValue||matchId"
   * Returns array of odds values in DOM order (team1, [draw], team2)
   */
  function extractOddsFromDataAtid(container) {
    const oddsEls = container.querySelectorAll('[data-atid*="||ODD||"]');
    const values = [];

    for (const el of oddsEls) {
      const atid = el.getAttribute('data-atid');
      if (!atid) continue;

      const parts = atid.split('||');
      // Expected: content||ODD||id1||id2||oddsValue||matchId (6 parts)
      if (parts.length >= 5) {
        const oddsStr = parts[4];
        const val = parseFloat(oddsStr);
        if (!isNaN(val) && val >= 1.01 && val <= 500) {
          values.push(val);
        }
      }
    }

    return values;
  }

  /**
   * Fallback: Extract odds from leaf text elements (decimal pattern).
   * Same approach as Tipsport scraper.
   */
  function extractOddsFromText(container) {
    const values = [];
    const candidates = container.querySelectorAll('span, button, td, div, b, strong');

    for (const el of candidates) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (text.length < 3 || text.length > 7) continue;
      if (/^\d{1,3}[,.]\d{2}$/.test(text)) {
        const val = parseFloat(text.replace(',', '.'));
        if (val >= 1.01 && val <= 500) {
          values.push(val);
        }
      }
    }

    return values;
  }

  // ====================================================================
  // SCORE EXTRACTION
  // ====================================================================

  /**
   * Find score element inside match container.
   * Chance.cz uses a dedicated div (class sc-837f7f43-0) with "X:Y"
   */
  function extractScore(container) {
    // Strategy 1: Find leaf element with EXACTLY "X:Y" as text
    const allEls = container.querySelectorAll('div, span, b, strong');
    for (const el of allEls) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (/^\d{1,3}:\d{1,3}$/.test(text)) {
        const m = text.match(/^(\d{1,3}):(\d{1,3})$/);
        if (m) return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
      }
    }

    // Strategy 2: Small leaf element containing score
    for (const el of allEls) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (text.length > 15 || text.length < 1) continue;
      // Skip period labels like "2.pol", "1.set"
      if (/\d\.\s*(?:pol|set|tře|per|mapa|min|čt)/i.test(text)) continue;
      const m = text.match(/(\d{1,3}):(\d{1,3})/);
      if (m) return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
    }

    return null;
  }

  /**
   * Extract detailed score string (map scores, period info).
   * Examples: "Lepší ze 3 | 3.mapa - 13:6, 9:13, 7:12"
   */
  function extractDetailedScore(container) {
    const allEls = container.querySelectorAll('span, div');
    for (const el of allEls) {
      const text = el.textContent.trim();
      // Match "Lepší ze/z N | ..." pattern (Czech "Best of N")
      if (/Lepší\s+z/i.test(text) && text.length < 120) {
        return text;
      }
      // Match period/set/map info
      if (/\d\.\s*(mapa|set|pol|čt|perioda|třetina)/i.test(text) && text.length < 80) {
        return text;
      }
    }
    return '';
  }

  // ====================================================================
  // TEAM NAME CLEANING
  // ====================================================================

  function cleanName(text) {
    if (!text) return "";
    return text
      .replace(/\([^)]*\)\s*$/g, "")   // Remove trailing (Ronin), (Griffin) etc.
      .replace(/\(\d+\)/g, "")          // Remove seedings (1)
      .replace(/^\d+\.\s*/, "")         // Remove numbering "1. "
      .replace(/GÓL\s*$/i, "")
      .replace(/\bGÓL\b/gi, "")
      .replace(/\bŽLUTÁ\b/gi, "")
      .replace(/\bČERVENÁ\b/gi, "")
      .replace(/Za\s*okamžik.*/i, "")
      .replace(/Za\s+\d+\s*min.*/i, "")
      .replace(/Přestávka.*/i, "")
      .replace(/\s+/g, " ")
      .trim();
  }

  // ====================================================================
  // MAIN SCAN — finds all live match rows on Chance.cz
  // ====================================================================

  function scanChanceMatches() {
    const matches = [];
    const seen = new Set();

    // PRIMARY: Find all match links with /live/zapas/ in href
    const matchLinks = document.querySelectorAll('a[href*="/live/zapas/"]');

    for (const link of matchLinks) {
      // Extract team name from first <span> with " - " pattern
      let t1 = null, t2 = null;
      const spans = link.querySelectorAll('span');
      for (const sp of spans) {
        const text = sp.textContent.trim();
        if (text.includes(' - ') && text.length < 150 && text.length > 4) {
          const m = text.match(/^(.+?)\s*-\s*(.+)$/);
          if (m) {
            t1 = cleanName(m[1]);
            t2 = cleanName(m[2]);
            break;
          }
        }
      }

      // Fallback: try link's direct text (first part before score)
      if (!t1 || !t2) {
        const fullText = link.textContent.trim();
        if (fullText.includes(' - ')) {
          // Cut at first digit sequence (score starts)
          const cut = fullText.match(/^(.+?)\s*-\s*(.+?)(?=\d{1,3}:\d{1,3}|Za\s|Lepší|Kurzy|\d{1,3}[,.]\d{2})/);
          if (cut) {
            t1 = cleanName(cut[1]);
            t2 = cleanName(cut[2]);
          }
        }
      }

      if (!t1 || !t2 || t1.length < 2 || t2.length < 2) continue;
      if (t1.toLowerCase() === t2.toLowerCase()) continue;

      // Deduplicate
      const key = `${t1.toLowerCase()}|${t2.toLowerCase()}`;
      if (seen.has(key)) continue;
      seen.add(key);

      // Live detection
      const fullText = link.textContent.toLowerCase();
      const hasScore = /\d{1,3}:\d{1,3}/.test(fullText);
      const hasPeriod = /pol\.|\.min|přestávka|mapa|\.set|\.čt|třetina/i.test(fullText);
      const isPrematch = /za\s+\d+\s*minut/i.test(fullText) || /za\s*okamžik/i.test(fullText);
      const hasKurzyUnavailable = /kurzy nejsou/i.test(fullText);
      const isLive = (hasScore || hasPeriod) && !isPrematch;

      // Skip prematch (no live edge) and matches without odds
      if (!isLive && isPrematch) {
        dbg(`SKIP prematch: ${t1} - ${t2}`);
        continue;
      }

      // Extract odds — data-atid (reliable!) first, text fallback
      let oddsValues = extractOddsFromDataAtid(link);
      if (oddsValues.length < 2) {
        oddsValues = extractOddsFromText(link);
      }

      if (oddsValues.length < 2 || hasKurzyUnavailable) {
        dbg(`No odds: ${t1} - ${t2} (found ${oddsValues.length}, kurzy_unavailable=${hasKurzyUnavailable})`);
        continue;
      }

      // Assign odds: 3+ = 1/X/2, 2 = 1/2
      let odds1, oddsX = 0, odds2;
      if (oddsValues.length >= 3) {
        odds1 = oddsValues[0]; oddsX = oddsValues[1]; odds2 = oddsValues[2];
      } else {
        odds1 = oddsValues[0]; odds2 = oddsValues[1];
      }

      // Extract score
      const scoreResult = extractScore(link);
      const score1 = scoreResult ? scoreResult.s1 : 0;
      const score2 = scoreResult ? scoreResult.s2 : 0;
      const detailedScore = extractDetailedScore(link);

      // Detect sport
      const sport = detectMatchSport(link, t1, t2);
      if (sport === 'skip') {
        dbg(`SKIP eFOOTBALL/eBasket: ${t1} - ${t2}`);
        continue;
      }

      matches.push({
        team1: t1,
        team2: t2,
        odds1: odds1,
        odds2: odds2,
        oddsX: oddsX,
        score1: score1,
        score2: score2,
        detailedScore: detailedScore,
        isLive: isLive,
        sport: sport || 'unknown',
      });
    }

    // FALLBACK: if no /live/zapas/ links found, try generic <a> approach (like Tipsport)
    if (matches.length === 0) {
      dbg("No /live/zapas/ links found, trying generic <a> scan...");
      return scanGenericLinks();
    }

    return matches;
  }

  /**
   * Fallback scanner: same approach as Tipsport scraper.
   * Find all <a> with "Team - Team" pattern, walk up DOM for odds.
   */
  function scanGenericLinks() {
    const matches = [];
    const seen = new Set();
    const allLinks = document.querySelectorAll('a');

    for (const link of allLinks) {
      const rawText = link.textContent.trim().replace(/\s+/g, ' ');
      if (rawText.length < 5 || rawText.length > 300) continue;
      if (rawText.indexOf(' - ') < 2) continue;

      // Skip league/header links
      if (rawText.includes(',') && /fotbal|tenis|hokej|basket|esport/i.test(rawText)) continue;
      if (/\s*-\s*(muži|ženy|women|men)/i.test(rawText)) continue;

      // Extract team names (cut before scores/garbage)
      const beforeGarbage = rawText.replace(/\d{1,3}:\d{1,3}.*$/, '').trim();
      const m = beforeGarbage.match(/^(.+?)\s*-\s*(.+)$/);
      if (!m) continue;

      const t1 = cleanName(m[1]);
      const t2 = cleanName(m[2]);
      if (!t1 || !t2 || t1.length < 2 || t2.length < 2) continue;

      const key = `${t1.toLowerCase()}|${t2.toLowerCase()}`;
      if (seen.has(key)) continue;
      seen.add(key);

      // Walk up DOM for odds
      let container = link;
      let oddsValues = [];
      for (let depth = 0; depth < 8; depth++) {
        container = container.parentElement;
        if (!container || container === document.body) break;

        oddsValues = extractOddsFromDataAtid(container);
        if (oddsValues.length < 2) oddsValues = extractOddsFromText(container);
        if (oddsValues.length >= 2 && oddsValues.length <= 8) break;
        if (oddsValues.length > 30) { oddsValues = []; break; }
      }

      if (oddsValues.length < 2) continue;

      let odds1, oddsX = 0, odds2;
      if (oddsValues.length >= 3) {
        odds1 = oddsValues[0]; oddsX = oddsValues[1]; odds2 = oddsValues[2];
      } else {
        odds1 = oddsValues[0]; odds2 = oddsValues[1];
      }

      const fullText = link.textContent.toLowerCase();
      const isLive = /\d:\d/.test(fullText) || /pol\.|\.min|mapa|set/i.test(fullText);

      if (!isLive) continue; // Live only

      const scoreResult = extractScore(container);
      const sport = detectMatchSport(link, t1, t2);
      if (sport === 'skip') continue;

      matches.push({
        team1: t1, team2: t2,
        odds1, odds2, oddsX,
        score1: scoreResult ? scoreResult.s1 : 0,
        score2: scoreResult ? scoreResult.s2 : 0,
        detailedScore: '',
        isLive: true,
        sport: sport || 'unknown',
      });
    }

    return matches;
  }

  // ====================================================================
  // FEED MESSAGES — identical format to Tipsport (for feed-hub compatibility)
  // ====================================================================

  function buildOddsMessage(match) {
    return {
      v: 1,
      type: "odds",
      source: SOURCE_NAME,
      ts: new Date().toISOString(),
      payload: {
        sport: match.sport,
        bookmaker: "chance",
        market: "match_winner",
        team1: match.team1,
        team2: match.team2,
        odds_team1: match.odds1,
        odds_team2: match.odds2,
        url: window.location.href,
      },
    };
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
        detailed_score: match.detailedScore,
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
    scanCount++;
    const matches = scanChanceMatches();
    let sentThisScan = 0;
    const sportCounts = {};

    for (const match of matches) {
      if (match.sport === 'skip') continue;

      // Send odds
      const oddsMsg = buildOddsMessage(match);
      if (sendJSON(oddsMsg)) sentThisScan++;

      // Send live score with sanity check
      if (match.isLive) {
        const maxScore = Math.max(match.score1, match.score2);
        const sportLimits = {
          'football': 8, 'hockey': 10, 'tennis': 7, 'basketball': 200,
          'cs2': 40, 'dota-2': 100, 'mma': 5, 'handball': 45, 'volleyball': 5,
          'esports': 50, 'league-of-legends': 100, 'valorant': 30,
          'table-tennis': 15, 'futsal': 15,
        };
        const limit = sportLimits[match.sport] || 999;
        if (maxScore > limit) {
          dbg(`SCORE SANITY REJECT: ${match.team1} ${match.score1}:${match.score2} (${match.sport} max=${limit})`);
        } else {
          const liveMsg = buildLiveMessage(match);
          if (liveMsg && sendJSON(liveMsg)) sentThisScan++;
        }
      }

      sportCounts[match.sport] = (sportCounts[match.sport] || 0) + 1;
    }

    lastMatchCount = matches.length;
    const statusText = connected ? "✅ Connected" : "❌ Disconnected";

    // Build match list for panel
    const matchInfo = matches.length > 0
      ? matches.slice(0, 8).map(m =>
          `${m.sport}: ${m.team1} ${m.odds1.toFixed(2)}/${m.odds2.toFixed(2)} ${m.team2}${m.isLive ? ' 🔴' : ''}`
        ).join("\n") + (matches.length > 8 ? `\n...+${matches.length - 8} more` : "")
      : "No live matches with odds found";

    updatePanel(statusText, matchInfo);

    // Sport summary in detail panel
    const sportSummary = Object.entries(sportCounts).map(([s, c]) => `${s}:${c}`).join(", ");
    updateDetail(`Scan #${scanCount} | ${matches.length} LIVE [${sportSummary}] | sent ${sentThisScan}`);

    if (matches.length > 0 && scanCount % 5 === 1) {
      log(`Scan #${scanCount}: ${matches.length} LIVE [${sportSummary}], sent ${sentThisScan}, total ${sentCount}`);
    }
  }

  // ====================================================================
  // INIT
  // ====================================================================

  function init() {
    log("🎰 Chance.cz Live Scraper v1.1");
    log("Page:", window.location.href);
    log("Strategy: data-atid odds (primary) + generic text (fallback)");
    log("Feed Hub:", WS_URL);

    createPanel();
    connectWS();
  }

  if (document.readyState === "complete") {
    setTimeout(init, 2000);
  } else {
    window.addEventListener("load", () => setTimeout(init, 2000));
  }
})();
