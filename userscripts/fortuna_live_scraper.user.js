// ==UserScript==
// @name         Fortuna Live Scraper → Feed Hub
// @namespace    http://tampermonkey.net/
// @version      3.0
// @description  Scrape live odds/scores from ifortuna.cz live page and POST to feed-hub (crash-proof)
// @author       MiskoLive
// @match        https://www.ifortuna.cz/sazeni?filter=live*
// @match        https://www.ifortuna.cz/sazeni/*filter=live*
// @match        https://www.ifortuna.cz/sazeni*filter=live*
// @grant        GM_xmlhttpRequest
// @connect      localhost
// @connect      127.0.0.1
// @connect      10.107.109.85
// @run-at       document-idle
// ==/UserScript==

(function() {
    'use strict';

    // === CONFIG ===
    const FEED_HUB_URL = 'http://127.0.0.1:8081/fortuna';
    const POLL_INTERVAL_MS = 2200; // FAST base for live mode
    const SEND_TIMEOUT_MS = 3500;
    const FULL_SYNC_EVERY_MS = 45000;
    const DEBUG = false; // HOTFIX: avoid console/memory spam in long runs
    const MAX_MATCH_WINNER_ODDS = 80; // odds > 80 are almost certainly noise
    const DOM_COUNT_SAMPLE_MS = 10000; // HOTFIX: don't count DOM every second
    const DOM_CAP_CHECK_MS = 15000; // HOTFIX: throttle expensive cap check
    const MAX_LINKS_PER_SCAN = 600; // HOTFIX: cap per-tick DOM traversal
    const IDLE_POLL_INTERVAL_MS = 3400; // when no matches found
    const INFLIGHT_PENALTY_MS = 700; // when previous send still in flight

    // === AUTO-REFRESH CONFIG (crash-proof) ===
    // Fortuna SPA DOM bloats over time → RESULT_CODE_HUNG crash
    // Periodic reload prevents DOM bloat and keeps data fresh
    const AUTO_REFRESH_MS = 150 * 1000;       // 2.5 minutes — full page reload
    const STALE_DETECT_MS = 45 * 1000;        // 45s — if same data, refresh early
    const ZERO_MATCHES_REFRESH_MS = 25 * 1000; // 25s — if no matches found, refresh
    const DOM_ELEMENT_CAP = 8000;              // if DOM exceeds this, force refresh
    const SCROLL_BURST_COUNT = 2;              // fast burst: 2 jumps each cycle
    const SCROLL_BURST_INTERVAL_MS = 2200;     // burst every ~2.2s (LIVE fast mode)
    const SCROLL_STEP_RATIO = 1.25;            // jump 125% of viewport per step
    const SCROLL_RESET_DELAY_MS = 120;         // short pause before top reset

    // === STATE ===
    let refreshPending = false; // guard: stop processing after doPageRefresh
    let connected = false;
    let sentCount = 0;
    let lastMatchCount = 0;
    let lastSportBreakdown = '';
    let lastSendStatus = '';
    let recentLines = [];
    let autoScrollEnabled = true;
    let inFlight = false;
    let queuedData = null;
    let failStreak = 0;
    let nextAllowedSendAt = 0;
    let backoffMs = 0;
    let lastFullSyncAt = 0;
    const lastFingerprintByKey = new Map();

    // === AUTO-REFRESH STATE ===
    let refreshTimer = null;
    let refreshAt = 0;
    let lastScanHash = '';
    let staleStartedAt = 0;
    let zeroMatchesSince = 0;
    let lastScrollBurstAt = 0;
    let scrollBurstStep = 0;
    let cachedDomCount = 0;
    let lastDomCountAt = 0;
    let lastDomCapCheckAt = 0;

    function log(...args) {
        if (DEBUG) console.log('[FORTUNA]', ...args);
    }

    // ================================================================
    // FLOATING UI PANEL (same design as HLTV scraper)
    // ================================================================
    function createPanel() {
        const panel = document.createElement('div');
        panel.id = 'fortuna-panel';
        panel.style.cssText = `
            position: fixed; bottom: 10px; right: 10px; z-index: 999999;
            background: #1a1a2e; color: #0f0; font-family: 'Consolas', monospace;
            font-size: 12px; padding: 10px 14px; border-radius: 8px;
            border: 1px solid #f80; min-width: 260px; opacity: 0.92;
            box-shadow: 0 0 20px rgba(255,136,0,0.2);
            cursor: move; user-select: none;
        `;
        panel.innerHTML = `
            <div style="font-weight:bold; margin-bottom:6px; font-size:13px; color:#f80;">
                🏢 Fortuna → Feed Hub v3
            </div>
            <div id="ft-status">⏳ Initializing...</div>
            <div id="ft-sport" style="color:#aaa;">Sport: –</div>
            <div id="ft-matches">Matches: –</div>
            <div id="ft-sent" style="color:#0ff;">Sent: 0</div>
            <div id="ft-last">Last scan: –</div>
            <div id="ft-refresh" style="color:#ff0;">🔄 Refresh: –</div>
            <div id="ft-detail" style="font-size:10px;color:#8f8;margin-top:4px;max-height:100px;overflow-y:auto;white-space:pre-wrap;"></div>
            <div style="margin-top:6px; font-size:10px; color:#888;">
                <span>${FEED_HUB_URL}</span>
            </div>
            <div style="margin-top:6px;">
                <button id="ft-btn-scan" style="background:#a60;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Force Scan</button>
                <button id="ft-btn-refresh" style="background:#a80;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Refresh Now</button>
                <button id="ft-btn-scroll" style="background:#0a0;color:#fff;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;margin-right:4px;">Scroll: ON</button>
                <button id="ft-btn-debug" style="background:#333;color:#ff0;border:none;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px;">DOM Debug</button>
            </div>
        `;
        document.body.appendChild(panel);

        // Dragging
        let isDragging = false, offsetX, offsetY;
        panel.addEventListener('mousedown', (e) => {
            if (e.target.tagName === 'BUTTON') return;
            isDragging = true;
            offsetX = e.clientX - panel.getBoundingClientRect().left;
            offsetY = e.clientY - panel.getBoundingClientRect().top;
        });
        document.addEventListener('mousemove', (e) => {
            if (!isDragging) return;
            panel.style.left = (e.clientX - offsetX) + 'px';
            panel.style.top = (e.clientY - offsetY) + 'px';
            panel.style.right = 'auto';
            panel.style.bottom = 'auto';
        });
        document.addEventListener('mouseup', () => { isDragging = false; });

        // Buttons
        document.getElementById('ft-btn-scan').addEventListener('click', () => tick());
        document.getElementById('ft-btn-refresh').addEventListener('click', () => {
            location.reload();
        });
        document.getElementById('ft-btn-scroll').addEventListener('click', (e) => {
            autoScrollEnabled = !autoScrollEnabled;
            e.target.textContent = `Scroll: ${autoScrollEnabled ? 'ON' : 'OFF'}`;
            e.target.style.background = autoScrollEnabled ? '#0a0' : '#a00';
        });
        document.getElementById('ft-btn-debug').addEventListener('click', () => {
            dumpPageStructure();
            addRecentLine('📋 DOM Debug → see console (F12)');
        });
    }

    function updatePanel() {
        const el = (id) => document.getElementById(id);
        if (!el('ft-status')) return;

        // Connection status
        const statusEl = el('ft-status');
        if (connected) {
            statusEl.textContent = '✅ Connected';
            statusEl.style.color = '#0f0';
        } else if (lastSendStatus.includes('error') || lastSendStatus.includes('404')) {
            statusEl.textContent = '❌ ' + lastSendStatus;
            statusEl.style.color = '#f00';
        } else {
            statusEl.textContent = '⏳ Waiting...';
            statusEl.style.color = '#ff0';
        }

        // Sport breakdown
        if (lastSportBreakdown) {
            el('ft-sport').textContent = 'Sport: ' + lastSportBreakdown;
        }

        // Match count
        el('ft-matches').textContent = `Matches: ${lastMatchCount}`;
        el('ft-sent').textContent = `Sent: ${sentCount}`;
        el('ft-last').textContent = `Last scan: ${new Date().toLocaleTimeString()}`;
        // Refresh countdown is updated by its own 1s timer (updateRefreshCountdown)

        // Recent detail lines
        if (recentLines.length > 0) {
            el('ft-detail').innerHTML = recentLines.slice(-8).join('<br>');
        }
    }

    function addRecentLine(line) {
        recentLines.push(line);
        if (recentLines.length > 15) recentLines.shift();
    }

    function buildMatchKey(match) {
        return `${match.sport}::${(match.team1 || '').toLowerCase()}_vs_${(match.team2 || '').toLowerCase()}`;
    }

    function buildFingerprint(match) {
        const odds = (match.odds || [])
            .map(o => `${o.market || ''}|${o.label || ''}|${o.value || ''}`)
            .sort()
            .join(';');
        return `${match.score1}:${match.score2}|${match.status || ''}|${odds}`;
    }

    function getAdaptiveTickDelay() {
        const hasLiveData = lastMatchCount > 0;
        const base = hasLiveData ? POLL_INTERVAL_MS : IDLE_POLL_INTERVAL_MS;
        const inflightPenalty = inFlight ? INFLIGHT_PENALTY_MS : 0;
        return Math.min(9000, base + backoffMs + inflightPenalty);
    }

    function getDomCountCached() {
        const now = Date.now();
        if ((now - lastDomCountAt) < DOM_COUNT_SAMPLE_MS && cachedDomCount > 0) {
            return cachedDomCount;
        }
        cachedDomCount = document.querySelectorAll('*').length;
        lastDomCountAt = now;
        return cachedDomCount;
    }

    // ================================================================
    // AUTO-REFRESH — prevents DOM bloat crash (RESULT_CODE_HUNG)
    // Pattern proven by HLTV scraper v3.1
    // ================================================================
    function doPageRefresh(reason) {
        if (refreshPending) return; // already refreshing
        refreshPending = true;
        log(`🔄 Page refresh (${reason})...`);
        try {
            sessionStorage.setItem('ft_refresh_reason', reason);
            sessionStorage.setItem('ft_refresh_time', Date.now().toString());
            sessionStorage.setItem('ft_sent_count', sentCount.toString());
        } catch (e) {}
        location.reload();
    }

    function scheduleAutoRefresh() {
        if (refreshTimer) clearTimeout(refreshTimer);
        refreshAt = Date.now() + AUTO_REFRESH_MS;
        refreshTimer = setTimeout(() => doPageRefresh('auto-timer'), AUTO_REFRESH_MS);
        log(`🔄 Auto-refresh scheduled in ${AUTO_REFRESH_MS / 1000}s`);
    }

    function updateRefreshCountdown() {
        const el = document.getElementById('ft-refresh');
        if (!el) return;
        const remaining = Math.max(0, Math.round((refreshAt - Date.now()) / 1000));
        const mins = Math.floor(remaining / 60);
        const secs = remaining % 60;
        const domCount = getDomCountCached();
        el.textContent = `🔄 Refresh: ${mins}:${secs.toString().padStart(2, '0')} | DOM:${domCount} | Scroll:${autoScrollEnabled ? 'ON' : 'OFF'}`;
        if (remaining < 15) el.style.color = '#f00';
        else if (remaining < 60) el.style.color = '#ff0';
        else el.style.color = '#8f8';
    }

    function checkStaleData(matches) {
        const hash = matches.map(m =>
            `${m.team1}|${m.team2}|${m.score1}-${m.score2}`
        ).sort().join(';');

        if (hash === lastScanHash && hash.length > 0) {
            if (staleStartedAt === 0) {
                staleStartedAt = Date.now();
                log('⚠️ Stale data detected, starting timer...');
            } else if (Date.now() - staleStartedAt > STALE_DETECT_MS) {
                log('⚠️ Data stale for >45s — refreshing early');
                doPageRefresh('stale-data');
                return;
            }
        } else {
            staleStartedAt = 0;
            lastScanHash = hash;
        }
    }

    function checkZeroMatches(matchCount) {
        if (matchCount === 0) {
            if (zeroMatchesSince === 0) {
                zeroMatchesSince = Date.now();
                log('⚠️ Zero matches found, starting timer...');
            } else if (Date.now() - zeroMatchesSince > ZERO_MATCHES_REFRESH_MS) {
                log('⚠️ Zero matches for >25s — refreshing');
                doPageRefresh('zero-matches');
                return;
            }
        } else {
            zeroMatchesSince = 0;
        }
    }

    function checkDOMCap() {
        const now = Date.now();
        if (now - lastDomCapCheckAt < DOM_CAP_CHECK_MS) return;
        lastDomCapCheckAt = now;
        const count = getDomCountCached();
        if (count > DOM_ELEMENT_CAP) {
            log(`🚨 DOM element count ${count} > cap ${DOM_ELEMENT_CAP} — force refresh`);
            doPageRefresh('dom-cap-exceeded');
        }
    }

    function recoverPostReload() {
        try {
            const reason = sessionStorage.getItem('ft_refresh_reason');
            const time = sessionStorage.getItem('ft_refresh_time');
            const savedSent = sessionStorage.getItem('ft_sent_count');
            if (reason && time) {
                const elapsed = Math.round((Date.now() - parseInt(time)) / 1000);
                addRecentLine(`🔄 Reloaded (${reason}, ${elapsed}s ago)`);
                if (savedSent) sentCount = parseInt(savedSent) || 0;
                sessionStorage.removeItem('ft_refresh_reason');
                sessionStorage.removeItem('ft_refresh_time');
                sessionStorage.removeItem('ft_sent_count');
                log(`Post-reload recovery: reason=${reason}, sentCount=${sentCount}`);
            }
        } catch (e) {}
    }

    // SCROLL BURST: scroll 3 viewports quickly, then stop until next burst
    // Much less CPU/DOM stress than continuous scrolling every tick
    function autoScrollStep() {
        if (!autoScrollEnabled) return;
        const now = Date.now();

        // Only burst every SCROLL_BURST_INTERVAL_MS
        if (now - lastScrollBurstAt < SCROLL_BURST_INTERVAL_MS && scrollBurstStep >= SCROLL_BURST_COUNT) return;

        if (now - lastScrollBurstAt >= SCROLL_BURST_INTERVAL_MS) {
            scrollBurstStep = 0;
            lastScrollBurstAt = now;
        }

        if (scrollBurstStep < SCROLL_BURST_COUNT) {
            const before = document.documentElement.scrollHeight;
            const maxY = Math.max(0, before - window.innerHeight);
            const targetY = Math.min(window.scrollY + Math.floor(window.innerHeight * SCROLL_STEP_RATIO), maxY);
            window.scrollTo({ top: targetY, behavior: 'auto' });

            const nearBottom = (window.innerHeight + window.scrollY) >= (before - 24);
            if (nearBottom) {
                setTimeout(() => window.scrollTo({ top: 0, behavior: 'auto' }), SCROLL_RESET_DELAY_MS);
                scrollBurstStep = SCROLL_BURST_COUNT; // stop burst, we've scrolled to bottom
            } else {
                scrollBurstStep++;
            }
        }
    }

    // ================================================================
    // SPORT NORMALIZATION — eFotbal detection is CRITICAL here!
    // ================================================================
    function normalizeSport(rawSport) {
        const s = (rawSport || '').toLowerCase().trim();

        // eFotbal / eFootball MUST be caught BEFORE regular football!
        if (/e[-\s]?fotbal|e[-\s]?football|e[-\s]?soccer/i.test(s)) return 'efootball';
        if (/e[-\s]?hokej|e[-\s]?ice/i.test(s)) return 'eicehockey';
        if (/e[-\s]?tenis/i.test(s)) return 'etennis';
        if (/e[-\s]?basketbal/i.test(s)) return 'ebasketball';

        // Real sports
        if (s.includes('fotbal') || s.includes('soccer') || s.includes('football')) return 'football';
        if (s.includes('tenis') && !s.includes('stoln')) return 'tennis';
        if (s.includes('basketbal')) return 'basketball';
        if (s.includes('hokej') || s.includes('ice hockey')) return 'ice-hockey';
        if (s.includes('stoln') || s === 'stolni-tenis' || s.includes('table-tenis') || s.includes('table tennis') || s.includes('ping pong')) return 'table-tennis';
        if (s.includes('counter-strike') || s.includes('cs2') || s.includes('cs:go')) return 'cs2';
        if (s.includes('dota')) return 'dota-2';
        if (s.includes('league of legends') || s.includes('lol')) return 'league-of-legends';
        if (s.includes('valorant')) return 'valorant';
        if (s.includes('esport') || s.includes('e-sport')) return 'esports';
        if (s.includes('handball') || s.includes('házen')) return 'handball';
        if (s.includes('volejbal') || s.includes('volleyball')) return 'volleyball';
        if (s.includes('baseball')) return 'baseball';
        if (s.includes('rugby')) return 'rugby';
        if (s.includes('mma') || s.includes('box') || s.includes('fight')) return 'mma';
        if (s.includes('stolní tenis') || s.includes('table tennis')) return 'table-tennis';
        if (s.includes('badminton')) return 'badminton';
        if (s.includes('florbal') || s.includes('floorball')) return 'floorball';
        if (s.includes('futsal')) return 'futsal';
        if (s.includes('americký fotbal') || s.includes('american football')) return 'american-football';
        return s.replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '') || 'unknown';
    }

    // Detect eFotbal from league name (backup — catches virtual leagues under real sport section)
    function isVirtualLeague(league) {
        const l = (league || '').toLowerCase();
        return /efotbal|efootball|esoccer|e-fotbal|e-football|fifa|cyber|virtual|simulated|esports battle|esport|e-sport|gt sports/.test(l);
    }

    function isVirtualTeams(team1, team2) {
        // Detects patterns like "Liverpool (Andrew)" or "Maroko (aguuero)"
        const pat = /\([a-zA-Z0-9_]+\)/;
        return pat.test(team1) || pat.test(team2);
    }

    function parseOddsValue(str) {
        if (!str) return null;
        const cleaned = str.replace(',', '.').replace(/[^\d.]/g, '');
        const val = parseFloat(cleaned);
        return (isNaN(val) || val <= 1.0 || val > 500) ? null : val;
    }

    function looksLikeTennisCard(lines) {
        const joined = (lines || []).join(' ');
        if (!joined) return false;
        if (/\b\d+\s*\.?\s*set\b/i.test(joined)) return true;
        if (/\(\s*\d{1,2}\s*[:.]\s*\d{1,2}\*?\s*\)/.test(joined)) return true; // point score in parentheses
        if (/\bTB\b/i.test(joined)) return true;
        return false;
    }

    /**
     * Tennis score extraction: find the SET score from Fortuna DOM text.
     *
     * CRITICAL: Fortuna SPA renders everything as ONE concatenated string
     * with NO newlines/spaces between elements. Example:
     *   "2. setGarin C.Baez S.6720404001Vítěz zápasuGarin C.3.05..."
     *
     * Structure: <N. set><Team1><Team2><digits_concatenated><Vítěz...>
     * The digits block contains: s1_games + s2_games + ... + points + SETS_WON_1 + SETS_WON_2
     *
     * Strategy: Use team names (already extracted) to locate the digit block,
     * then extract individual digits. The LAST two single digits before "Vítěz"
     * are sets_won_team1 and sets_won_team2.
     */
    function extractTennisSetScore(lines, team1, team2) {
        // Join all lines into one string (usually already one line)
        const fullText = lines.join('');

        // Find where team names appear in the text
        const t1Idx = fullText.indexOf(team1);
        const t2Idx = fullText.indexOf(team2);
        if (t1Idx < 0 || t2Idx < 0) {
            log('[Tennis] Cannot find team names in fullText');
            return null;
        }

        // The digit block starts after both team names
        const afterTeams = Math.max(t1Idx + team1.length, t2Idx + team2.length);

        // Find where the digit block ends (before "Vítěz" or odds like "1.53")
        const rest = fullText.slice(afterTeams);
        const endMatch = rest.match(/[Vv]ítěz|Výsledek|\d+\.\d{2}/);
        const digitBlock = endMatch ? rest.slice(0, endMatch.index) : rest.slice(0, 30);

        log(`[Tennis] digitBlock for ${team1} vs ${team2}: "${digitBlock}"`);

        // Extract all single digits from the block
        // The block is like "6720404001" → individual chars that are digits
        const allDigits = [];
        for (const ch of digitBlock) {
            if (/\d/.test(ch)) allDigits.push(parseInt(ch));
        }

        if (allDigits.length < 2) {
            log('[Tennis] Not enough digits in block');
            return null;
        }

        // The LAST two digits are sets_won for team1 and team2
        // Format: ...game_scores...points...SETS1 SETS2
        const s1 = allDigits[allDigits.length - 2];
        const s2 = allDigits[allDigits.length - 1];

        // Sanity: set scores ≤ 5
        if (s1 > 5 || s2 > 5) {
            log(`[Tennis] Set scores too high: ${s1}:${s2}, rejecting`);
            return null;
        }

        log(`[Tennis] Extracted set score: ${s1}:${s2} (from ${allDigits.length} digits)`);
        return { score1: s1, score2: s2 };
    }

    function tryRepairFootballScore(lines) {
        const ints = lines
            .filter(l => /^\d{1,2}$/.test(l))
            .map(v => parseInt(v, 10));

        // Prefer small realistic pair (0..7)
        for (let i = 0; i < ints.length - 1; i++) {
            const a = ints[i], b = ints[i + 1];
            if (a >= 0 && a <= 7 && b >= 0 && b <= 7) {
                return { score1: a, score2: b, repaired: true };
            }
        }
        return null;
    }

    function sanitizeScoreBySport(sport, score1, score2, lines) {
        const s = (sport || '').toLowerCase();
        let a = Number.isFinite(score1) ? score1 : 0;
        let b = Number.isFinite(score2) ? score2 : 0;

        if (s === 'football') {
            // Real football: very high live score is almost always parser noise
            if (a > 7 || b > 7 || a < 0 || b < 0) {
                const repaired = tryRepairFootballScore(lines || []);
                if (repaired) return { score1: repaired.score1, score2: repaired.score2, repaired: true, dropped: false };
                return { score1: 0, score2: 0, repaired: false, dropped: true };
            }
        }

        if (s === 'ice-hockey') {
            if (a > 15 || b > 15 || a < 0 || b < 0) {
                return { score1: 0, score2: 0, repaired: false, dropped: true };
            }
        }

        if (s === 'tennis') {
            // Tennis scores are set scores (0..3 for Bo3, 0..5 for Bo5).
            // If > 5, the extraction failed and generic parser grabbed game scores.
            if (a > 5 || b > 5) {
                return { score1: 0, score2: 0, repaired: false, dropped: true };
            }
        }

        return { score1: a, score2: b, repaired: false, dropped: false };
    }

    // ================================================================
    // DOM DISCOVERY — dump page structure for debugging
    // ================================================================
    function dumpPageStructure() {
        log('=== PAGE STRUCTURE DUMP ===');
        log('Title:', document.title);
        log('URL:', location.href);

        // Count interesting elements
        const counts = {};
        ['h1','h2','h3','h4','section','article','header','button','a'].forEach(tag => {
            counts[tag] = document.querySelectorAll(tag).length;
        });
        log('Element counts:', JSON.stringify(counts));

        // Find elements with interesting class patterns
        const classPatterns = ['sport','match','fixture','event','odds','league','score',
                               'participant','team','competitor','market','card','offer','live'];
        classPatterns.forEach(pat => {
            const els = document.querySelectorAll(`[class*="${pat}"]`);
            if (els.length > 0) {
                const classes = new Set();
                els.forEach(el => {
                    (el.className || '').split(/\s+/).forEach(c => {
                        if (c.toLowerCase().includes(pat)) classes.add(c);
                    });
                });
                log(`[class*="${pat}"] (${els.length} els):`, [...classes].slice(0,10).join(', '));
            }
        });

        // Find data-* attributes
        const dataEls = document.querySelectorAll('[data-testid], [data-test], [data-testing-selector], [data-cy], [data-id]');
        if (dataEls.length > 0) {
            const attrs = new Set();
            dataEls.forEach(el => {
                Array.from(el.attributes).forEach(a => {
                    if (a.name.startsWith('data-test') || a.name.startsWith('data-cy') || a.name === 'data-id') {
                        attrs.add(`${a.name}="${a.value}"`);
                    }
                });
            });
            log('Data test attrs:', [...attrs].slice(0,20).join(', '));
        }

        // Dump hrefs for match-like links
        const links = document.querySelectorAll('a[href*="/sazeni/"]');
        log(`Links with /sazeni/: ${links.length}`);
        Array.from(links).slice(0, 8).forEach((l, i) => {
            const href = l.getAttribute('href') || '';
            const text = (l.textContent || '').replace(/\s+/g, ' ').trim().substring(0, 120);
            log(`  [${i}] href="${href}" text="${text}"`);
            // Dump child structure
            const children = [];
            l.querySelectorAll('*').forEach(c => {
                const cls = c.className ? `.${(c.className+'').split(/\s+/).join('.')}` : '';
                const tag = c.tagName.toLowerCase();
                const txt = (c.childNodes.length === 1 && c.childNodes[0].nodeType === 3)
                    ? ` "${c.textContent.trim().substring(0,30)}"` : '';
                children.push(`${tag}${cls}${txt}`);
            });
            if (children.length <= 30) {
                log(`    children: ${children.join(' > ')}`);
            } else {
                log(`    children (${children.length} total): ${children.slice(0,15).join(', ')} ...`);
            }
        });

        // Look at h2/h3 headers that might be sport/league
        document.querySelectorAll('h2, h3').forEach((el, i) => {
            if (i < 10) {
                const text = (el.textContent || '').trim().substring(0, 60);
                const cls = el.className || '';
                log(`  <${el.tagName} class="${cls}"> "${text}"`);
            }
        });

        log('=== END DUMP ===');
    }

    // ================================================================
    // MAIN SCRAPE — multi-strategy approach
    // ================================================================
    function scrapeAll() {
        const results = [];
        const seen = new Set(); // dedup by team combo

        // === STRATEGY 1: Find match links by href pattern ===
        // Fortuna match URLs: /sazeni/<sport>/<league>/<team1-vs-team2-id>
        const matchLinks = document.querySelectorAll('a[href*="/sazeni/"]');
        const limitedLinks = Array.from(matchLinks).slice(0, MAX_LINKS_PER_SCAN);
        log(`Strategy 1: ${matchLinks.length} links with /sazeni/ (processing ${limitedLinks.length})`);

        for (const link of limitedLinks) {
            const href = link.getAttribute('href') || '';
            // Skip navigation/category links (too few path segments)
            const pathParts = href.replace(/^\//, '').split('/');
            if (pathParts.length < 3) continue; // need at least sazeni/sport/league/match

            const text = (link.textContent || '').trim();
            // Match links should have substantial content (team names + odds)
            if (text.length < 15 || text.length > 5000) continue;

            // Must contain at least one decimal number (odds)
            if (!/\d+[.,]\d{1,2}/.test(text)) continue;

            // Detect sport from URL path
            let sport = 'unknown';
            const sazIdx = pathParts.indexOf('sazeni');
            if (sazIdx >= 0 && pathParts[sazIdx + 1]) {
                sport = normalizeSport(decodeURIComponent(pathParts[sazIdx + 1]));
            }

            // Detect league from URL path
            let league = '';
            if (sazIdx >= 0 && pathParts[sazIdx + 2]) {
                league = decodeURIComponent(pathParts[sazIdx + 2]).replace(/-/g, ' ');
            }

            const match = parseMatchFromElement(link, sport, league);
            if (match) {
                const key = `${match.team1}|${match.team2}`.toLowerCase();
                if (!seen.has(key)) {
                    seen.add(key);
                    // Override sport if league is virtual
                    if (isVirtualLeague(match.league) && match.sport === 'football') {
                        match.sport = 'efootball';
                    }
                    if (match.sport === 'football' && isVirtualTeams(match.team1, match.team2)) {
                        match.sport = 'efootball';
                    }
                    results.push(match);
                }
            }
        }

        // === STRATEGY 2: Find by known test/data attributes ===
        if (results.length === 0) {
            log('Strategy 2: searching by data attributes...');
            const selectors = [
                '[data-testing-selector*="Fixture"]',
                '[data-testid*="match"]',
                '[data-testid*="event"]',
                '[data-test*="match"]',
                '[data-cy*="match"]',
            ];
            for (const sel of selectors) {
                try {
                    const els = document.querySelectorAll(sel);
                    if (els.length > 0) {
                        log(`  Found ${els.length} via ${sel}`);
                        for (const el of els) {
                            const match = parseMatchFromElement(el, 'unknown', '');
                            if (match) {
                                const key = `${match.team1}|${match.team2}`.toLowerCase();
                                if (!seen.has(key)) {
                                    seen.add(key);
                                    if (match.sport === 'football' && isVirtualTeams(match.team1, match.team2)) {
                                        match.sport = 'efootball';
                                    }
                                    results.push(match);
                                }
                            }
                        }
                    }
                } catch(e) {}
            }
        }

        // === STRATEGY 3: Find by class name patterns ===
        if (results.length === 0) {
            log('Strategy 3: searching by class patterns...');
            const classSelectors = [
                '[class*="fixture"]',
                '[class*="match-card"]',
                '[class*="event-card"]',
                '[class*="offer-card"]',
                '[class*="MatchRow"]',
                '[class*="EventRow"]',
            ];
            for (const sel of classSelectors) {
                try {
                    const els = document.querySelectorAll(sel);
                    if (els.length > 0) {
                        log(`  Found ${els.length} via ${sel}`);
                        for (const el of els) {
                            const match = parseMatchFromElement(el, 'unknown', '');
                            if (match) {
                                const key = `${match.team1}|${match.team2}`.toLowerCase();
                                if (!seen.has(key)) {
                                    seen.add(key);
                                    if (match.sport === 'football' && isVirtualTeams(match.team1, match.team2)) {
                                        match.sport = 'efootball';
                                    }
                                    results.push(match);
                                }
                            }
                        }
                    }
                } catch(e) {}
            }
        }

        // Detect sport from nearest sport header for matches that have 'unknown'
        results.forEach(m => {
            if (m.sport === 'unknown') {
                m.sport = detectSportFromDOM(m._element) || 'unknown';
                delete m._element;
            } else {
                delete m._element;
            }
        });

        return results;
    }

    // ================================================================
    // PARSE A MATCH FROM ANY ELEMENT
    // ================================================================
    function parseMatchFromElement(el, sport, league) {
        const fullText = (el.textContent || '').trim();
        if (!fullText || fullText.length < 10) return null;

        const lines = fullText.split('\n').map(l => l.trim()).filter(l => l.length > 0);

        // --- Extract Team Names ---
        let team1 = '', team2 = '';
        let score1 = null, score2 = null;

        // Method A: structured participant elements
        const participantSelectors = [
            '[class*="participant"]', '[class*="team-name"]', '[class*="competitor"]',
            '[class*="Participant"]', '[class*="TeamName"]', '[class*="Competitor"]',
        ];
        for (const sel of participantSelectors) {
            const els = el.querySelectorAll(sel);
            if (els.length >= 2) {
                team1 = cleanTeamName(els[0].textContent);
                team2 = cleanTeamName(els[1].textContent);
                break;
            }
        }

        // Method B: parse from lines — look for two non-numeric, non-status lines
        if (!team1 || !team2) {
            const extracted = extractTeamsFromLines(lines);
            if (extracted) {
                team1 = extracted.team1;
                team2 = extracted.team2;
                if (extracted.score1 !== null) score1 = extracted.score1;
                if (extracted.score2 !== null) score2 = extracted.score2;
            }
        }

        // Method C: try from href (last resort)
        if (!team1 || !team2) {
            const href = el.getAttribute('href') || el.closest('a')?.getAttribute('href') || '';
            const slugMatch = href.match(/([a-z0-9-]+)-vs-([a-z0-9-]+)/i);
            if (slugMatch) {
                team1 = team1 || slugMatch[1].replace(/-/g, ' ');
                team2 = team2 || slugMatch[2].replace(/-/g, ' ');
            }
        }

        if (!team1 || !team2) return null;

        const preParsedScore1 = score1;
        const preParsedScore2 = score2;

        // --- Tennis: extract SET score FIRST (before generic score parser) ---
        // Tennis DOM contains multiple score types (sets, games, points).
        // Generic parser grabs first numbers which are often game scores (6, 3).
        // Tennis needs the SET score specifically.
        const tennisLike = sport === 'tennis' || (sport === 'unknown' && looksLikeTennisCard(lines));
        if (tennisLike && sport === 'unknown') {
            sport = 'tennis';
            log(`Sport inferred as tennis from card text: ${team1} vs ${team2}`);
        }

        if (tennisLike) {
            const tennisScore = extractTennisSetScore(lines, team1, team2);
            if (tennisScore) {
                score1 = tennisScore.score1;
                score2 = tennisScore.score2;
            } else if (
                Number.isFinite(preParsedScore1) &&
                Number.isFinite(preParsedScore2) &&
                preParsedScore1 >= 0 && preParsedScore1 <= 5 &&
                preParsedScore2 >= 0 && preParsedScore2 <= 5
            ) {
                score1 = preParsedScore1;
                score2 = preParsedScore2;
                log(`Tennis fallback score used: ${team1} ${score1}-${score2} ${team2}`);
            } else {
                // Can't find set score → send 0:0 rather than garbage game scores
                score1 = 0;
                score2 = 0;
            }
        }

        // --- Extract Scores (non-tennis) ---
        if (score1 === null) {
            const scoreSelectors = [
                '[class*="score"]', '[class*="Score"]',
                '[class*="scoreboard"]', '[class*="Scoreboard"]',
            ];
            for (const sel of scoreSelectors) {
                const scoreEls = el.querySelectorAll(sel);
                // Look for elements that contain just a number
                const nums = [];
                scoreEls.forEach(se => {
                    const t = se.textContent.trim();
                    if (/^\d{1,2}$/.test(t)) nums.push(parseInt(t));
                });
                if (nums.length >= 2) {
                    score1 = nums[0]; score2 = nums[1]; break;
                }
            }
        }

        // Fallback: find "N - N" or "N : N" pattern in text
        if (score1 === null) {
            // Look for standalone numbers near team names in the lines
            for (let i = 0; i < lines.length && i < 20; i++) {
                const m = lines[i].match(/^(\d{1,2})\s*[-:]\s*(\d{1,2})$/);
                if (m) { score1 = parseInt(m[1]); score2 = parseInt(m[2]); break; }
            }
        }
        if (score1 === null) score1 = 0;
        if (score2 === null) score2 = 0;

        const sanitizedScore = sanitizeScoreBySport(sport, score1, score2, lines);
        if (sanitizedScore.repaired) {
            log(`Score repaired [${sport}] ${team1} ${score1}-${score2} ${team2} -> ${sanitizedScore.score1}-${sanitizedScore.score2}`);
        } else if (sanitizedScore.dropped) {
            log(`Score sanitized [${sport}] ${team1} ${score1}-${score2} ${team2} -> 0-0`);
        }
        score1 = sanitizedScore.score1;
        score2 = sanitizedScore.score2;

        // --- Extract Status ---
        let status = 'LIVE';
        const statusSelectors = [
            '[class*="live"]', '[class*="Live"]', '[class*="status"]',
            '[class*="Status"]', '[class*="period"]', '[class*="time"]',
        ];
        for (const sel of statusSelectors) {
            const el2 = el.querySelector(sel);
            if (el2) {
                const t = el2.textContent.trim();
                if (t.length > 0 && t.length < 30) { status = t; break; }
            }
        }
        // Text-based status detection
        if (status === 'LIVE') {
            for (const line of lines) {
                if (/přestávka/i.test(line)) { status = line; break; }
                if (/\d+\.\s*pol/i.test(line)) { status = line; break; }
                if (/poločas/i.test(line)) { status = line; break; }
                if (/prodloužen/i.test(line)) { status = line; break; }
                if (/^\d{1,3}['m]/.test(line)) { status = line; break; }
            }
        }

        // --- Extract Odds ---
        const rawOdds = [];

        // Method A: find button/clickable elements with odds values
        const buttons = el.querySelectorAll('button, [role="button"], [class*="odds"], [class*="Odds"]');
        for (const btn of buttons) {
            const btnText = btn.textContent.trim();
            const btnLines = btnText.split('\n').map(s => s.trim()).filter(s => s);

            // Pattern: "Label\nValue" (e.g. "Bologna\n1.42" or "Remíza\n3.65")
            let label = '', value = null;
            for (const part of btnLines) {
                const v = parseOddsValue(part);
                if (v) { value = v; }
                else if (part.length > 1 && part.length < 40) { label = part; }
            }

            // Concatenated text fix: Fortuna SPA renders "TeamName1.42" as one string
            // without newlines. Split label from trailing odds value.
            if (value && !label && btnLines.length === 1) {
                const cMatch = btnLines[0].match(/^(.+?)(\d+[.,]\d{1,2})\s*$/);
                if (cMatch && cMatch[1].trim().length >= 1) {
                    label = cMatch[1].trim();
                }
            }

            if (value) {
                rawOdds.push({ market: 'match_winner', label, value });
            }
        }

        // Method B: if no buttons found, scan text for decimal patterns
        if (rawOdds.length === 0) {
            for (let i = 0; i < lines.length; i++) {
                const line = lines[i];
                // Skip known non-odds patterns
                if (/přestávka|pol\.|poločas|výsledek/i.test(line)) continue;
                // Match decimal odds like 1.42, 3,65, 11.00
                const oddsMatch = line.match(/^(\d+[.,]\d{1,2})$/);
                if (oddsMatch) {
                    const val = parseOddsValue(oddsMatch[1]);
                    if (val) {
                        // Look back for label
                        const label = (i > 0 && !parseOddsValue(lines[i-1]) && lines[i-1].length < 40)
                            ? lines[i-1] : '';
                        rawOdds.push({ market: 'match_winner', label, value: val });
                    }
                }
            }
        }

        // === POST-PROCESS: filter draw odds, non-match-winner markets, noise ===
        const odds = [];
        for (const o of rawOdds) {
            const lab = (o.label || '').trim();
            const labLower = lab.toLowerCase();
            // Skip draw labels (1X2 middle button) — this was causing ~60% wrong odds
            if (/^(remíza|nerozhodn[ěe]?|draw|x|tie)$/i.test(lab)) {
                log(`  [odds] SKIP draw: "${lab}" ${o.value}`);
                continue;
            }
            // Skip over/under, handicap, and other non-match-winner labels
            if (/přes\s+\d|pod\s+\d|handicap|hendikep|skóre|celkem|gól[ůuy]?|počet/i.test(labLower)) continue;
            // Skip sub-market labels: "Vítěz 2. setu", "Vítěz 1. poloviny"
            if (/vítěz\s+\d/i.test(labLower)) continue;
            // Skip noise odds (>80 is almost certainly not a real match_winner)
            if (o.value > MAX_MATCH_WINNER_ODDS) {
                log(`  [odds] SKIP noise: "${lab}" ${o.value} (>MAX)`);
                continue;
            }
            odds.push(o);
        }

        // Smart selection: try to find home/away by team name matching
        let finalOdds = odds;
        if (odds.length > 2) {
            const t1p = team1.toLowerCase().substring(0, Math.min(5, team1.length));
            const t2p = team2.toLowerCase().substring(0, Math.min(5, team2.length));
            const home = odds.find(o => {
                const l = (o.label || '').toLowerCase();
                return l === '1' || (t1p.length >= 3 && l.includes(t1p));
            });
            const away = odds.find(o => {
                const l = (o.label || '').toLowerCase();
                return l === '2' || (t2p.length >= 3 && l.includes(t2p));
            });
            if (home && away && home !== away) {
                finalOdds = [home, away];
                log(`  [odds] Matched by team names: ${home.label}=${home.value}, ${away.label}=${away.value}`);
            } else {
                // Fallback: first + last (Fortuna order: home/draw/away → after draw filter: home/away)
                finalOdds = [odds[0], odds[odds.length - 1]];
                log(`  [odds] Fallback first+last: ${odds[0].value}, ${odds[odds.length-1].value}`);
            }
        }

        return {
            sport,
            league: league || '',
            team1,
            team2,
            score1,
            score2,
            status,
            odds: finalOdds,
            _element: el // kept temporarily for sport detection, deleted later
        };
    }

    function cleanTeamName(text) {
        return (text || '').trim()
            .replace(/^\d+\s+/, '') // remove leading rankings only if followed by space
            .replace(/\s+[0-9]{1,2}$/, '') // remove trailing score only if separated by space
            .replace(/\s+/g, ' ')
            .trim();
    }

    function extractTeamsFromLines(lines) {
        // Fortuna text pattern for a match card:
        // Lines contain: team1, score1, team2, score2, status, "Výsledek zápasu", odds...
        // Strategy: find pairs of lines that look like team names (non-numeric, non-status)

        let team1 = '', team2 = '';
        let score1 = null, score2 = null;
        let foundFirst = false;

        for (let i = 0; i < Math.min(lines.length, 20); i++) {
            const line = lines[i];
            if (isTeamNameLine(line)) {
                if (!foundFirst) {
                    team1 = line;
                    foundFirst = true;
                    // Check if next line is a score
                    if (i + 1 < lines.length && /^\d{1,2}$/.test(lines[i+1])) {
                        score1 = parseInt(lines[i+1]);
                    }
                } else {
                    team2 = line;
                    // Check if next line is a score
                    if (i + 1 < lines.length && /^\d{1,2}$/.test(lines[i+1])) {
                        score2 = parseInt(lines[i+1]);
                    }
                    break; // got both teams
                }
            }
        }

        if (team1 && team2) {
            return { team1, team2, score1, score2 };
        }
        return null;
    }

    function isTeamNameLine(line) {
        if (!line || line.length < 2 || line.length > 60) return false;
        // Not a number
        if (/^\d+[.,]?\d*$/.test(line)) return false;
        // Not a known status/label word
        if (/^(přestávka|live|pauza|výsledek|remíza|\d+\.\s*pol|poločas|prodloužen)/i.test(line)) return false;
        // Not "Výsledek zápasu"
        if (/výsledek\s+zápasu/i.test(line)) return false;
        // Not just a time like "45m" or "37m"
        if (/^\d+[m':]/.test(line)) return false;
        // Not tennis period indicators: "1. set", "2. set", etc.
        if (/^\d+\.\s*set\b/i.test(line)) return false;
        // Not market/bet labels: "Vítěz 2. gamu v 2. setu", "Vítěz zápasu", etc.
        if (/^vítěz\b/i.test(line)) return false;
        // Not tennis market fragments containing set/game references
        if (/\bsetu\b|\bgamu\b|\bgame\b|\bset\b.*\bwin/i.test(line)) return false;
        // Has at least one letter
        if (!/[a-záčďéěíňóřšťúůýž]/i.test(line)) return false;
        return true;
    }

    // Try to find sport from ancestor elements in the DOM
    function detectSportFromDOM(el) {
        if (!el) return null;

        // Walk up and look for sport section headers
        let node = el.parentElement;
        let depth = 0;
        while (node && depth < 20) {
            // Check if this node or its preceding siblings contain a sport header
            const headerEl = node.querySelector('h2, h3, [class*="sport-header"], [class*="SportHeader"]');
            if (headerEl) {
                const text = (headerEl.textContent || '').trim();
                if (text.length < 30 && looksLikeSportName(text)) {
                    return normalizeSport(text);
                }
            }

            // Check preceding siblings
            let sib = node.previousElementSibling;
            let sibDepth = 0;
            while (sib && sibDepth < 5) {
                const text = (sib.textContent || '').trim();
                if (text.length < 30 && looksLikeSportName(text)) {
                    return normalizeSport(text);
                }
                sib = sib.previousElementSibling;
                sibDepth++;
            }

            node = node.parentElement;
            depth++;
        }
        return null;
    }

    function looksLikeSportName(text) {
        const t = text.toLowerCase();
        return /fotbal|hokej|tenis|basketbal|esport|efotbal|counter|dota|league of|valorant|handball|házen|volejbal|baseball|rugby|mma|box|florbal|futsal/.test(t);
    }

    // ================================================================
    // SEND TO FEED HUB
    // ================================================================
    function sendToFeedHub(data) {
        const now = Date.now();
        if (!data || data.length === 0) return;

        if (now < nextAllowedSendAt) {
            queuedData = data;
            return;
        }

        if (inFlight) {
            queuedData = data;
            return;
        }

        inFlight = true;

        const payload = {
            timestamp: now,
            source: 'fortuna',
            matches: data
        };

        const jsonStr = JSON.stringify(payload);
        log(`Sending ${data.length} matches (${(jsonStr.length/1024).toFixed(1)}KB)...`);

        // Log first few matches for debug
        data.slice(0, 5).forEach((m, i) => {
            const oddsStr = m.odds.map(o => `${o.label||'?'}:${o.value}`).join(', ');
            log(`  [${i}] ${m.sport} | ${m.team1} ${m.score1}-${m.score2} ${m.team2} | ${m.status} | odds: ${oddsStr}`);
            addRecentLine(`${m.team1} ${m.score1}-${m.score2} ${m.team2}`);
        });

        if (typeof GM_xmlhttpRequest !== 'undefined') {
            GM_xmlhttpRequest({
                method: 'POST',
                url: FEED_HUB_URL,
                headers: { 'Content-Type': 'application/json' },
                data: jsonStr,
                timeout: SEND_TIMEOUT_MS,
                onload: function(resp) {
                    log(`→ ${resp.status} ${resp.responseText.substring(0, 200)}`);
                    if (resp.status >= 200 && resp.status < 300) {
                        connected = true;
                        sentCount++;
                        lastSendStatus = `OK (${resp.status})`;
                        failStreak = 0;
                        backoffMs = 0;
                        nextAllowedSendAt = Date.now();
                    } else {
                        connected = false;
                        lastSendStatus = `error ${resp.status}`;
                        failStreak++;
                        backoffMs = Math.min(7000, 700 * failStreak);
                        nextAllowedSendAt = Date.now() + backoffMs;
                    }
                    inFlight = false;
                    updatePanel();
                    if (queuedData && Date.now() >= nextAllowedSendAt) {
                        const queued = queuedData;
                        queuedData = null;
                        sendToFeedHub(queued);
                    }
                },
                ontimeout: function() {
                    connected = false;
                    failStreak++;
                    backoffMs = Math.min(7000, 700 * failStreak);
                    nextAllowedSendAt = Date.now() + backoffMs;
                    lastSendStatus = `error: timeout ${SEND_TIMEOUT_MS}ms`;
                    inFlight = false;
                    updatePanel();
                },
                onerror: function(err) {
                    log('Send error:', err.statusText || err);
                    connected = false;
                    const detail = err?.status ? `${err.status}` : (err?.statusText || err?.error || 'fetch failed');
                    lastSendStatus = 'error: ' + detail;
                    failStreak++;
                    backoffMs = Math.min(7000, 700 * failStreak);
                    nextAllowedSendAt = Date.now() + backoffMs;
                    inFlight = false;
                    updatePanel();
                }
            });
        } else {
            fetch(FEED_HUB_URL, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: jsonStr,
                keepalive: false
            })
            .then(r => {
                if (r.ok) {
                    connected = true;
                    sentCount++;
                    lastSendStatus = `OK (${r.status})`;
                    failStreak = 0;
                    backoffMs = 0;
                    nextAllowedSendAt = Date.now();
                } else {
                    connected = false;
                    lastSendStatus = `error ${r.status}`;
                    failStreak++;
                    backoffMs = Math.min(7000, 700 * failStreak);
                    nextAllowedSendAt = Date.now() + backoffMs;
                }
                return r.text().then(t => log(`→ ${r.status} ${t.substring(0, 200)}`));
            })
            .catch(e => {
                log('Send error:', e);
                connected = false;
                lastSendStatus = 'error: ' + (e?.message || 'fetch failed');
                failStreak++;
                backoffMs = Math.min(7000, 700 * failStreak);
                nextAllowedSendAt = Date.now() + backoffMs;
            })
            .finally(() => {
                inFlight = false;
                updatePanel();
                if (queuedData && Date.now() >= nextAllowedSendAt) {
                    const queued = queuedData;
                    queuedData = null;
                    sendToFeedHub(queued);
                }
            });
        }
    }

    // ================================================================
    // MAIN TICK
    // ================================================================
    function tick() {
        if (refreshPending) return; // page is about to reload, skip processing
        try {
            autoScrollStep();

            // DOM cap check — prevents RESULT_CODE_HUNG crash
            checkDOMCap();
            if (refreshPending) return; // checkDOMCap may trigger refresh

            const data = scrapeAll();
            lastMatchCount = data.length;
            log(`Scraped: ${data.length} matches`);

            // Summary by sport
            const bySport = {};
            data.forEach(m => { bySport[m.sport] = (bySport[m.sport] || 0) + 1; });
            if (Object.keys(bySport).length > 0) {
                lastSportBreakdown = Object.entries(bySport).map(([s,c]) => `${s}:${c}`).join(', ');
                log('  By sport:', lastSportBreakdown);
            }

            if (data.length > 0) {
                // Stale data detection — refresh if same data for >45s
                checkStaleData(data);
                zeroMatchesSince = 0; // reset zero-matches timer
                const now = Date.now();
                const isFullSync = (now - lastFullSyncAt) >= FULL_SYNC_EVERY_MS;
                const delta = [];

                for (const match of data) {
                    const key = buildMatchKey(match);
                    const fingerprint = buildFingerprint(match);
                    const prev = lastFingerprintByKey.get(key);
                    if (isFullSync || prev !== fingerprint) {
                        delta.push(match);
                        lastFingerprintByKey.set(key, fingerprint);
                    }
                }

                if (isFullSync) {
                    lastFullSyncAt = now;
                }

                if (delta.length > 0) {
                    sendToFeedHub(delta);
                } else {
                    lastSendStatus = 'no delta';
                }
            } else {
                // Zero matches — auto-refresh after 25s
                checkZeroMatches(0);
                lastSendStatus = 'no matches found';
                log('No matches found. Waiting for next tick...');
            }
            updatePanel();
        } catch(e) {
            log('Scrape error:', e);
            lastSendStatus = 'scrape error: ' + e.message;
            updatePanel();
        }
    }

    // ================================================================
    // INIT
    // ================================================================
    log('Fortuna Live Scraper v3.0 initialized (crash-proof)');
    log('Target URL:', FEED_HUB_URL);
    log('Poll interval:', POLL_INTERVAL_MS + 'ms');
    log('Auto-refresh:', AUTO_REFRESH_MS / 1000 + 's');
    log('Stale detect:', STALE_DETECT_MS / 1000 + 's');
    log('DOM cap:', DOM_ELEMENT_CAP);
    log('Starting in 4s (waiting for SPA to render)...');

    // Create floating panel immediately
    createPanel();

    // Recover state from previous reload
    recoverPostReload();

    // Schedule auto-refresh timer (prevents DOM bloat crash)
    scheduleAutoRefresh();

    // Start countdown display (updates every second)
    setInterval(updateRefreshCountdown, 1000);

    setTimeout(() => {
        tick(); // First run
        const scheduleNext = () => {
            setTimeout(() => {
                tick();
                scheduleNext();
            }, getAdaptiveTickDelay());
        };
        scheduleNext();
    }, 4000);

})();
