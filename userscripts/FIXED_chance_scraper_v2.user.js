// ==UserScript==
// @name         Chance.cz → Feed Hub Live Scraper (FIXED v2)
// @namespace    rustmisko
// @version      2.1
// @description  Čistý parser pro Chance.cz + WS ingest do feed-hubu (fotbal/tenis/basket + CS2/LoL/Dota/Valorant)
// @author       RustMisko
// @match        https://www.chance.cz/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

(function() {
    'use strict';

        // ====================================================================
        // CONFIG
        // ====================================================================
        const WS_URL = "ws://localhost:8080/feed";
        const SCAN_INTERVAL_MS = 2000;
        const RECONNECT_MS = 5000;
        const HEARTBEAT_MS = 20000;
        // feed-hub staleness cleanup je 120s; když se skóre/kurzy nehýbou, musíme občas refreshnout i beze změny
        const RESEND_UNCHANGED_MS = 20000;
        const SOURCE_NAME = "chance";
        const BOOKMAKER = "chance";
        const DEBUG = false;

        let ws = null;
        let connected = false;
        let sentCount = 0;
        let errorCount = 0;
        let scanTimer = null;
        let hbTimer = null;

        const lastSendStateByKey = new Map();
        let lastResults = [];

        const PREFIX = "[Chance→Hub FIXEDv2]";
        function log(...args) { console.log(PREFIX, ...args); }
        function dbg(...args) { if (DEBUG) console.log(PREFIX, "[DBG]", ...args); }

    // Konfigurace sportů, o které máme zájem
    const ALLOWED_ESPORTS = ['counter strike', 'league of legends', 'dota', 'valorant', 'cs2', 'cs:go', 'csgo', 'lol'];
    const BANNED_ESPORTS = ['efootball', 'ebasketball', 'esports battle', 'etenis', 'ehokej']; // z předchozích pravidel
    
        // ====================================================================
        // UI PANEL
        // ====================================================================
        function createPanel() {
        const panel = document.createElement("div");
                panel.id = "chance-fixedv2-panel";
        panel.style.cssText = `
          position: fixed; bottom: 10px; right: 10px; z-index: 999999;
          background: #111; color: #0f0; font-family: 'Consolas', monospace;
          font-size: 11px; padding: 10px; border-radius: 8px;
          border: 1px solid #0f0; width: 450px; max-height: 500px;
          overflow-y: auto; opacity: 0.95;
        `;
        panel.innerHTML = `
          <div style="display:flex; justify-content:space-between; margin-bottom: 5px; border-bottom: 1px solid #333; padding-bottom: 5px;">
                        <b>Chance → Feed Hub (FIXED v2)</b>
                        <button id="tc-run" style="background:#0f0; color:#000; border:none; padding:2px 8px; cursor:pointer;">FORCE SCAN</button>
          </div>
                    <div id="tc-status" style="color:#fa0; font-size:10px; margin-bottom:3px;">⏳ Connecting...</div>
                    <div id="tc-counts" style="color:#aaa; font-size:10px; margin-bottom:5px;">Čekám...</div>
                    <div id="tc-sent" style="color:#888; font-size:10px; margin-bottom:5px;">Sent: 0 | Errors: 0</div>
          <pre id="tc-output" style="margin:0; white-space: pre-wrap; word-wrap: break-word;"></pre>
        `;
        document.body.appendChild(panel);

                document.getElementById('tc-run').addEventListener('click', () => doScan(true));
    }

        function updatePanel(statusText, countsText) {
                const statusEl = document.getElementById('tc-status');
                const countsEl = document.getElementById('tc-counts');
                const sentEl = document.getElementById('tc-sent');
                if (statusEl) statusEl.textContent = statusText;
                if (countsEl) countsEl.textContent = countsText;
                if (sentEl) sentEl.textContent = `Sent: ${sentCount} | Errors: ${errorCount}`;
        }

    // Pomocná funkce: Extrakce textu z prvku, ignoruje potomky skóre/kurzů
    function getCleanText(el) {
        if (!el) return '';
        return el.innerText.trim();
    }

    // Pomocná funkce: Hledání sportu z nadřazené hlavičky
    function detectSportFromHeader(rowElement) {
        let current = rowElement;
        // Jdeme nahoru a hledáme hlavičkový div (např. "CCT Europe, Counter Strike")
        for (let i = 0; i < 15; i++) {
            if (!current || current === document.body) break;
            
            // Siblings (bráškové) před tímto prvkem mohou být hlavičky
            let prev = current.previousElementSibling;
            for(let j=0; j<5; j++) {
                if(!prev) break;
                const text = prev.innerText || '';
                if (text.length > 2 && text.length < 150) {
                    const lower = text.toLowerCase();
                    // Pokud obsahuje známý sport, je to přímo hlavička události
                    if (lower.includes('fotbal') || lower.includes('tenis') || lower.includes('basket') || lower.includes('nba') || lower.includes('volejbal') || ALLOWED_ESPORTS.some(e => lower.includes(e))) {
                        return lower;
                    }
                }
                prev = prev.previousElementSibling;
            }
            current = current.parentElement;
        }
        return "neznámý";
    }

    // Hlavní parsovací logika
    function parseMatch(row) {
        try {
            // NÁM JDE JEN O A TAGY
            if (row.tagName !== 'A') return null;
            
            const rawText = getCleanText(row);
            if (!rawText) return null;

            const href = row.getAttribute('href') || '';
            if (!href.includes('/live/zapas/')) return null;

            // 1. Zjistit sport (z hlavičky)
            let sportHeader = detectSportFromHeader(row);
            let sport = "neznámý";
            let isAllowedSport = false;
            
            if (sportHeader.includes('fotbal')) { sport = 'fotbal'; isAllowedSport = true; }
            else if (sportHeader.includes('tenis')) { sport = 'tenis'; isAllowedSport = true; }
            else if (sportHeader.includes('basket') || sportHeader.includes('nba')) { sport = 'basketbal'; isAllowedSport = true; }
            else if (sportHeader.includes('volejbal')) { sport = 'volejbal'; isAllowedSport = true; }
            else {
                const isEsportType = ALLOWED_ESPORTS.some(e => sportHeader.includes(e));
                const isBanned = BANNED_ESPORTS.some(e => sportHeader.includes(e));
                if (isEsportType && !isBanned) { sport = 'esport'; isAllowedSport = true; }
            }

            if (!isAllowedSport) sport = 'SKIP_' + sportHeader.substring(0,20);

            // Řádky pole - tohle je MNOHEM SPOLHELIVĚJŠÍ než DOM třídy
            // Protože Chance dává každou informaci na nový řádek vizuálně
            const lines = rawText.split('\n').map(l => l.trim()).filter(l => l.length > 0);
            
            let team1 = "Neznámý";
            let team2 = "Neznámý";
            let mainScoreStr = "0:0";
            let detailedScore = "";
            let esportRounds = null;
            let hasOdds = true;
            let ods = { "1": null, "0": null, "2": null };
            let timeInfo = "";

            // 1. Týmy (Vždy první řádek s ' - ')
            if (lines.length > 0 && lines[0].includes(' - ')) {
                const teamParts = lines[0].split(' - ');
                team1 = teamParts[0].trim();
                team2 = teamParts.slice(1).join(' - ').trim();
            }

            // 2. Projdeme zbytek řádků
            for (let i = 1; i < lines.length; i++) {
                const line = lines[i];

                // 1. Skóre s případnými doplňky v závorce (např. "0:1 (6:18)" nebo "1:0")
                const scoreMatch = line.match(/^(\d{1,3}:\d{1,3})(?:\s*\((.*?)\))?/);
                if (scoreMatch) {
                    mainScoreStr = scoreMatch[1];
                    if (scoreMatch[2]) {
                        detailedScore = line;
                        if (sport === 'esport') {
                           // Extrakce kol pro CS2 (např. "6:18")
                           esportRounds = scoreMatch[2].trim();
                        }
                    }
                }
                // 2. Detailní skóre (fáze zápasu jako "Lepší z", "mapa", "set")
                else if (line.includes('mapa') || line.includes('set') || line.includes('pol.') || line.includes('.tř.') || line.includes('Lepší z') || line.includes('přestávka') || (line.includes('(') && line.includes(')'))) {
                    detailedScore = line;
                }
                // Časový údaj (Za X minut, Za okamžik)
                else if (line.startsWith('Za ')) {
                    timeInfo = line;
                }
                // Info o nepřítomnosti kurzů
                else if (line.includes('Kurzy nejsou') || line.includes('Událost skončila')) {
                    hasOdds = false;
                }
            }

            // 3. Parsování kurzů
            // Pokud nemají kurzy, vůbec to nebudeme analyzovat na kurzy
            if (hasOdds) {
                // Iterujeme pole pozpátku/dopředu pro čísla kurzů: [..., "1", "1.85", "2", "2.15"]
                for (let i = 0; i < lines.length - 1; i++) {
                    const label = lines[i];
                    // Label musí být "1", "0", nebo "2"
                    if (label === "1" || label === "0" || label === "2" || label === "10" || label === "02" || label === "12") {
                        if (ods[label] === undefined) continue; // zajímají nás jen hlavní 1, 0, 2
                        
                        const rawVal = (lines[i + 1] || '').trim();
                        // Odds jsou prakticky vždy desetinná čísla (např. 1.85 / 1,85).
                        // Tohle chrání proti falešným pozitivům (např. tenis body 0/15/30/40).
                        if (!(rawVal.includes('.') || rawVal.includes(','))) continue;

                        const valStr = rawVal.replace(',', '.');
                        if (!/^\d+\.\d+$/.test(valStr)) continue;

                        const val = parseFloat(valStr);
                        // Kurz musí být logické číslo > 1
                        if (!isNaN(val) && val > 1.0) {
                            ods[label] = val;
                        }
                    }
                }
            }

            // Fallback (některé prázdné kurzy píšou visací kurzy_dostupne)
            if (ods["1"] === null && ods["2"] === null && ods["0"] === null) {
                hasOdds = false;
            }

            return {
                match: team1 + " vs " + team2,
                team1: team1,
                team2: team2,
                sport_kategorie: sportHeader.replace('\n', ' '),
                sport_typ: sport,
                stav_cas: timeInfo,
                score_hlavni: mainScoreStr,
                score_detailni: detailedScore,
                esport_kola: esportRounds,
                kurzy_dostupne: hasOdds,
                kurz_1: ods["1"],
                kurz_x: ods["0"],
                kurz_2: ods["2"],
            };

        } catch (e) {
            console.error("TestChance Error on row:", e);
            return null;
        }
    }

    // ====================================================================
    // NORMALIZACE DO FEED-HUB SCHÉMAT
    // ====================================================================
    function normalizeSport(parsed) {
        const sportTyp = (parsed.sport_typ || '').toLowerCase();
        const header = (parsed.sport_kategorie || '').toLowerCase();
        if (sportTyp === 'fotbal') return 'football';
        if (sportTyp === 'tenis') return 'tennis';
        if (sportTyp === 'basketbal') return 'basketball';
        if (sportTyp === 'volejbal') return 'volleyball';

        if (sportTyp === 'esport') {
            if (header.includes('counter strike') || header.includes('counter-strike') || header.includes('cs2') || header.includes('cs:go') || header.includes('csgo')) return 'cs2';
            if (header.includes('dota')) return 'dota-2';
            if (header.includes('league of legends') || header.includes('lol')) return 'league-of-legends';
            if (header.includes('valorant')) return 'valorant';
            return 'esports';
        }
        return null;
    }

    function parseMainScore(scoreStr) {
        const m = String(scoreStr || '').match(/^(\d{1,3}):(\d{1,3})$/);
        if (!m) return { score1: 0, score2: 0 };
        return { score1: parseInt(m[1], 10) || 0, score2: parseInt(m[2], 10) || 0 };
    }

    function buildKey(sport, team1, team2) {
        return `${sport}||${team1}||${team2}`;
    }

    function shouldSend(kind, key, fingerprint) {
        const k = `${kind}||${key}`;
        const now = Date.now();
        const prev = lastSendStateByKey.get(k);
        if (!prev) {
            lastSendStateByKey.set(k, { fp: fingerprint, sentAt: now });
            return true;
        }

        const fpChanged = prev.fp !== fingerprint;
        const tooOld = (now - (prev.sentAt || 0)) >= RESEND_UNCHANGED_MS;

        if (fpChanged || tooOld) {
            lastSendStateByKey.set(k, { fp: fingerprint, sentAt: now });
            return true;
        }

        return false;
    }

    function buildLiveEnvelope(parsed, sport) {
        const { score1, score2 } = parseMainScore(parsed.score_hlavni);
        const status = parsed.stav_cas && parsed.stav_cas.trim().length > 0 ? parsed.stav_cas.trim() : 'Live';
        return {
            v: 1,
            type: 'live_match',
            source: SOURCE_NAME,
            ts: new Date().toISOString(),
            payload: {
                sport,
                team1: parsed.team1,
                team2: parsed.team2,
                score1,
                score2,
                detailed_score: parsed.score_detailni || '',
                status,
                url: window.location.href,
            },
        };
    }

    function buildOddsEnvelope(parsed, sport) {
        // Bezpečnost: feed-hub má dnes 2-way market; 1X2 (draw) přeskočíme.
        if (parsed.kurz_x !== null && parsed.kurz_x !== undefined) return null;
        if (!parsed.kurzy_dostupne) return null;
        if (parsed.kurz_1 === null || parsed.kurz_2 === null) return null;

        return {
            v: 1,
            type: 'odds',
            source: SOURCE_NAME,
            ts: new Date().toISOString(),
            payload: {
                sport,
                bookmaker: BOOKMAKER,
                market: 'match_winner',
                team1: parsed.team1,
                team2: parsed.team2,
                odds_team1: parsed.kurz_1,
                odds_team2: parsed.kurz_2,
                url: window.location.href,
            },
        };
    }

    // ====================================================================
    // WEBSOCKET
    // ====================================================================
    function connectWS() {
        if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;

        log('Connecting to', WS_URL);
        ws = new WebSocket(WS_URL);

        ws.onopen = () => {
            connected = true;
            updatePanel('✅ Connected', 'Scanning...');
            startScanning();
            startHeartbeat();
        };

        ws.onclose = (e) => {
            connected = false;
            updatePanel('❌ Disconnected — reconnecting...', 'Reconnecting...');
            stopScanning();
            stopHeartbeat();
            setTimeout(connectWS, RECONNECT_MS);
            dbg('WS close', e.code);
        };

        ws.onerror = () => {
            errorCount++;
            updatePanel('⚠️ WS error', '');
        };

        ws.onmessage = (e) => {
            dbg('Server:', e.data);
        };
    }

    function sendJSON(obj) {
        if (!ws || ws.readyState !== WebSocket.OPEN) return false;
        try {
            ws.send(JSON.stringify(obj));
            sentCount++;
            return true;
        } catch (_) {
            errorCount++;
            return false;
        }
    }

    function startHeartbeat() {
        stopHeartbeat();
        hbTimer = setInterval(() => {
            if (!connected) return;
            sendJSON({ v: 1, type: 'heartbeat', source: SOURCE_NAME, ts: new Date().toISOString(), payload: {} });
            updatePanel('✅ Connected', `Scanning... (links: ${document.querySelectorAll('a[href*="/live/zapas/"]').length})`);
        }, HEARTBEAT_MS);
    }

    function stopHeartbeat() {
        if (hbTimer) {
            clearInterval(hbTimer);
            hbTimer = null;
        }
    }

    // ====================================================================
    // SCAN LOOP
    // ====================================================================
    function startScanning() {
        stopScanning();
        scanTimer = setInterval(() => doScan(false), SCAN_INTERVAL_MS);
        doScan(false);
    }

    function stopScanning() {
        if (scanTimer) {
            clearInterval(scanTimer);
            scanTimer = null;
        }
    }

    function doScan(forceDebug) {
        const outputEl = document.getElementById('tc-output');

        const links = document.querySelectorAll('a[href*="/live/zapas/"]');
        const totalLinks = links.length;

        const unique = new Map();
        let parsedCount = 0;
        let validCount = 0;

        links.forEach(link => {
            const parsed = parseMatch(link);
            if (!parsed) return;
            parsedCount++;
            if (parsed.sport_typ && parsed.sport_typ.startsWith('SKIP_')) return;

            const sport = normalizeSport(parsed);
            if (!sport) return;
            if (sport !== 'football' && sport !== 'tennis' && sport !== 'basketball' && sport !== 'cs2' && sport !== 'dota-2' && sport !== 'league-of-legends' && sport !== 'valorant' && sport !== 'esports') {
                return;
            }

            // Odfiltrování obvious „virtuál/efootball“ (typicky: Team (nickname))
            if (sport === 'football' && (/(\(.+\))/.test(parsed.team1) || /(\(.+\))/.test(parsed.team2))) {
                return;
            }

            const key = buildKey(sport, parsed.team1, parsed.team2);
            const existing = unique.get(key);
            if (!existing) {
                unique.set(key, { parsed, sport });
            } else {
                // prefer entry with odds
                if ((!existing.parsed.kurzy_dostupne) && parsed.kurzy_dostupne) {
                    unique.set(key, { parsed, sport });
                }
            }
        });

        const results = [];
        let sentNow = 0;
        let skipped1x2 = 0;
        let oddsBuilt = 0;

        for (const { parsed, sport } of unique.values()) {
            validCount++;
            results.push(parsed);

            const key = buildKey(sport, parsed.team1, parsed.team2);

            // live_match
            const liveFp = `${parsed.score_hlavni}||${parsed.score_detailni || ''}||${parsed.stav_cas || ''}`;
            if (shouldSend('live', key, liveFp)) {
                const env = buildLiveEnvelope(parsed, sport);
                if (sendJSON(env)) sentNow++;
            }

            // odds (2-way only)
            if (parsed.kurz_x !== null && parsed.kurz_x !== undefined) {
                skipped1x2++;
            }
            const oddsEnv = buildOddsEnvelope(parsed, sport);
            if (oddsEnv) {
                oddsBuilt++;
                const oddsFp = `${parsed.kurz_1 || ''}||${parsed.kurz_2 || ''}`;
                if (shouldSend('odds', key, oddsFp)) {
                    if (sendJSON(oddsEnv)) sentNow++;
                }
            }
        }

        lastResults = results;

        updatePanel(
            connected ? '✅ Connected' : '⏳ Connecting...',
            `Links: ${totalLinks} | Parsed: ${parsedCount} | Matches: ${validCount} | Odds(2-way): ${oddsBuilt} | Skip 1X2: ${skipped1x2} | SentΔ: ${sentNow}`
        );

        if (forceDebug && outputEl) {
            outputEl.innerText = JSON.stringify(lastResults, null, 2);
        }
    }

    // Spuštění po načtení
    setTimeout(() => {
        createPanel();
        connectWS();
    }, 2000);
})();
