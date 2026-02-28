// ==UserScript==
// @name         Tipsport → Feed Hub Odds Scraper
// @namespace    rustmisko
// @version      2.3
// @description  Scrapes live odds from Tipsport.cz and sends to Feed Hub — v2.3 row-scoped score extraction
// @author       RustMisko
// @match        https://www.tipsport.cz/*
// @match        https://m.tipsport.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function () {
  "use strict";

  const WS_URL = "ws://localhost:8080/feed";
  const SCAN_INTERVAL_MS = 2000; // 2s — instant score detection is our edge!
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
  const TP_FOOTBALL_KW = ['efootball', 'e-football', 'ea sports', 'fifa', 'e-fotbal', 'efotbal',
                           'esoccer', 'e-soccer', 'e-copa', 'ecopa', 'echampions', 'eliga', 'e-liga'];
  const TP_BASKET_KW   = ['nba 2k', 'nba2k', 'e-basketbal', 'ebasketbal', 'nba 2k25', 'nba2k25',
                           'ebasketball', 'e-basketball'];

  // Team names that indicate real football/basketball clubs in e-sports
  const TP_FOOTBALL_TEAMS = [
    'liverpool', 'realmadrid', 'barcelona', 'manchestercity', 'manchesterunited',
    'chelsea', 'arsenal', 'juventus', 'bayernmunchen', 'dortmund', 'borussiadortmund',
    'psg', 'atletico', 'porto', 'benfica', 'ajax', 'milan', 'internazionale', 'roma',
    'napoli', 'rangers', 'celtic', 'racing', 'riverplate', 'bocajuniors', 'flamengo',
    'corinthians', 'deportivo', 'vflwolfsburg', 'wolfsburg', 'eintrachtfrankfurt',
    'eintracht', 'sportingcp', 'vitoriasc', 'braga', 'sevilla', 'villarreal',
    'realbetica', 'realsociedad', 'osasuna', 'girona', 'lazio', 'fiorentina',
    'argentina', 'brazil', 'france', 'spain', 'germany', 'england', 'portugal',
    'netherlands', 'italy', 'sweden', 'denmark', 'ghana', 'mexico', 'unitedstates',
    'switzerland', 'austria', 'belgium', 'poland', 'ukraine',
  ];
  const TP_BASKETBALL_TEAMS = [
    'lakers', 'celtics', 'warriors', 'bulls', 'heat', 'knicks', 'nets', 'clippers',
    'houstonrockets', 'clevelandcavaliers', 'sacramentokings', 'minnesotatimberwolves',
    'denvernuggets', 'phoenixsuns', 'milwaukeebucks', 'goldenstatewarriors',
    'sanantoniospurs', 'torontoraptors', 'dallasmavericks', 'neworleanspe',
    'memphisgrizzlies', 'atlantahawks', 'charlottehornets', 'detroitpistons',
    'indianapacers', 'chicagobulls', 'orlandoMagic', 'washingtonwizards',
  ];

  function guessEsportTypeFromTeams(t1, t2) {
    const key = (t1 + ' ' + t2).toLowerCase().replace(/[^a-z]/g, '');
    if (TP_BASKETBALL_TEAMS.some(c => key.includes(c))) return 'skip'; // eBasketball → EXCLUDE
    if (TP_FOOTBALL_TEAMS.some(c => key.includes(c))) return 'skip'; // eFOOTBALL → EXCLUDE
    return null;
  }

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
          for (const kw of TP_CS2_KW)      if (txt.includes(kw)) return 'cs2';
          for (const kw of TP_DOTA_KW)     if (txt.includes(kw)) return 'dota-2';
          for (const kw of TP_LOL_KW)      if (txt.includes(kw)) return 'league-of-legends';
          for (const kw of TP_VAL_KW)      if (txt.includes(kw)) return 'valorant';
          for (const kw of TP_FOOTBALL_KW) if (txt.includes(kw)) { dbg(`TP e-football SKIP: ${txt.substring(0,40)}`); return 'skip'; }
          for (const kw of TP_BASKET_KW)   if (txt.includes(kw)) { dbg(`TP e-basketball SKIP: ${txt.substring(0,40)}`); return 'skip'; }
        }
        sib = sib.previousElementSibling;
        sibCount++;
      }
      node = node.parentElement;
    }
    return null; // Unknown, keep as 'esports'
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
      const { cleanText, garbage } = stripTipsportGarbage(rawText);
      const cleanedText = cleanText;
      let detailedScore = garbage;
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
        // specific: 'cs2','dota-2','lol','valorant','skip',null
        if (specific === 'skip') {
          dbg(`TP SKIP eFOOTBALL/eBasket: ${t1} vs ${t2}`);
          continue; // eFOOTBALL/eBasketball → EXCLUDE from feed entirely
        }
        if (specific) {
          matchSport = specific;
          dbg(`TP esport detected: ${matchSport} for ${t1} vs ${t2}`);
        } else {
          // Fallback: guess from team names (e.g. Houston Rockets → basketball)
          const guessed = guessEsportTypeFromTeams(t1, t2);
          if (guessed === 'skip') {
            dbg(`TP SKIP eFOOTBALL/eBasket (team-guess): ${t1} vs ${t2}`);
            continue; // eFOOTBALL/eBasketball → EXCLUDE from feed entirely
          }
          if (guessed) {
            matchSport = guessed;
            dbg(`TP esport team-guess: ${matchSport} for ${t1} vs ${t2}`);
          }
          // else keep as 'esports' — might still fuse via feed_hub fallback
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
        detailedScore: detailedScore, // <-- ADDED detailed score string here
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

        // Score extraction v3 — ELEMENT-LEVEL (not textContent!)
        // textContent concatenates child elements: "0:1" + "2.třetina" → "0:12.třetina"
        // SOLUTION: Find a specific DOM element whose text is ONLY a score like "0:1"
        // The blue badge on Tipsport is exactly such an element.
        let score1 = 0, score2 = 0;
        // CRITICAL FIX: score must be extracted from THIS match row/link first,
        // otherwise parent-level search can reuse one score for multiple matches.
        const scoreResult = findScoreElement(linkElement, container);
        if (scoreResult) {
          score1 = scoreResult.s1;
          score2 = scoreResult.s2;
        }

        // Assign odds: if 3+, treat as 1/X/2; if 2, treat as 1/2
        let odds1, oddsX, odds2;
        if (odds.length >= 3) {
          odds1 = odds[0]; oddsX = odds[1]; odds2 = odds[2];
        } else {
          odds1 = odds[0]; oddsX = 0; odds2 = odds[1];
        }

        // Filter suspended/placeholder odds:
        // odds ≤ 1.05 or ≥ 50.0 = market suspended (Tipsport shows 1.01/120.00 during VAR/goals)
        const isSuspended = odds1 <= 1.05 || odds2 <= 1.05 || odds1 >= 50.0 || odds2 >= 50.0;
        return {
          odds1, odds2, oddsX, score1, score2, isLive,
          hasOdds: odds1 > 1.0 && odds2 > 1.0 && !isSuspended,
        };
      }

      // If we find way too many odds, we've walked up to a page-level container
      if (odds.length > 30) return null;
    }

    return null;
  }

  /**
   * Find a DOM element that contains ONLY a score pattern "X:Y".
   * This avoids the textContent concatenation bug where "0:1" + "2.třetina" becomes "0:12".
   * Strategy 1: Leaf element with EXACTLY "X:Y" as full text (most reliable)
   * Strategy 2: Small element (<15 chars) containing "X:Y" as fallback
   * Strategy 3: Regex on textContent as last resort (with period-separator fix)
   */
  function findScoreElement(linkElement, container) {
    const selectors = 'span, div, b, strong, em, td, p, label, small, i';

    // Phase A (strict): scan only inside the concrete match link/row first.
    // This prevents cross-match score bleeding (same score copied to many rows).
    const primaryRoot = linkElement || container;
    const primaryCandidates = primaryRoot.querySelectorAll(selectors);

    for (const el of primaryCandidates) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (/^\d{1,3}\s*:\s*\d{1,3}$/.test(text)) {
        const m = text.match(/^(\d{1,3})\s*:\s*(\d{1,3})$/);
        if (m) {
          dbg(`Score found (phase A, row-scoped exact): "${text}"`);
          return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
        }
      }
    }

    for (const el of primaryCandidates) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (text.length < 1 || text.length > 15) continue;
      if (/\d\.\s*(?:pol|set|tře|per|mapa|min|čt|half|třetina|perioda)/i.test(text)) continue;
      const m = text.match(/(\d{1,3})\s*:\s*(\d{1,3})/);
      if (m) {
        dbg(`Score found (phase A, row-scoped small): "${text}"`);
        return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
      }
    }

    const candidates = container.querySelectorAll(selectors);

    // Strategy 1: Leaf element with EXACTLY "X:Y" as its entire text
    for (const el of candidates) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (/^\d{1,3}\s*:\s*\d{1,3}$/.test(text)) {
        const m = text.match(/^(\d{1,3})\s*:\s*(\d{1,3})$/);
        if (m) {
          dbg(`Score found (strategy 1, exact element): "${text}"`);
          return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
        }
      }
    }

    // Strategy 2: Small leaf element containing "X:Y" among short text
    for (const el of candidates) {
      if (el.children.length > 0) continue;
      const text = el.textContent.trim();
      if (text.length < 1 || text.length > 15) continue;
      // Must not contain period indicators ("2.pol", "3.set" etc)
      if (/\d\.\s*(?:pol|set|tře|per|mapa|min|čt|half|třetina|perioda)/i.test(text)) continue;
      const m = text.match(/(\d{1,3})\s*:\s*(\d{1,3})/);
      if (m) {
        dbg(`Score found (strategy 2, small element): "${text}"`);
        return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
      }
    }

    // Strategy 3: Last resort — regex on preprocessed textContent
    const txt = container.textContent.toLowerCase()
      .replace(/(\d+\s*:\s*\d+?)(\d\.\s*(?:pol|set|tře|per|mapa|min|čt|čtvrt|kol|half|třetina|perioda|čtvrtina|poločas))/gi, '$1 $2');
    const m = txt.match(/(\d{1,3})\s*:\s*(\d{1,3})/);
    if (m) {
      dbg(`Score found (strategy 3, textContent fallback): "${m[0]}"`);
      return { s1: parseInt(m[1]), s2: parseInt(m[2]) };
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
      /Za\s*okamžik/i,                       // "Za okamžik" (starting soon)
      /Kurzy\s/i,                            // "Kurzy nejsou..."
      /Událost\s/i,                          // "Událost skončila..."
      /Lepší\s+ze/i,                         // "Lepší ze 3"
      /Přestávka/i,                          // Half-time
      /\d{2,}[,.]\d{2}/,                     // Odds values: "11.50", "118.00"
      /\+\d{2,}/,                            // Bet count: "+53", "+25"
      /Inquisitor/i,                         // Tipsport internal labels
      /BetBoom/i,                            // Sponsor labels
      /RushB/i,                              // Tournament labels
      /Summit/i,                             // Tournament labels
      /Probíhá/i,                            // "Probíhá" (in progress)
      /Živě/i,                               // "Živě" (live)
    ];

    let minIdx = text.length;
    for (const pattern of cutPatterns) {
      const m = text.match(pattern);
      if (m && m.index < minIdx) {
        minIdx = m.index;
      }
    }

    const cleanText = text.substring(0, minIdx).trim();
    
    // Extract detailed score: everything between team names and odds values.
    // Examples from real Tipsport data:
    //   Tennis:     "1:0 2.set - 6:2, 5:5 (*00:00) 11.18 2.62"
    //   Football:   "2:0 2. pol. - 55.min (2:0, 0:0) 1.03 15.00 60.00 +53"
    //   Esports:    "0:0 Lepší ze 3 | 1.mapa - 3:612.3421.491.mapa13.7721.201.mapa-"
    //   Basketball: "21:22 2.čt. <3min (15:13, 6:9)"
    //
    // KEY CHALLENGE: esport odds are GLUED to round scores without spaces!
    //   "3:612.34" = score "3:6" + odds "12.34"
    //
    // Strategy: scan character by character to find the first odds-like decimal
    // that is NOT a period/set/map/minute label.
    let garbage = text.substring(minIdx).trim();
    
    // Step 1: strip trailing "+NN" bet count and trailing "-"
    garbage = garbage.replace(/[-]?\s*\+\d+\s*$/g, '').trim();
    garbage = garbage.replace(/-\s*$/g, '').trim();
    
    // Step 2: strip known Czech trailing labels (football goal markets, status messages)
    garbage = garbage.replace(/Událost skončila.*$/i, '').trim();
    garbage = garbage.replace(/Kurzy nejsou.*$/i, '').trim();
    garbage = garbage.replace(/za okamžik.*$/i, '').trim();
    // Football: "1029.gól1Nikdo2", "9.gól13.10Nikdo" — strip from N.gól onwards
    garbage = garbage.replace(/\d+\.?\s*gól.*$/i, '').trim();
    // Any trailing "Nikdo", "Více", "Méně" with optional digits around them
    garbage = garbage.replace(/[\d\s]*(Nikdo|Více|Méně)[\d\s]*$/i, '').trim();
    
    // Step 3: find the first odds-like decimal and cut from there.
    // Odds are ALWAYS format: \d{1,3}\.\d{2} (e.g. "1.03", "11.85", "2.62")
    // NOT followed by common labels: set, pol, min, tř, mapa, kolo, čt, perioda
    const oddsRe = /(\d{1,3})[,.](\d{2})/g;
    let oddsMatch;
    let cutIdx = -1;
    while ((oddsMatch = oddsRe.exec(garbage)) !== null) {
      const afterOdds = garbage.substring(oddsMatch.index + oddsMatch[0].length);
      // Check if this is a period/set/map label (with optional space before label word)
      // Handles: "1.set", "2.pol", "14.min", "1.mapa", "2. mapa", "3.tř", "2.čt"
      const isLabel = /^\s*(?:set|pol|min|tř|mapa|kolo|čt|perioda|třetina|gól|s\b)/i.test(afterOdds);
      if (isLabel) continue;
      
      // Validate odds range (1.01 to 500)
      const oddsVal = parseFloat(oddsMatch[1] + '.' + oddsMatch[2]);
      if (oddsVal < 1.01 || oddsVal > 500) continue;
      
      // Skip if preceded by "(" — score context like "(15:13)"
      if (oddsMatch.index > 0 && garbage[oddsMatch.index - 1] === '(') continue;
      
      // Skip if preceded by "*" — tennis serve indicator like "(*15:00)"
      if (oddsMatch.index > 0 && garbage[oddsMatch.index - 1] === '*') continue;
      
      // This looks like a genuine odds value — cut here!
      cutIdx = oddsMatch.index;
      break;
    }
    
    if (cutIdx > 0) {
      garbage = garbage.substring(0, cutIdx).trim();
    }
    
    // Step 4: strip esport market labels that might remain at end
    // Patterns: "1.mapa", "2.mapa-", "1.mapa12", "2.mapa-" etc.
    garbage = garbage.replace(/\s*\d\.mapa[\d\s-]*$/i, '').trim();
    
    // Step 5: final cleanup of any remaining trailing decimal numbers/junk
    garbage = garbage.replace(/\s*\d{1,3}[,.]\d{2}[\s\d,.+]*$/g, '').trim();
    garbage = garbage.replace(/-\s*$/g, '').trim();
    // Strip trailing incomplete round score like "0:" or "6:" at end of esport
    // These appear when a map is in progress: "13:4, 8:13, 0:" → keep as-is, it's valid info

    return { cleanText, garbage };
  }

  function cleanName(text) {
    if (!text) return "";
    return text
      // Czech game event notifications (Tipsport shows inline)
      .replace(/GÓL\s*$/i, "")         // Strip trailing Czech "GÓL" (goal notification)
      .replace(/\bGÓL\b/gi, "")        // Strip "GÓL" anywhere in name
      .replace(/\bGOAL\b/gi, "")       // Strip English "GOAL"
      .replace(/\bŽLUTÁ\b/gi, "")     // Strip "ŽLUTÁ" (yellow card)
      .replace(/\bČERVENÁ\b/gi, "")   // Strip "ČERVENÁ" (red card)
      .replace(/\bFAUL\b/gi, "")       // Strip "FAUL"
      .replace(/\bPENALTA\b/gi, "")    // Strip "PENALTA"
      .replace(/\bTYČ\b/gi, "")        // Strip "TYČ" (post)
      .replace(/\bROH\b/gi, "")        // Strip "ROH" (corner)
      .replace(/\bOFSAJD\b/gi, "")     // Strip "OFSAJD" (offside)
      .replace(/\bAUT\b/gi, "")        // Strip "AUT" (out)
      .replace(/\bVAR\b/gi, "")        // Strip "VAR" (video review)
      // Tipsport status text that sticks to team names
      .replace(/Za\s*okamžik.*/i, "")  // "Za okamžik" (starting soon)
      .replace(/Za\s+\d+\s*min.*/i, "") // "Za 15 min" (starting in X min)
      .replace(/Přestávka.*/i, "")      // "Přestávka" half-time
      .replace(/Inquisitor.*/i, "")     // Random Tipsport labels
      .replace(/BetBoom.*/i, "")        // Sponsor labels that stick to names
      .replace(/RushB.*/i, "")          // Tournament labels
      .replace(/Summit.*/i, "")         // Tournament labels
      .replace(/(zaokamžik|inquisitor|betboom|rushb|summit|probíhá|živě).*$/i, "") // Glued lowercase labels
      .replace(/\([^)]*\)\s*$/g, "")  // Remove trailing parenthetical: "(odv.)", "(OM)", "(KSA)"
      .replace(/\(\d+\)/g, "")         // Remove seeding like "(1)"
      .replace(/^\d+\.\s*/, "")        // Remove numbering like "1. "
      // Final cleanup: if name is still > 40 chars, something stuck — cut at first suspicious word
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
        detailed_score: match.detailedScore, // NEW: Full detailed score string
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
    let skippedPrematch = 0;
    const sportCounts = {};

    for (const match of matches) {
      // SKIP eFOOTBALL/eBasketball that slipped through
      if (match.sport === 'skip') continue;

      // LIVE-ONLY FILTER — prematch is useless, our edge is LIVE score detection!
      if (!match.isLive) {
        skippedPrematch++;
        continue;
      }

      // Send odds (primary purpose)
      const oddsMsg = buildOddsMessage(match);
      if (sendJSON(oddsMsg)) sentThisScan++;

      // Send live score if available (bonus) — with score sanity check
      if (match.isLive) {
        // Score sanity gate: reject obviously garbage scores at source
        const maxScore = Math.max(match.score1, match.score2);
        const sportLimits = {
          'football': 8, 'hockey': 10, 'tennis': 7, 'basketball': 200,
          'cs2': 40, 'dota-2': 100, 'mma': 5, 'handball': 45, 'volleyball': 5,
          'esports': 50,
        };
        const limit = sportLimits[match.sport] || 999;
        if (maxScore > limit) {
          dbg(`SCORE SANITY REJECT: ${match.team1} ${match.score1}:${match.score2} ${match.team2} (${match.sport} max=${limit})`);
        } else {
          const liveMsg = buildLiveMessage(match);
          if (liveMsg && sendJSON(liveMsg)) sentThisScan++;
        }
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
      log(`Scan: ${matches.length - skippedPrematch} LIVE [${sportSummary}], sent ${sentThisScan}, skipped ${skippedPrematch} prematch`);
    }
  }

  // ====================================================================
  // INIT
  // ====================================================================

  function init() {
    const sport = detectTipsportSport();
    log(`💰 Tipsport Odds Scraper v2.3 (Row-scoped score extraction)`);
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
