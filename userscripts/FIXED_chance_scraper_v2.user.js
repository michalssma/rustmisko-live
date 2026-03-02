// ==UserScript==
// @name         TESTCHANCE - Čistý JSON Parser
// @namespace    rustmisko
// @version      2.0
// @description  Cistý parser pro Chance.cz (Fotbal, Tenis, Basketbal, Volejbal, CS2, LoL, Dota, Valorant)
// @author       Vytvořeno pro Tebe
// @match        https://www.chance.cz/*
// @grant        none
// ==/UserScript==

(function() {
    'use strict';

    // Konfigurace sportů, o které máme zájem
    const ALLOWED_ESPORTS = ['counter strike', 'league of legends', 'dota', 'valorant', 'cs2', 'cs:go', 'csgo', 'lol'];
    const BANNED_ESPORTS = ['efootball', 'ebasketball', 'esports battle', 'etenis', 'ehokej']; // z předchozích pravidel
    
    // Panel pro zobrazení JSON struktury
    function createDebugPanel() {
        const panel = document.createElement("div");
        panel.id = "testchance-debug-panel";
        panel.style.cssText = `
          position: fixed; bottom: 10px; right: 10px; z-index: 999999;
          background: #111; color: #0f0; font-family: 'Consolas', monospace;
          font-size: 11px; padding: 10px; border-radius: 8px;
          border: 1px solid #0f0; width: 450px; max-height: 500px;
          overflow-y: auto; opacity: 0.95;
        `;
        panel.innerHTML = `
          <div style="display:flex; justify-content:space-between; margin-bottom: 5px; border-bottom: 1px solid #333; padding-bottom: 5px;">
            <b>TESTCHANCE JSON Parser v2.0</b>
            <button id="tc-run" style="background:#0f0; color:#000; border:none; padding:2px 8px; cursor:pointer;">PARSOVAT HNED</button>
          </div>
          <div id="tc-counts" style="color:#aaa; font-size:10px; margin-bottom:5px;">Čekám...</div>
          <pre id="tc-output" style="margin:0; white-space: pre-wrap; word-wrap: break-word;"></pre>
        `;
        document.body.appendChild(panel);

        document.getElementById('tc-run').addEventListener('click', runParser);
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
                // Pokud text vypadá jako hlavička (krátký, bez skóre, bez tlačítka play)
                if (text.length > 2 && text.length < 100 && !/\d:\d/.test(text) && !text.includes('1.8') && text.includes(',')) {
                    return text.toLowerCase();
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

                // Hlavní skóre (formát X:Y přesně, nebo jako 0:0 v stringu bez nového řádku)
                if (/^\d{1,3}:\d{1,3}$/.test(line)) {
                    mainScoreStr = line;
                }
                // Detailní skóre (obsahuje mapa, set, pol., třetina, Lepší)
                else if (line.includes('mapa') || line.includes('set') || line.includes('pol.') || line.includes('.tř.') || line.includes('Lepší z') || line.includes('přestávka') || (line.includes('(') && line.includes(')'))) {
                    detailedScore = line;
                    // Extrakce kol pro esport
                    if (sport === 'esport') {
                        const scorePartIndex = line.lastIndexOf('-');
                        if (scorePartIndex !== -1 && /[0-9:]/.test(line.substring(scorePartIndex))) {
                             esportRounds = line.substring(scorePartIndex + 1).trim();
                        }
                    }
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
                        
                        const valStr = lines[i+1].replace(',', '.');
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

    function runParser() {
        const outputEl = document.getElementById('tc-output');
        const countsEl = document.getElementById('tc-counts');
        outputEl.innerText = "Skenuji DOM Chance.cz...";
        
        // Najdi všechny a tagy, které by mohly být zápasy
        const links = document.querySelectorAll('a[href*="/live/zapas/"]');
        
        let results = [];
        let validSportsCount = 0;
        let totalCount = links.length;

        links.forEach(link => {
            const parsed = parseMatch(link);
            if (parsed && !parsed.sport_typ.startsWith('SKIP_')) {
                results.push(parsed);
                validSportsCount++;
            }
        });

        countsEl.innerText = `Nalezeno ${totalCount} zápasů, z toho ${validSportsCount} validních (fotbal/tenis/basket/esporty).`;
        outputEl.innerText = JSON.stringify(results, null, 2);
    }

    // Spuštění po načtení
    setTimeout(createDebugPanel, 2000);
})();
