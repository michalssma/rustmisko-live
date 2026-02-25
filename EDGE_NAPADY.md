# EDGE NÁPADY — RustMiskoLive Expansion Roadmap

> Datum: 2026-02-25 | Stav: v4.4.0 | Balance: $38.74 USDT
> Persona: Pasivní příjem $2,000/měsíc cíl
> VYLOUČENO: CS2 skiny — Miša to nechce dělat.

---

## PRIORITNÍ MAPA

| # | Edge | ROI/den odhad | Effort | Status |
|---|------|--------------|--------|--------|
| 1 | **Více bookmakerů (Bet365, Betano, Fortuna)** | $50-200 | Nízký | ⏳ TODO |
| 2 | **Fotbal/Hokej live score → Azuro** | $30-150 | Nízký | ⏳ TODO |
| 3 | **Betfair Exchange scraping** | $50-300 | Střední | ⏳ TODO |
| 4 | **Polymarket vs Azuro cross-market ARB** | $10-50 | Vysoký | ⏳ TODO |
| 5 | **Crypto Funding Rate Harvesting** | 15-25% p.a. | Střední | ⏳ TODO |
| 6 | **Twitter/X Sentiment → Crypto alerts** | manual | Nízký | ⏳ TODO |
| 7 | **ESPN / LiveSports.cz data** | rozšíření | Nízký | ⏳ TODO |

---

## EDGE #1 — Více Bookmakerů (NEJJEDNODUŠŠÍ, OKAMŽITĚ)

**Princip:**  
Čím více bookmakerů = více cenosvých divergencí vůči Azuru.
Momentálně máme Tipsport → ~8 kurzů. S 5+ booky → 50-100 divergencí/hodinu.

**Implementace:**  
- Napsat Tampermonkey scraper pro: Fortuna.cz, Bet365.com, Betano.cz, Unibet.cz, Pinnacle.com
- Stejná architektura jako `tipsport_odds_scraper.user.js`
- Každý nový bookmaker = nový `.user.js` soubor
- Feed hub je připraven — stačí přidat `bookmaker: "fortuna"` atd.

**ARB matematika:**  
$$\text{ARB edge} = \frac{1}{\text{Azuro odds}} + \frac{1}{\text{Fortuna odds}} < 1.0$$

Pokud součet < 1.0 → garantovaný risk-free profit.

**Soubory k vytvoření:**
- `userscripts/fortuna_odds_scraper.user.js`
- `userscripts/bet365_odds_scraper.user.js`
- `userscripts/betano_odds_scraper.user.js`

**Effort:** ~2-4 hodiny na scraper, scrapeovat URL strukturu cílového bookmakera first.

---

## EDGE #2 — Fotbal/Hokej/Tenis Live Score Edge (ROZŠÍŘENÍ EXISTUJÍCÍHO)

**Princip:**  
Stejná logika jako CS2 (score momentum), ale pro:
- **Fotbal:** Gól → kurzy zaostávají 10-60 sekund → sázej ihned
- **Hokej:** Přesilovka/oslabení → temporary edge
- **Tenis:** Break of serve → kurzy pomalé na update

**Aktuální stav:**  
`find_score_edges()` má modely jen pro CS2 (round/map score) a tenis (set score).
Fotbal a hokej jsou zatím SKIPPED.

**Implementace v alert_bot.rs:**  
- Přidat football model: gól → +1 = ~69% win prob, +2 = ~88%, +3 = ~96%
- Přidat hockey model: puck ahead +1 = ~65%, +2 = ~82% (hockey is closer)
- Přidat basketball model: +10pts = ~70%, +20pts = ~90%

```rust
"football" => {
    // Gól model — Dixon-Coles inspired
    let fair = match score_diff {
        1 => 69.0,  // leads by 1 goal
        2 => 86.0,  // leads by 2 goals  
        3 => 95.0,  // leads by 3 goals
        _ => 50.0 + (score_diff as f64 * 12.0).min(45.0),
    };
}
```

**Effort:** ~3-5 hodin. Modely jsou jednoduché.

---

## EDGE #3 — Betfair Exchange Scraping (PREMIUM LIQUIDITY)

**Princip:**  
Betfair Exchange = peer-to-peer sázkový trh, $500B+ roční obrat.
Betfair má NEJLEPŠÍ světové kurzy na sport (žádná marže bookmakera).

**Edge:**  
- Betfair kurzy ~5-10% lepší než tradiční bookmakerky
- Pokud Betfair nabízí 2.10 a Azuro 1.95 → sázej Azuro (očekávaná hodnota +7.7%)
- Betfair nemá API rate limity pro DOM scraping

**URL:** `betfair.com/exchange/plus/football/event/{id}`

**Implementace:**  
- `userscripts/betfair_exchange_scraper.user.js`
- Scrapeovat "Back" odds pro každý trh
- WebSocket do Feed Hub jako `bookmaker: "betfair_exchange"`

**Effort:** ~4-6 hodin (Betfair DOM je komplexní)

---

## EDGE #4 — Polymarket vs Azuro Cross-Market ARB

**Princip:**  
Polymarket (prediction market) vs Azuro = stejné eventy, různé líkvidní zdroje.
Polymarket cena 0.70 = implikuje 70% pravděpodobnost.
Azuro kurz 1.40 = implikuje 71.4% pravděpodobnost.
Divergence → hedguj oboje strany → risk-free.

**Polymarket scraping URL:** `polymarket.com/event/{slug}`

**Implementace:**
- `userscripts/polymarket_scraper.user.js` — scrape event prices
- Feed Hub: přidat `bookmaker: "polymarket"` support
- alert_bot: přidat polymarket vs azuro fúze logiku
- Polymarket používá USDC na Polygon — stejná síť jako Azuro!

**Effort:** ~1-2 týdny (Polymarket má React SSR, scraping je obtížnější)

---

## EDGE #5 — Crypto Funding Rate Harvesting (PASIVNÍ, 24/7)

**Princip:**  
Na Binance/Bybit perpetual futures se každých 8h platí "funding rate".
Pokud funding > 0.05% (= >18% ročně) → delta-neutral harvest:

$$\text{Setup} = \text{SHORT futures} + \text{LONG spot} = \text{delta-neutral}$$
$$\text{Výdělek} = 3 \times \text{funding} / \text{den} = 18-25\%/\text{rok risk-free}$$

**Implementace:**
- Tampermonkey na `binance.com/en/futures` scrape funding rates tabulku
- Alert pokud funding > 0.05% (= výhodné spustit harvest)
- Žádné on-chain transakce — jen Binance/Bybit účet

**Scraper URL:** `binance.com/en/futures/funding-history`

**Effort:** ~3-5 hodin scraper + manuální execution na začátku

---

## EDGE #6 — Twitter/X Sentiment Alerts → Crypto

**Princip:**  
Klíčová slova na Twitter/X předcházejí pohybům cen krypta:
- CZ Binance tweet → BNB pump
- Coinbase Listing announcement → coin pumps 20-50% do 5 minut
- Elon Musk zmínka → DOGE/crypto pohyb

**Implementace:**
- Tampermonkey na `twitter.com` (nebo `x.com`)
- Sledovat specific accounts: @cz_binance, @coinbase, @elonmusk, @brian_armstrong
- Pattern match na klíčová slova: "listing", "partnership", "pump"
- Alert → Telegram zpráva okamžitě → manuální react

**Effort:** ~2-3 hodiny (Twitter DOM je stabilní)

**POZOR:** Jen alerting, ne auto-execution. Sentiment trading je risky.

---

## EDGE #7 — ESPN / LiveSports.cz / SofaScore Jako Další Live Data

**Princip:**  
Více src pro live score = rychlejší detekce gólů/eventů = větší edge okno.

**Cíle:**
- `espn.com/soccer/scoreboard` — americký futbol, NBA, NFL
- `livesports.cz` — rychlé české výsledky  
- `sofascore.com` — komplexní live stats (shots, possession, etc.)
- `whoscored.com` — detailní fotbal statistiky per event

**Výhoda SofaScore:** Má events timeline (přesný čas gólu v sekundách), nejen skóre.

**Implementace:**
- Každý = 1 Tampermonkey skript vysílající do Feed Hub
- Feed Hub de-dupuje stejné zápasy (normalizace jmen)
- Rychleji potvrzený gól/event = dřívější sázka = větší edge

**Effort:** ~2-4 hodiny na scripty, 1-2 hodiny na de-dup logiku v feed_hub

---

## IMPLEMENTAČNÍ PLÁN (Fáze)

### Fáze 1 — IHNED (toto týden)
1. ✅ **CS2 sport matching fix** (esports→cs2 v feed_hub) — HOTOVO v4.4.0
2. ⏳ **Fortuna.cz scraper** — největší ROI/effort ratio
3. ⏳ **Football score model** v alert_bot (gól → edge)

### Fáze 2 — Tento měsíc
4. ⏳ **Betfair Exchange scraper**
5. ⏳ **Betano.cz / Bet365 scrapers**
6. ⏳ **Hockey + Basketball score models**

### Fáze 3 — Za 2-4 týdny  
7. ⏳ **SofaScore live stats scraper**
8. ⏳ **Funding rate harvesting** (Binance)
9. ⏳ **Polymarket integration**

### Fáze 4 — Experimentální
10. ⏳ **Twitter/X sentiment alerts**
11. ⏳ **ESPN multi-sport data**

---

## TECHNICKÁ ARCHITEKTURA (pro všechny edge)

```
Chrome Tab (Tampermonkey) ─── WebSocket ──→ Feed Hub :8080
                                                    │
                                               fuse_all()
                                                    │
                                           alert_bot polls /state
                                                    │
                                      find_score_edges() + find_odds_anomalies()
                                                    │
                                           edge detected? YES
                                                    │
                                    HIGH confidence? → POST /bet → Azuro on-chain
                                    MEDIUM confidence? → Telegram alert → manual
```

**Každý nový edge = nový Tampermonkey skript + případný model v alert_bot.rs**

---

*Poslední update: 2026-02-25 | Git: v4.4.0*
